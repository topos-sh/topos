import { candidateStoredBytes, storageCapRefusalForIngest } from "@/lib/api/storage-quota.server";
import { type BundleKind, kindEntry } from "@/lib/bundle-base";
import { mintBundleId } from "@/lib/db/identity.server";
import {
  bundleCapRefusal,
  bundleCapRefusalInTx,
  type FinalTx,
  type GenesisDestination,
  type GenesisRegistration,
  inFinalTxOrRefusal,
  NO_CHANNEL,
  type PublishActor,
  registerGenesisBundleInTx,
} from "@/lib/db/queries.custody.server";
import type { McpGateRefusal } from "@/lib/mcp/publish-gate.server";
import { publishVersion } from "@/lib/plane/custody.server";
import type { LaneFile } from "@/lib/plane/wire";

/**
 * PUBLISHING A BRAND-NEW BUNDLE — the sequence, written once.
 *
 * Two doors put a bundle into a workspace for the first time: the session lane's publish (an
 * agent's `topos publish`) and add-from-GitHub. They differ in where the bytes came from and what
 * each records ALONGSIDE the new bundle — upstream provenance, an import audit line, an op
 * receipt — and in nothing else. What they must NOT differ in is the bundle they produce: the
 * same id, the same minted catalog name, the same birth kind, the same placement rules, the same
 * audit. This is that shared middle:
 *
 *   the vault call → ONE transaction holding the registration and whatever the door adds → one
 *   typed outcome.
 *
 * The `alsoInTx` hook runs in THAT transaction, which is where a door's op receipt belongs: a
 * receipt written afterwards, in a second transaction, leaves a crash window in which the bundle
 * exists with no replay record — and the op's retry then stops being a replay and becomes a
 * second publish against a bundle that now exists, reported as a generation conflict.
 *
 * WHAT CANNOT COME THROUGH HERE AT ALL: a kind whose bundles are not files. An MCP server is a
 * row in the server catalog — the workspace connects to one, and the document lives there — so a
 * publish naming that kind is describing something this door cannot make, and is refused before
 * anything is ingested rather than turned into a bundle of bytes nobody will ever be served.
 *
 * WHERE A NEW BUNDLE REACHES is a property of the DOOR: the session lane carries the wire's
 * semantics (an absent channel means the workspace default, which is what makes a published
 * bundle arrive on the team's machines), while a web creation page carries whatever its own form
 * rests on. So `destination` is a required argument, never a default read from a record here.
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
  /** A cap, a quota, or the kind itself said no — nothing was registered. */
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

/**
 * A KIND THAT IS NOT FILES refuses bytes, in the same words at every door. The sentence names
 * where the act really belongs, because the person meeting it wanted a server and is not helped
 * by being told what a publish is not.
 */
export function noFilesRefusal(kind: BundleKind): McpGateRefusal {
  const record = kindEntry(kind);
  return {
    code: "KIND_HAS_NO_FILES",
    message: `${record.sectionLabel} are catalog entries, not bundles of files — add one on the ${record.sectionLabel} page`,
  };
}

export async function publishGenesisBundle<T = undefined>(
  args: GenesisPublishArgs<T>,
): Promise<GenesisPublishOutcome<T>> {
  const record = kindEntry(args.kind);
  const bundleId = args.bundleId ?? mintBundleId();
  const ws = args.actor.workspaceId;

  // THE KIND, before anything else: a bundle whose document lives in the server catalog has no
  // bytes to publish, so this refuses ahead of every cap, quota and custody call.
  if (!record.isFileBundle) {
    return { kind: "refused", refusal: noFilesRefusal(args.kind) };
  }

  // THE BUNDLE CAP (`bundles`), before any custody call — a NEW identity at the cap must not
  // ingest bytes it will never register (a no-op without a limit; the in-transaction check
  // below stays the race-fenced authority). New versions of existing bundles never run this
  // path at all.
  const capped = await bundleCapRefusal(args.actor, bundleId);
  if (capped !== null) {
    return { kind: "refused", refusal: capped };
  }

  // THE STORAGE QUOTA (`storage-bytes`), HERE so every genesis door meets it — the web
  // creation pages call this path directly, never the lane routes' own check — and before any
  // custody call, so a refusal leaves no ingested bytes (a no-op without a limit).
  const overQuota = await storageCapRefusalForIngest(ws, candidateStoredBytes(args.candidate));
  if (overQuota !== null) {
    return { kind: "refused", refusal: overQuota };
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
      // The bundle cap's AUTHORITY — re-checked in this transaction under the advisory lock,
      // so two concurrent geneses at the boundary serialize (the pre-vault read above only
      // keeps the common refusal byte-free). A refusal rolls the whole registration back.
      const stillCapped = await bundleCapRefusalInTx(tx, args.actor, bundleId);
      if (stillCapped !== null) {
        refuse(stillCapped);
      }
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
      const extra = (await args.alsoInTx?.(tx, landing)) as T;
      return { landing, extra };
    },
  );
  if (landed.refused !== null) {
    return { kind: "refused", refusal: landed.refused };
  }
  return { kind: "ok", ...landed.value.landing, extra: landed.value.extra };
}
