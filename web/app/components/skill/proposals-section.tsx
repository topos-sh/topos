import { Link } from "react-router";
import { relativeTime } from "@/components/format";
import { CopyCommand } from "@/components/review/CopyCommand";
import { Card, Chip, SectionHeading, ShortId } from "@/components/ui";
import { buildDiffCommand } from "@/lib/diff/command";

/** One proposal row as the route loader serializes it (dates as ISO strings). */
export interface ProposalListItem {
  id: string;
  candidateVersionId: string;
  status: "open" | "approved" | "rejected" | "withdrawn";
  /** The proposer's current display name — null when the account is gone. */
  proposedByDisplay: string | null;
  createdAt: string;
  resolvedByDisplay: string | null;
  resolvedAt: string | null;
  resolvedReason: string | null;
}

/**
 * The skill's review surface: the OPEN queue first (each row links into its review page), then
 * the resolved record — proposals are rows with a terminal status now, so the decisions stay
 * readable here instead of disappearing. Every person renders by display name (attribution,
 * never an authority key); `skill` is the catalog NAME and every link is name-keyed.
 */
export function ProposalsSection({
  ws,
  skill,
  proposals,
}: {
  ws: string;
  skill: string;
  proposals: ProposalListItem[];
}) {
  const open = proposals.filter((p) => p.status === "open");
  const resolved = proposals.filter((p) => p.status !== "open");
  return (
    <div className="space-y-6">
      <section aria-labelledby="proposals-heading" className="space-y-2">
        <SectionHeading>
          <span id="proposals-heading">Awaiting review</span>
        </SectionHeading>
        {open.length === 0 ? (
          <Card className="px-4 py-3">
            <p className="text-sm text-faint">
              No open proposals. A member&apos;s publish on a review-required skill lands here; so
              does an explicit propose.
            </p>
          </Card>
        ) : (
          <Card>
            <ul>
              {open.map((proposal) => (
                <OpenRow key={proposal.id} ws={ws} skill={skill} proposal={proposal} />
              ))}
            </ul>
          </Card>
        )}
      </section>
      {resolved.length > 0 && (
        <section aria-labelledby="resolved-heading" className="space-y-2">
          <SectionHeading>
            <span id="resolved-heading">Decided</span>
          </SectionHeading>
          <Card>
            <ul>
              {resolved.map((proposal) => (
                <ResolvedRow key={proposal.id} ws={ws} skill={skill} proposal={proposal} />
              ))}
            </ul>
          </Card>
        </section>
      )}
    </div>
  );
}

function OpenRow({
  ws,
  skill,
  proposal,
}: {
  ws: string;
  skill: string;
  proposal: ProposalListItem;
}) {
  return (
    <li className="flex min-h-14 flex-wrap items-center gap-x-4 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0">
      <span className="flex items-center gap-1.5 text-sm text-dim">
        candidate <ShortId value={proposal.candidateVersionId} />
      </span>
      {proposal.proposedByDisplay !== null && (
        <span className="text-sm text-faint">by {proposal.proposedByDisplay}</span>
      )}
      <span className="text-sm text-faint">{relativeTime(proposal.createdAt)}</span>
      <span className="ml-auto flex items-center gap-2">
        <CopyCommand
          text={buildDiffCommand(skill, proposal.candidateVersionId)}
          label="Copy diff command"
        />
        <Link
          to={`/workspaces/${ws}/skills/${skill}/versions/${proposal.candidateVersionId}`}
          className="inline-flex min-h-9 items-center rounded-md border border-line px-3 font-mono text-[13px] text-dim hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
        >
          Files
        </Link>
        <Link
          to={`/workspaces/${ws}/skills/${skill}/proposals/${proposal.candidateVersionId}`}
          className="inline-flex min-h-9 items-center rounded-md border border-line px-3 font-mono text-[13px] text-dim hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
        >
          Review
        </Link>
      </span>
    </li>
  );
}

const RESOLVED_LABEL: Record<string, string> = {
  approved: "approved",
  rejected: "rejected",
  withdrawn: "withdrawn",
};

function ResolvedRow({
  ws,
  skill,
  proposal,
}: {
  ws: string;
  skill: string;
  proposal: ProposalListItem;
}) {
  return (
    <li className="flex min-h-12 flex-wrap items-center gap-x-4 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0">
      <Link
        to={`/workspaces/${ws}/skills/${skill}/proposals/${proposal.candidateVersionId}`}
        className="flex items-center gap-1.5 rounded text-sm text-dim focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
      >
        <ShortId value={proposal.candidateVersionId} />
      </Link>
      <Chip tone={proposal.status === "approved" ? "verified" : "neutral"}>
        {RESOLVED_LABEL[proposal.status] ?? proposal.status}
      </Chip>
      {proposal.resolvedByDisplay !== null && (
        <span className="text-sm text-faint">by {proposal.resolvedByDisplay}</span>
      )}
      {proposal.resolvedAt !== null && (
        <span className="text-sm text-faint">{relativeTime(proposal.resolvedAt)}</span>
      )}
    </li>
  );
}
