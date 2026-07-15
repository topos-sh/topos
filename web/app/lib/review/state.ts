/**
 * The proposal page's ONE state derivation — pure, over the web proposal row's stored status
 * plus the live current pointer (two reads the page already holds; nothing here fetches). The
 * row's status is the STORED resolution ("open" | "approved" | "rejected" | "withdrawn"); the
 * live pointer only refines an approval into accepted-live vs superseded. There is no staleness
 * state: a proposal stays open until someone decides it, and the approve action's CAS binding
 * is what refuses a moved pointer — as a fresh outcome, not a page state.
 */

export type ProposalPageState =
  /** Open — the decidable state. */
  | "pending"
  /** Approved, and the candidate IS the live current version. */
  | "accepted-live"
  /** Approved earlier — current has since moved on. */
  | "superseded"
  | "rejected"
  /** Withdrawn — closed without a verdict (the author retracted it, or a lifecycle ceremony
   * auto-closed it). A real terminal state, not an anchoring failure. */
  | "closed"
  /** An unrecognized stored status — stated plainly, never dressed up. */
  | "unknown";

export function deriveProposalPageState(
  status: string,
  candidateVersionId: string,
  liveCurrentVersionId: string | null,
): ProposalPageState {
  if (status === "open") {
    return "pending";
  }
  if (status === "approved") {
    return liveCurrentVersionId === candidateVersionId ? "accepted-live" : "superseded";
  }
  if (status === "rejected") {
    return "rejected";
  }
  if (status === "withdrawn") {
    return "closed";
  }
  return "unknown";
}

/**
 * Whether the file diff can render, derived from the CANDIDATE's version-meta read. The vault
 * retains a candidate's bytes only while the version is trunk-reachable or an OPEN proposal —
 * so a rejected or withdrawn candidate's meta 404 is `reclaimed`: that retention rule doing its
 * job, not a page error. The page keeps the proposal's RECORD (banner, resolution, comments)
 * and renders an honest diff-less card in place of the files. Any other failure is
 * `unreadable` — transient, worth a reload.
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
