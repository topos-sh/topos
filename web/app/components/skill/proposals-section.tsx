import { Link } from "react-router";
import { relativeTime } from "@/components/format";
import { CopyCommand } from "@/components/review/CopyCommand";
import { Card, Chip, SectionHeading, ShortId } from "@/components/ui";
import { buildDiffCommand } from "@/lib/diff/command";
import { deriveProposalStatus, type GenerationLike } from "@/lib/diff/staleness";
import type { WireOpenProposal } from "@/lib/plane/wire";

/**
 * The Proposals tab's data, resolved entirely in the loader: either nothing has published to this
 * skill yet (`published: false`, so no `current` base and no open proposals), or the current
 * generation plus the read result — the open-proposal list on success, or an honest message on a
 * failed read. The generation feeds only the per-row "current moved" comparison; no epoch or seq is
 * ever rendered.
 */
export type ProposalsSectionData =
  | { published: false }
  | {
      published: true;
      currentGeneration: GenerationLike;
      result: { ok: true; proposals: readonly WireOpenProposal[] } | { ok: false; message: string };
    };

/**
 * The skill's action queue — open proposals, read on the member-session lane by the route loader
 * and handed here (`skill` is the catalog NAME; every link is name-keyed). The server deliberately
 * discloses no proposer on this route; nothing here invents one. The server lists only open
 * proposals ON the current base, so the tab badge (the DB count) and this list agree by
 * construction; the "current moved" chip covers the read-race window where the catalog row advanced
 * after this list was fetched.
 */
export function ProposalsSection({
  ws,
  skill,
  data,
}: {
  ws: string;
  skill: string;
  data: ProposalsSectionData;
}) {
  return (
    <section aria-labelledby="proposals-heading" className="space-y-2">
      <SectionHeading>
        <span id="proposals-heading">Awaiting review</span>
      </SectionHeading>
      <Body ws={ws} skill={skill} data={data} />
    </section>
  );
}

function Body({ ws, skill, data }: { ws: string; skill: string; data: ProposalsSectionData }) {
  if (!data.published) {
    return (
      <Card className="px-4 py-3">
        <p className="text-sm text-faint">
          Nothing published yet — a skill with no current version can hold no proposals.
        </p>
      </Card>
    );
  }
  if (!data.result.ok) {
    return (
      <Card className="px-4 py-3">
        <p className="text-sm text-faint">Couldn&apos;t list proposals — {data.result.message}.</p>
      </Card>
    );
  }
  const proposals = data.result.proposals;
  const currentGeneration = data.currentGeneration;
  if (proposals.length === 0) {
    return (
      <Card className="px-4 py-3">
        <p className="text-sm text-faint">
          No open proposals. Proposals superseded by a newer current disappear here by design; the
          server discloses no proposer on this route.
        </p>
      </Card>
    );
  }
  return (
    <Card>
      <ul>
        {proposals.map((proposal) => (
          <li
            key={proposal.version_id}
            className="flex min-h-14 flex-wrap items-center gap-x-4 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0"
          >
            <span className="flex items-center gap-1.5 text-sm text-dim">
              candidate <ShortId value={proposal.version_id} />
            </span>
            {deriveProposalStatus({ proposals }, proposal.version_id, currentGeneration) ===
              "moved" && <Chip tone="neutral">current moved</Chip>}
            <span className="text-sm text-faint">{relativeTime(proposal.created_at)}</span>
            <span className="ml-auto flex items-center gap-2">
              <code className="hidden max-w-64 overflow-x-auto whitespace-nowrap rounded bg-panel2 px-1.5 py-0.5 font-mono text-xs text-dim md:inline-block">
                topos diff {skill}@{proposal.version_id.slice(0, 12)}…
              </code>
              <CopyCommand text={buildDiffCommand(skill, proposal.version_id)} label="Copy" />
              <Link
                to={`/workspaces/${ws}/skills/${skill}/versions/${proposal.version_id}`}
                className="inline-flex min-h-9 items-center rounded-md border border-line px-3 font-mono text-[13px] text-dim hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
              >
                Files
              </Link>
              <Link
                to={`/workspaces/${ws}/skills/${skill}/proposals/${proposal.version_id}`}
                className="inline-flex min-h-9 items-center rounded-md border border-line px-3 font-mono text-[13px] text-dim hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
              >
                Review
              </Link>
            </span>
          </li>
        ))}
      </ul>
    </Card>
  );
}
