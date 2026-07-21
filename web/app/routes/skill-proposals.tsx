import type { LoaderFunctionArgs } from "react-router";
import { redirect, useLoaderData } from "react-router";
import { type ProposalListItem, ProposalsSection } from "@/components/skill/proposals-section";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { notFound, requireMemberInScope } from "@/lib/auth/guards.server";
import { proposalsOf, skillIndexRow } from "@/lib/db/queries.server";
import { resolveSkillName } from "@/lib/db/resolve.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `${params.skill ?? "skill"} · proposals · Topos` }];
}

/**
 * The skill's Proposals tab — the review queue as its own shareable route (a sibling of Current
 * and History; each `…/proposals/{versionId}` review page is a MEMBER of this collection). Same
 * guard-then-probe order as every skill page: requireMember before any data, then the DB catalog
 * probe as the uniform 404 (an unknown NAME).
 *
 * Proposals are the app's OWN rows now — one read, no vault call: the open ones first (the
 * queue), then the resolved record (who decided, and why on a reject). The tab badge (the DB
 * count) and the open list agree by construction — they read the same table.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await requireMemberInScope(request, params);
  const skill = params.skill as string;
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    // A rename left an old name behind: follow the resolving hint to the live name; else 404.
    const resolved = await resolveSkillName(actor, skill);
    if (resolved !== undefined && resolved.via === "hint" && resolved.status === "active") {
      throw redirect(wsPathServer(workspace.name, `skills/${resolved.name}/proposals`));
    }
    notFound();
  }

  const rows = await proposalsOf(actor, row.skillId);
  const proposals: ProposalListItem[] = rows.map((p) => ({
    id: p.id,
    candidateVersionId: p.candidateVersionId,
    status: p.status as ProposalListItem["status"],
    proposedByDisplay: p.proposedByDisplay,
    createdAt: p.createdAt.toISOString(),
    resolvedByDisplay: p.resolvedByDisplay,
    resolvedAt: p.resolvedAt !== null ? p.resolvedAt.toISOString() : null,
    resolvedReason: p.resolvedReason,
  }));

  return {
    isOwner: actor.role === "owner",
    wsName: workspace.name,
    skill,
    currentShort: row.versionId !== null ? row.versionId.slice(0, 12) : "—",
    displayName: row.displayName,
    kind: row.kind,
    openProposals: row.openProposals,
    proposals,
  };
}

export default function SkillProposalsPage() {
  const { isOwner, wsName, skill, currentShort, displayName, kind, openProposals, proposals } =
    useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  return (
    <div className="space-y-6">
      <SkillHeader
        ws={wsName}
        skill={skill}
        currentShort={currentShort}
        displayName={displayName}
        kind={kind}
      />
      <SkillTabs
        basePath={wsPath(`skills/${skill}`)}
        active="proposals"
        openProposals={openProposals}
        showSettings={isOwner}
      />
      <ProposalsSection skill={skill} proposals={proposals} />
    </div>
  );
}
