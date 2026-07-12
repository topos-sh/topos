/**
 * The FIRST thing a reviewer must know: where this proposal stands against the team's current
 * version. Keyed to the page state machine (lib/review/state.ts) — the STORED resolution plus
 * derived staleness. `unknown` covers a read that couldn't anchor a real state — stated plainly,
 * never dressed up as one of the real states.
 */
export type ReviewStatus =
  | "pending"
  | "stale"
  | "accepted-live"
  | "superseded"
  | "rejected"
  | "unknown";

// Zinc only — semantic green/amber/gray stay reserved for domain verification, so the one
// trust signal in the product never competes with a status banner.
const STYLES: Record<ReviewStatus, string> = {
  pending: "border-line-soft bg-ground text-dim",
  stale: "border-line bg-panel2 font-medium text-ink",
  "accepted-live": "border-line-soft bg-ground text-dim",
  superseded: "border-line-soft bg-ground text-dim",
  rejected: "border-line-soft bg-ground text-dim",
  unknown: "border-line-soft bg-ground text-dim",
};

const COPY: Record<ReviewStatus, string> = {
  pending: "Open — proposed against the team's current version.",
  stale:
    "current moved since this was proposed — it can no longer be approved as-is (a fresh propose is the path). The diff below compares against today's current.",
  "accepted-live": "Accepted — this candidate is the team's current version.",
  superseded:
    "Accepted earlier — current has since moved on. The diff below compares against today's current.",
  rejected: "Rejected — the resolution below says why.",
  unknown:
    "This proposal's status couldn't be confirmed. The change below is the immutable candidate version.",
};

export function ProposalStatusBanner({ status }: { status: ReviewStatus }) {
  return (
    <p role="status" className={`rounded-md border px-3 py-2 text-sm ${STYLES[status]}`}>
      {COPY[status]}
    </p>
  );
}
