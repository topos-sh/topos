import { type LastSetLine, LastSetNote } from "@/components/policy/last-set-line";
import { ReviewGateSwitch } from "@/components/policy/review-gate-switch";
import { Card, SectionHeading } from "@/components/ui";

/**
 * The review-required gate — the workspace's protection DEFAULT (`reviewed`/`open`), what an
 * unpinned skill inherits. An owner sees the step-up switch; a non-owner member sees the
 * current value read-only with role-honest copy. The "last set by" line reads the audit ledger
 * — the same rows the setter lands in its own transaction.
 */
export function ReviewRequiredPanel({
  isOwner,
  reviewRequired,
  lastSet,
}: {
  isOwner: boolean;
  /** The workspace row's real value — what the switch (or the read-only line) shows. */
  reviewRequired: boolean;
  lastSet: LastSetLine | null;
}) {
  return (
    <section aria-labelledby="review-gate-heading" className="space-y-3">
      <SectionHeading>
        <span id="review-gate-heading">Review gate</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-sm text-dim">
          When review is required, a direct publish is refused and every change goes through a
          proposal a reviewer approves. A skill can pin its own protection from its settings page —
          this is the default the unpinned ones inherit.
        </p>
        {isOwner ? (
          <ReviewGateSwitch checked={reviewRequired} />
        ) : (
          <p className="text-ink text-sm">
            Review is currently{" "}
            <span className="font-medium">{reviewRequired ? "required" : "not required"}</span>.
            Only an owner can change this.
          </p>
        )}
        <LastSetNote lastSet={lastSet} describe={(v) => (v === "reviewed" ? "ON" : "OFF")} />
      </Card>
    </section>
  );
}
