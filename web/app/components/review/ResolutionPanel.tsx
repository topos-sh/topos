import { relativeTime } from "@/components/format";
import { Card, SectionHeading } from "@/components/ui";

/** The proposal row's resolution facts (dates as ISO strings; the resolver as a display name). */
export interface ResolutionFacts {
  resolved_by: string;
  reason: string | null;
  resolved_at: string | null;
}

const HEADINGS = {
  "accepted-live": "Accepted",
  superseded: "Accepted — since superseded",
  rejected: "Rejected",
  closed: "Closed without a decision",
} as const;

const BODIES = {
  "accepted-live": "This candidate was approved and is the team's current version.",
  superseded:
    "This candidate was approved and later superseded — current has moved on since. The diff above compares against today's current.",
  rejected: "This candidate was rejected and never became current.",
  closed:
    "This proposal closed without a verdict — withdrawn by its proposer, or auto-closed by a lifecycle ceremony.",
} as const;

/**
 * The terminal panel: what was decided, by whom, and (when one was recorded) why. Every value
 * is the row's recorded resolution, rendered as text nodes only; a row with no facts renders
 * the decision alone.
 */
export function ResolutionPanel({
  state,
  resolution,
}: {
  state: keyof typeof HEADINGS;
  resolution: ResolutionFacts | null;
}) {
  const decidedAt =
    resolution?.resolved_at !== null && resolution?.resolved_at !== undefined
      ? relativeTime(resolution.resolved_at)
      : "";
  return (
    <Card className="flex flex-col gap-2 p-4">
      <SectionHeading>{HEADINGS[state]}</SectionHeading>
      <p className="text-sm text-dim">{BODIES[state]}</p>
      {resolution !== null ? (
        <p className="text-sm text-faint">
          decided by {resolution.resolved_by}
          {decidedAt !== "" ? ` · ${decidedAt}` : null}
        </p>
      ) : null}
      {(state === "rejected" || state === "closed") &&
      resolution?.reason != null &&
      resolution.reason !== "" ? (
        <blockquote className="whitespace-pre-wrap border-line border-l-2 pl-3 text-sm text-ink">
          {resolution.reason}
        </blockquote>
      ) : null}
    </Card>
  );
}
