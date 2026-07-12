import { Card, SectionHeading } from "@/components/ui";
import { ApproveForm } from "./ApproveForm";
import { RejectDialog } from "./RejectDialog";

/**
 * The browser decision surface for a PENDING proposal, rendered for a viewer whose seat can
 * decide (owner|reviewer) — the server's in-transaction gate re-checks all of it. The page mints
 * one request id PER VERB (approve and reject occupy separate idempotency slots) and binds each
 * form to what the reviewer saw: approve to the rendered diff's current generation, reject to
 * the proposal's base.
 *
 * `withholdApprove` is the four-eyes case — the viewer proposed this candidate and
 * review-required is on. The gate applies to APPROVE only, so the panel keeps the reject flow
 * live as a WITHDRAW (the same reject write, reason still mandatory) and renders an inline
 * four-eyes line where Approve would be. Display-only either way: the server refuses a
 * self-approve regardless. `skill` is the catalog NAME; ws/skill/versionId ride the review
 * route's own params, and each form re-sends the version id as the source instructs.
 */
export function ReviewDecisionPanel({
  ws,
  skill,
  versionId,
  approveRequestId,
  rejectRequestId,
  expectedEpoch,
  expectedSeq,
  baseEpoch,
  baseSeq,
  withholdApprove = false,
}: {
  ws: string;
  skill: string;
  versionId: string;
  approveRequestId: string;
  rejectRequestId: string;
  /** The live current generation the diff above was computed against (the approve binding). */
  expectedEpoch: string;
  expectedSeq: string;
  /** The proposal's base generation (the reject binding — a reject moves no pointer). */
  baseEpoch: string;
  baseSeq: string;
  /** The viewer proposed this candidate under review-required — Approve is withheld. */
  withholdApprove?: boolean;
}) {
  return (
    <Card className="flex flex-col gap-4 p-4">
      <div className="flex flex-col gap-1">
        <SectionHeading>{withholdApprove ? "Your proposal" : "Decide"}</SectionHeading>
        <p className="text-sm text-dim">
          {withholdApprove
            ? "You proposed this candidate, and review-required is on for this workspace. Withdrawing records your reason under your workspace email, and the server re-checks your seat when it lands."
            : "Approving makes this candidate the team's current version — followers pick it up on their next pull. Either decision records under your workspace email, and the server re-checks your seat when it lands."}
        </p>
      </div>
      {withholdApprove ? (
        <p className="text-sm text-ink">
          A different owner or reviewer must approve your own proposal.
        </p>
      ) : (
        <ApproveForm
          ws={ws}
          skill={skill}
          versionId={versionId}
          requestId={approveRequestId}
          expectedEpoch={expectedEpoch}
          expectedSeq={expectedSeq}
        />
      )}
      <div className="border-line-soft border-t pt-3">
        <RejectDialog
          ws={ws}
          skill={skill}
          versionId={versionId}
          requestId={rejectRequestId}
          baseEpoch={baseEpoch}
          baseSeq={baseSeq}
          variant={withholdApprove ? "withdraw" : "reject"}
        />
      </div>
    </Card>
  );
}
