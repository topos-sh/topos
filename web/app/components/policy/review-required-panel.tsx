import { relativeTime } from "@/components/format";
import { ReviewGateSwitch } from "@/components/policy/review-gate-switch";
import { Card, SectionHeading } from "@/components/ui";
import type { PolicyEventRow } from "@/lib/db/queries.server";

const DENIED_COPY =
  "the server refused the change — a configuration issue on the server, not your permissions";

/**
 * The review-required gate. The directory now holds the real value (`reviewRequired`), so the
 * switch reflects it directly; the `policy_event` audit trail (this tier's own record of who set
 * it from here, and whether the write landed) shows below as an honest history line.
 */
export function ReviewRequiredPanel({
  lastEvent,
  isOwner,
  reviewRequired,
}: {
  lastEvent: PolicyEventRow | undefined;
  isOwner: boolean;
  /** The directory's real review-required value — what the switch shows. */
  reviewRequired: boolean;
}) {
  return (
    <section aria-labelledby="review-gate-heading" className="space-y-3">
      <SectionHeading>
        <span id="review-gate-heading">Review gate</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-sm text-dim">
          When review is required, a direct publish is refused and every change goes through a
          proposal a reviewer approves.
        </p>
        {isOwner && <ReviewGateSwitch checked={reviewRequired} />}
        <div className="space-y-1">
          <p className="text-sm text-dim">
            {lastEvent === undefined
              ? "Not set from this dashboard yet."
              : lastEvent.outcome === "ok"
                ? `Last set from this dashboard: ${lastEvent.reviewRequired ? "ON" : "OFF"}, by ${lastEvent.setBy}, ${relativeTime(lastEvent.setAt)}`
                : lastEvent.outcome === "denied"
                  ? `Last attempt from this dashboard (by ${lastEvent.setBy}, ${relativeTime(lastEvent.setAt)}) was refused: ${DENIED_COPY}.`
                  : `Last attempt from this dashboard (by ${lastEvent.setBy}, ${relativeTime(lastEvent.setAt)}) failed — the server couldn't be reached or reported a fault.`}
          </p>
        </div>
      </Card>
    </section>
  );
}
