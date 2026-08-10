import { type BundleKind, kindEntry } from "@/lib/bundle-base";
import { claimBundleIdentityInTx } from "@/lib/db/bundle-identity.server";
import { mintBundleId } from "@/lib/db/identity.server";
import {
  type FinalTx,
  type GenesisDestination,
  type GenesisRegistration,
  inFinalTxOrRefusal,
  NO_CHANNEL,
  type PublishActor,
  registerGenesisBundleInTx,
} from "@/lib/db/queries.custody.server";
import {
  type McpGateRefusal,
  mcpCandidateRefusal,
  mcpNameTakenRefusal,
} from "@/lib/mcp/publish-gate.server";
import { publishVersion } from "@/lib/plane/custody.server";
import type { LaneFile } from "@/lib/plane/wire";

/**
 * PUBLISHING A BRAND-NEW BUNDLE — the sequence, written once.
 *
 * Three doors put a bundle into a workspace for the first time: the session lane's publish (an
 * agent's `topos publish`), add-from-GitHub, and add-an-MCP-server. They differ in where the
 * bytes came from and what each records ALONGSIDE the new bundle — upstream provenance, an
 * import audit line, an op receipt — and in nothing else. What they must NOT differ in is the
 * bundle they produce: the same id, the same minted catalog name, the same birth kind, the same
 * placement rules, the same identity claim, the same audit. This is that shared middle:
 *
 *   the kind's own candidate gate → the vault call → ONE transaction holding the registration,
 *   the identity claim, and whatever the door adds → one typed outcome.
 *
 * TWO THINGS COME FROM THE KIND'S RECORD rather than from the caller, because a call site is
 * exactly the wrong place to decide either. `hasContentGate` picks the gate a candidate's BYTES
 * must pass before any custody call (an MCP server's `server.json` rules; a skill has none), and
 * `defaultGenesisDestination` decides where a new bundle REACHES when the caller names nowhere —
 * publishing a skill hands it to the team, while importing a server does not hand it to anyone.
 *
 * ORDER INSIDE THE TRANSACTION IS LOAD-BEARING: the registration comes first because the
 * identity claim's foreign key points at the bundle row, and a refused claim rolls the whole
 * thing back — so "refused" keeps meaning that no catalog row was written.
 */

/** The bytes a genesis publish carries, in the shape the custody lane takes them. */
export interface GenesisCandidate {
  files: LaneFile[];
  /** The commit frame's author — the WIRE's on the session lane (the client pre-derived the
   *  version id from it), the actor's display on a web door. */
  attribution: string;
  message: string;
  /** A parent commit, when the door has one. Genesis normally has none. */
  parent?: string;
}

export interface GenesisPublishArgs<T> {
  actor: PublishActor;
  /** What this bundle IS, recorded once at birth. */
  kind: BundleKind;
  /**
   * The id to register under. The session lane supplies the CLIENT's own (the author's install
   * keys every later read and publish CAS on the id it minted at `add`); the web doors mint one
   * here.
   */
  bundleId?: string;
  candidate: GenesisCandidate;
  displayName: string | null;
  /** Where it reaches. Omit for this kind's own default. */
  destination?: GenesisDestination;
  /**
   * The rows this DOOR adds, inside the same transaction as the registration — upstream
   * provenance, an import audit line, an op receipt. Whatever it returns rides the outcome.
   */
  alsoInTx?: (tx: FinalTx, landed: GenesisLanding) => Promise<T>;
}

/** What the registration produced, for the door's own rows to reference. */
export interface GenesisLanding {
  bundleId: string;
  /** The minted catalog name (suffix-on-collision). */
  name: string;
  versionId: string;
  bundleDigest: string;
  placement: GenesisRegistration["placement"];
}

export type GenesisPublishOutcome<T> =
  | ({ kind: "ok"; generation: number; extra: T } & GenesisLanding)
  /** The kind's gate, or the identity claim, said no — nothing was registered. */
  | { kind: "refused"; refusal: McpGateRefusal }
  /** The vault refused the candidate itself (a malformed tree). */
  | { kind: "rejected"; message: string | null }
  | { kind: "fault" };

/** A kind's default destination, resolved from its record's tag to the value the DAL takes. */
function defaultDestination(kind: BundleKind): GenesisDestination {
  return kindEntry(kind).defaultGenesisDestination === "no-channel" ? NO_CHANNEL : null;
}

export async function publishGenesisBundle<T = undefined>(
  args: GenesisPublishArgs<T>,
): Promise<GenesisPublishOutcome<T>> {
  const record = kindEntry(args.kind);
  const bundleId = args.bundleId ?? mintBundleId();
  const ws = args.actor.workspaceId;

  // THE KIND'S GATE, before any custody call — a refused candidate must leave no ingested bytes
  // behind. A kind with no content gate passes straight through.
  let identity: string | null = null;
  if (record.hasContentGate) {
    const gate = await mcpCandidateRefusal(args.actor, args.candidate.files, bundleId);
    if (gate.refusal !== null) {
      return { kind: "refused", refusal: gate.refusal };
    }
    identity = gate.serverName;
  }

  const published = await publishVersion(ws, bundleId, {
    files: args.candidate.files,
    ...(args.candidate.parent === undefined ? {} : { parent: args.candidate.parent }),
    attribution: args.candidate.attribution,
    message: args.candidate.message,
  });
  if (published.kind === "rejected") {
    return { kind: "rejected", message: published.message ?? null };
  }
  if (published.kind !== "ok") {
    return { kind: "fault" };
  }

  const landed = await inFinalTxOrRefusal<{ landing: GenesisLanding; extra: T }, McpGateRefusal>(
    async (tx, refuse) => {
      const registration = await registerGenesisBundleInTx(
        tx,
        args.actor,
        bundleId,
        args.displayName,
        args.destination ?? defaultDestination(args.kind),
        args.kind,
      );
      const landing: GenesisLanding = {
        bundleId,
        name: registration.name,
        versionId: published.value.version_id,
        bundleDigest: published.value.bundle_digest,
        placement: registration.placement,
      };
      // The claim comes AFTER the row it points at, and a refusal rolls both back.
      if (identity !== null) {
        const claimed = await claimBundleIdentityInTx(tx, ws, bundleId, args.kind, identity);
        if (!claimed.ok && claimed.heldBy !== null) {
          refuse(mcpNameTakenRefusal(identity, claimed.heldBy));
        }
      }
      const extra = (await args.alsoInTx?.(tx, landing)) as T;
      return { landing, extra };
    },
  );
  if (landed.refused !== null) {
    return { kind: "refused", refusal: landed.refused };
  }
  return {
    kind: "ok",
    ...landed.value.landing,
    generation: published.value.pointer.generation,
    extra: landed.value.extra,
  };
}
