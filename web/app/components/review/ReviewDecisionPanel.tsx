import { Card, SectionHeading } from "@/components/ui";
import { ApproveForm } from "./ApproveForm";
import { RejectDialog } from "./RejectDialog";

/**
 * The browser decision surface for a PENDING proposal, rendered for a viewer whose seat can
 * decide (owner|reviewer) — the route action re-checks all of it. The approve binds
 * `expectedGeneration`, the current generation the rendered diff was computed against, so the
 * server refuses a moved pointer instead of approving something the reviewer didn't see.
 *
 * `withholdApprove` is the four-eyes case — the viewer proposed this candidate and the skill's
 * effective protection is 'reviewed'. The gate applies to APPROVE only, so the panel keeps the
 * reject flow live as a WITHDRAW (the same resolve, verdict `withdrawn`, reason still
 * mandatory) and renders an inline four-eyes line where Approve would be. Display-only either
 * way: the action refuses a self-approve regardless. ws/skill ride the review route's own
 * params; each form re-sends the version id.
 */
export function ReviewDecisionPanel({
  versionId,
  expectedGeneration,
  withholdApprove = false,
}: {
  versionId: string;
  /** The live current generation the diff above was computed against (the approve binding). */
  expectedGeneration: string;
  /** The viewer proposed this candidate under review-required — Approve is withheld. */
  withholdApprove?: boolean;
}) {
  return (
    <Card className="flex flex-col gap-4 p-4">
      <div className="flex flex-col gap-1">
        <SectionHeading>{withholdApprove ? "Your proposal" : "Decide"}</SectionHeading>
        <p className="text-sm text-dim">
          {withholdApprove
            ? "You proposed this candidate, and review is required for this skill. Withdrawing records your reason under your name, and the server re-checks your seat when it lands."
            : "Approving makes this candidate the team's current version — followers pick it up on their next pull. Either decision records under your name, and the server re-checks your seat when it lands."}
        </p>
      </div>
      {withholdApprove ? (
        <p className="text-sm text-ink">
          A different owner or reviewer must approve your own proposal.
        </p>
      ) : (
        <ApproveForm versionId={versionId} expectedGeneration={expectedGeneration} />
      )}
      <div className="border-line-soft border-t pt-3">
        <RejectDialog versionId={versionId} variant={withholdApprove ? "withdraw" : "reject"} />
      </div>
    </Card>
  );
}
