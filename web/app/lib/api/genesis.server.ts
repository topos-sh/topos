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
import { schedulePublishProbe } from "@/lib/mcp/probe.server";
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
 * The `alsoInTx` hook runs in THAT transaction, which is where a door's op receipt belongs: a
 * receipt written afterwards, in a second transaction, leaves a crash window in which the bundle
 * exists with no replay record — and the op's retry then stops being a replay and becomes a
 * second publish against a bundle that now exists, reported as a generation conflict.
 *
 * ONE THING COMES FROM THE KIND'S RECORD rather than from the caller: `hasContentGate` picks the
 * gate a candidate's BYTES must pass before any custody call (an MCP server's `server.json`
 * rules; a skill has none). That one is a property of the KIND — the same bytes are the same
 * bytes at every door.
 *
 * WHERE A NEW BUNDLE REACHES IS NOT. It is a property of the DOOR: the session lane carries the
 * wire's semantics for every kind (an absent channel means the workspace default, which is what
 * makes a published bundle arrive on the team's machines), while a web creation page carries
 * whatever its own form rests on. So `destination` is a required argument, never a default read
 * from a record here.
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
  /**
   * Where it reaches — REQUIRED, and each door says it outright. There is no shared default,
   * because there is no shared answer: the session lane carries the WIRE's semantics (a named
   * channel, or the workspace default when none is named), while a web creation page carries
   * whatever its form rests on. A default living here would silently apply one door's ruling to
   * the others, which is exactly how a publish stops reaching the team it used to reach.
   */
  destination: GenesisDestination;
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
  /** The pointer's generation after the publish — a receipt written in this transaction needs it. */
  generation: number;
  placement: GenesisRegistration["placement"];
}

export type GenesisPublishOutcome<T> =
  | ({ kind: "ok"; extra: T } & GenesisLanding)
  /** The kind's gate, or the identity claim, said no — nothing was registered. */
  | { kind: "refused"; refusal: McpGateRefusal }
  /** The vault refused the candidate itself (a malformed tree). */
  | { kind: "rejected"; message: string | null }
  /**
   * The bundle already holds a `current` this publish did not fence on. ROUTINE, not a fault:
   * the ids these doors publish under can be client-minted, so a retry after a refused first
   * attempt meets exactly this. Folding it into a fault would answer 500 forever on that id.
   */
  | { kind: "conflict"; generation: number | null }
  /** The vault does not hold the bundle this id names. */
  | { kind: "not_found" }
  | { kind: "fault" };

/**
 * What a WEB CREATION PAGE means by "no channel chosen", for its kind — the tag on that kind's
 * record resolved to the value the DAL takes. Only the web pages call this; the session lane
 * passes the wire's own channel through untouched.
 */
export function webNewDestination(kind: BundleKind, chosenChannel: string): GenesisDestination {
  if (chosenChannel.length > 0) {
    return chosenChannel;
  }
  return kindEntry(kind).webNewDefaultDestination === "no-channel" ? NO_CHANNEL : null;
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
  // The address the gate read out of the document, kept for the advisory probe below rather than
  // parsed a second time out of the vault.
  let endpoint: string | null = null;
  if (record.hasContentGate) {
    const gate = await mcpCandidateRefusal(args.actor, args.candidate.files, bundleId);
    if (gate.refusal !== null) {
      return { kind: "refused", refusal: gate.refusal };
    }
    identity = gate.serverName;
    endpoint = gate.endpoint;
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
  if (published.kind === "conflict") {
    return { kind: "conflict", generation: published.generation ?? null };
  }
  if (published.kind === "not_found") {
    return { kind: "not_found" };
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
        args.destination,
        args.kind,
      );
      const landing: GenesisLanding = {
        bundleId,
        name: registration.name,
        versionId: published.value.version_id,
        bundleDigest: published.value.bundle_digest,
        generation: published.value.pointer.generation,
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
  // THE ADVISORY PROBE, and only now: the bundle is registered, the pointer has moved, and this
  // call cannot change any of it. It asks the server the document names whether anything answers
  // there, records the answer beside the version, and is not waited on — a report about somebody
  // else's uptime must not lengthen the act that produced it, and its failure must not be visible
  // at all. All three genesis doors ride this one line.
  if (record.hasContentGate) {
    schedulePublishProbe(args.actor, {
      bundleId,
      versionId: landed.value.landing.versionId,
      endpoint,
    });
  }
  return { kind: "ok", ...landed.value.landing, extra: landed.value.extra };
}
