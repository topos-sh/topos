import { relativeTime } from "@/components/format";
import { Card, SectionHeading } from "@/components/ui";

/** The detail read's resolution facts — older rows may carry partial facts (render what exists). */
export interface ResolutionFacts {
  resolved_by: string;
  reason: string | null;
  resolved_at: string | null;
}

const HEADINGS = {
  "accepted-live": "Accepted",
  superseded: "Accepted — since superseded",
  rejected: "Rejected",
} as const;

const BODIES = {
  "accepted-live": "This candidate was approved and is the team's current version.",
  superseded:
    "This candidate was approved and later superseded — current has moved on since. The diff above compares against today's current.",
  rejected: "This candidate was rejected and never became current.",
} as const;

/**
 * The terminal panel: what was decided, by whom, and (on a reject) why. Every value is the
 * server's recorded resolution, rendered as text nodes only; a pre-disclosure row with no facts
 * renders the decision alone.
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
      {state === "rejected" && resolution?.reason != null && resolution.reason !== "" ? (
        <blockquote className="whitespace-pre-wrap border-line border-l-2 pl-3 text-sm text-ink">
          {resolution.reason}
        </blockquote>
      ) : null}
    </Card>
  );
}
