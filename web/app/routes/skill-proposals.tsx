import type { LoaderFunctionArgs } from "react-router";
import { useLoaderData } from "react-router";
import { ProposalsSection, type ProposalsSectionData } from "@/components/skill/proposals-section";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { sessionProposals } from "@/lib/plane/reads.server";

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `${params.skill ?? "skill"} · proposals · Topos` }];
}

/**
 * The skill's Proposals tab — the "Awaiting review" queue as its own shareable route (a sibling of
 * Current and History; each `…/proposals/{versionId}` review page is a MEMBER of this collection).
 * Same guard-then-probe order as every skill page: requireMember before any data, then the DB
 * catalog probe as the uniform 404 (an unknown NAME). A name that has never published carries no
 * `current` base, so it can hold no open proposals — the queue renders its honest empty state.
 *
 * The list is read HERE on the member-session lane and handed to ProposalsSection as plain data.
 * The vault lists only open proposals ON the current base, so the tab badge (the DB count) and this
 * list agree by construction; the current generation feeds only the per-row "current moved"
 * comparison — no epoch or seq is ever rendered.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws as string;
  const skill = params.skill as string;
  const actor = await requireMember(request, ws);
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    notFound();
  }

  let proposals: ProposalsSectionData;
  if (row.versionId === null || row.epoch === null || row.seq === null) {
    proposals = { published: false };
  } else {
    const result = await sessionProposals(actor.email, ws, row.skillId);
    proposals = {
      published: true,
      currentGeneration: { epoch: row.epoch, seq: row.seq },
      result: result.ok
        ? { ok: true, proposals: result.data.proposals }
        : { ok: false, message: result.message },
    };
  }

  return {
    ws,
    skill,
    currentShort: row.versionId !== null ? row.versionId.slice(0, 12) : "—",
    displayName: row.displayName,
    openProposals: row.openProposals,
    proposals,
  };
}

export default function SkillProposalsPage() {
  const { ws, skill, currentShort, displayName, openProposals, proposals } =
    useLoaderData<typeof loader>();
  return (
    <div className="space-y-6">
      <SkillHeader ws={ws} skill={skill} currentShort={currentShort} displayName={displayName} />
      <SkillTabs
        basePath={`/workspaces/${ws}/skills/${skill}`}
        active="proposals"
        openProposals={openProposals}
      />
      <ProposalsSection ws={ws} skill={skill} data={proposals} />
    </div>
  );
}
