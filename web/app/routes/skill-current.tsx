import type { LoaderFunctionArgs } from "react-router";
import { redirect, useLoaderData } from "react-router";
import { VersionFiles } from "@/components/browse/version-files";
import { SkillHeader } from "@/components/skill/skill-header";
import { SkillTabs } from "@/components/skill/skill-tabs";
import { Card } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import { loadVersionFilesData } from "@/lib/browse/version-files.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { resolveSkillName } from "@/lib/db/resolve.server";

export function meta({ params }: { params: { skill?: string } }) {
  return [{ title: `${params.skill ?? "skill"} · Topos` }];
}

/**
 * The skill's Current tab — the DEFAULT view, showing the current version's files + doc preview
 * inline. Proposals and History are sibling routes (see SkillTabs); making the tabs real routes
 * rather than client state means each is a shareable URL that renders as one complete document
 * under blocking SSR.
 *
 * The catalog row this page probes IS the directory's identity surface: the NAME exists the moment
 * a skill is minted, and the `current` pointer joins in when a publish has landed one. A skill
 * whose NAME is unknown here is the uniform 404; a known name that has NEVER published (`versionId`
 * is null) renders honestly — the header and tabs stand, and the body says nothing is published
 * yet, rather than inventing a pointer. When a current version exists we hand VersionFiles the
 * fully-resolved body with `currentChip` set (by construction this IS current). Authorization
 * (requireMember) runs before any data read; the same probe is the uniform 404.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws as string;
  const skill = params.skill as string;
  const actor = await requireMember(request, ws);
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    // A rename left an old name behind: follow the resolving hint to the live name; else 404.
    const resolved = await resolveSkillName(actor, skill);
    if (resolved !== undefined && resolved.via === "hint" && resolved.status === "active") {
      throw redirect(`/workspaces/${ws}/skills/${resolved.name}`);
    }
    notFound();
  }

  const versionFiles =
    row.versionId !== null ? await loadVersionFilesData(actor, row.skillId, row.versionId) : null;

  return {
    ws,
    skill,
    currentShort: row.versionId !== null ? row.versionId.slice(0, 12) : "—",
    displayName: row.displayName,
    openProposals: row.openProposals,
    versionId: row.versionId,
    versionFiles,
  };
}

export default function SkillCurrentPage() {
  const { ws, skill, currentShort, displayName, openProposals, versionId, versionFiles } =
    useLoaderData<typeof loader>();
  return (
    <div className="space-y-6">
      <SkillHeader ws={ws} skill={skill} currentShort={currentShort} displayName={displayName} />
      <SkillTabs
        basePath={`/workspaces/${ws}/skills/${skill}`}
        active="current"
        openProposals={openProposals}
      />
      {versionId !== null && versionFiles !== null ? (
        <VersionFiles ws={ws} skill={skill} versionId={versionId} currentChip {...versionFiles} />
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
