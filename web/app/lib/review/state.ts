import type { GenerationLike } from "@/lib/diff/staleness";

/**
 * The proposal page's ONE state derivation — pure, over the detail read + the live current
 * pointer (two reads the page already holds; nothing here fetches). The detail's `status` is
 * the STORED resolution; staleness stays a derived view, computed the same way the server's
 * own listing predicate works (`open AND base == current`). Generations feed only the equality
 * comparison — nothing downstream renders an epoch or a seq.
 */

export type ProposalPageState =
  /** Open on the live current base — the decidable state. */
  | "pending"
  /** Open, but current moved off the proposal's base — undecidable as-is (a fresh propose is the path). */
  | "stale"
  /** Accepted, and the candidate IS the live current version. */
  | "accepted-live"
  /** Accepted earlier — current has since moved on. */
  | "superseded"
  | "rejected"
  /** The detail or the pointer couldn't anchor a real state — stated plainly, never dressed up. */
  | "unknown";

export interface ProposalDetailLike {
  /** The candidate version id (hex64). */
  version_id: string;
  /** The stored status: "open" | "accepted" | "rejected" (anything else folds to unknown). */
  status: string;
  base_generation: GenerationLike;
}

export interface LiveCurrentLike {
  /** The live current version id (hex64). */
  versionId: string;
  generation: GenerationLike;
}

export function deriveProposalPageState(
  detail: ProposalDetailLike,
  live: LiveCurrentLike | undefined,
): ProposalPageState {
  if (live === undefined) {
    return "unknown";
  }
  if (detail.status === "open") {
    const base = detail.base_generation;
    return base.epoch === live.generation.epoch && base.seq === live.generation.seq
      ? "pending"
      : "stale";
  }
  if (detail.status === "accepted") {
    return live.versionId === detail.version_id ? "accepted-live" : "superseded";
  }
  if (detail.status === "rejected") {
    return "rejected";
  }
  return "unknown";
}

/**
 * Whether the file diff can render, derived from the CANDIDATE's version-meta read. The server
 * retains a candidate's bytes only while the version is trunk-reachable or an OPEN proposal on
 * the live base — so a rejected or staled candidate's meta 404 is `reclaimed`: that retention
 * rule doing its job, not a page error. The page keeps the proposal's RECORD (banner,
 * resolution, comments) and renders an honest diff-less card in place of the files. Any other
 * failure is `unreadable` — transient, worth a reload.
 */
export type DiffAvailability = "full" | "reclaimed" | "unreadable";

/** The shape both PlaneResult arms share — `kind` exists only on a failure. */
export interface VersionMetaReadLike {
  ok: boolean;
  kind?: string;
}

export function deriveDiffAvailability(read: VersionMetaReadLike): DiffAvailability {
  if (read.ok) {
    return "full";
  }
  return read.kind === "not_found" ? "reclaimed" : "unreadable";
}
