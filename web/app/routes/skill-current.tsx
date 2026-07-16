import type { LoaderFunctionArgs } from "react-router";
import { redirect, useLoaderData } from "react-router";
import { VersionFiles } from "@/components/browse/version-files";
import { ResourcePage } from "@/components/resource-page";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { Card } from "@/components/ui";
import {
  actorFromSession,
  notFound,
  requireMember,
  workspaceInScope,
} from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { loadVersionFilesData } from "@/lib/browse/version-files.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { resolveSkillName } from "@/lib/db/resolve.server";
import { useWsPath } from "@/lib/ws-path";
import { wsPathServer } from "@/lib/ws-url.server";

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `${params.skill ?? "skill"} · Topos` }];
}

/**
 * The skill FACE — resource address and canonical Current tab as ONE route. Admission mirrors the
 * workspace face: a non-browser document fetch got the protocol card already; an anonymous browser
 * gets the constant teaser; a signed-in member gets the skill page WITH chrome; a signed-in
 * non-member (or unknown workspace slug) gets the house 404.
 *
 * The Current tab is the DEFAULT skill view: the current version's files + doc preview inline.
 * Proposals and History are sibling MEMBER-only routes (see SkillTabs). The catalog row this page
 * probes IS the directory's identity surface: the NAME exists the moment a skill is minted, and the
 * `current` pointer joins in when a publish has landed one. A known name that has NEVER published
 * (`versionId` null) renders honestly; an unknown NAME is the uniform 404 (a rename hint redirects).
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (actor === null) {
    return { face: "teaser" as const };
  }
  const workspace = await workspaceInScope(params);
  const memberActor = await requireMember(request, workspace.id);
  const skill = params.skill as string;
  const row = await skillIndexRow(memberActor, skill);
  if (row === undefined) {
    // A rename left an old name behind: follow the resolving hint to the live name; else 404.
    const resolved = await resolveSkillName(memberActor, skill);
    if (resolved !== undefined && resolved.via === "hint" && resolved.status === "active") {
      throw redirect(wsPathServer(workspace.name, `skills/${resolved.name}`));
    }
    notFound();
  }

  const versionFiles =
    row.versionId !== null
      ? await loadVersionFilesData(memberActor, row.skillId, row.versionId)
      : null;

  return {
    face: "page" as const,
    wsName: workspace.name,
    skill,
    currentShort: row.versionId !== null ? row.versionId.slice(0, 12) : "—",
    displayName: row.displayName,
    kind: row.kind,
    openProposals: row.openProposals,
    versionId: row.versionId,
    versionFiles,
  };
}

export default function SkillCurrentPage() {
  const data = useLoaderData<typeof loader>();
  if (data.face === "teaser") {
    return <ResourcePage />;
  }
  return <SkillCurrentContent {...data} />;
}

function SkillCurrentContent({
  wsName,
  skill,
  currentShort,
  displayName,
  kind,
  openProposals,
  versionId,
  versionFiles,
}: Extract<Awaited<ReturnType<typeof loader>>, { face: "page" }>) {
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
        active="current"
        openProposals={openProposals}
      />
      {versionId !== null && versionFiles !== null ? (
        <VersionFiles skill={skill} versionId={versionId} currentChip {...versionFiles} />
      ) : (
        <Card className="px-4 py-3">
          <p className="text-dim text-sm">
            Nothing published yet — this skill has a name in the catalog, but no version has been
            published to it. Publish one with the topos CLI and it appears here.
          </p>
        </Card>
      )}
    </div>
  );
}
