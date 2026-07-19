import type { LoaderFunctionArgs } from "react-router";
import { redirect, useLoaderData } from "react-router";
import { VersionFiles } from "@/components/browse/version-files";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { Card } from "@/components/ui";
import { actorFromSession, memberInScope, notFound } from "@/lib/auth/guards.server";
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
 * The skill FACE — resource address and canonical Current tab as ONE route. A skill page is
 * MEMBERS-ONLY: an anonymous browser gets the house 404, indistinguishable from a mistyped path, so
 * nothing about a skill (not even that the address shape names one) leaks to a signed-out visitor.
 * (A non-browser document fetch still got the constant protocol card from the server entry — that
 * machine face is existence-blind and teaches `topos follow` regardless.) A signed-in member gets
 * the skill page WITH chrome; a signed-in non-member (or unknown workspace slug) gets the same 404.
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
    // Signed out: the skill face is not a public teaser — it is the uniform house 404, so an
    // anonymous probe cannot tell a real skill from a nonexistent one (or from any other path).
    notFound();
  }
  const { workspace, actor: memberActor } = await memberInScope(actor, params);
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
