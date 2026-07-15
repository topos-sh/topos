/**
 * The FIRST thing a reviewer must know: where this proposal stands. Keyed to the page state
 * machine (lib/review/state.ts) — the stored resolution, with an approval refined against the
 * live current pointer. `unknown` covers a stored status this build doesn't recognize — stated
 * plainly, never dressed up as one of the real states.
 */
export type ReviewStatus =
  | "pending"
  | "accepted-live"
  | "superseded"
  | "rejected"
  | "closed"
  | "unknown";

// Zinc only — semantic green/amber/gray stay reserved for domain verification, so the one
// trust signal in the product never competes with a status banner.
const STYLES: Record<ReviewStatus, string> = {
  pending: "border-line-soft bg-ground text-dim",
  "accepted-live": "border-line-soft bg-ground text-dim",
  superseded: "border-line-soft bg-ground text-dim",
  rejected: "border-line-soft bg-ground text-dim",
  closed: "border-line-soft bg-ground text-dim",
  unknown: "border-line-soft bg-ground text-dim",
};

const COPY: Record<ReviewStatus, string> = {
  pending:
    "Open — awaiting a reviewer's decision. The diff below compares against today's current.",
  "accepted-live": "Accepted — this candidate is the team's current version.",
  superseded:
    "Accepted earlier — current has since moved on. The diff below compares against today's current.",
  rejected: "Rejected — the resolution below says why.",
  closed:
    "Closed without a decision — withdrawn by its proposer, or retired with its skill. The reason below says which.",
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
