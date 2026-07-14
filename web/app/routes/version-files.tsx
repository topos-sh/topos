import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { BrowseShell } from "@/components/browse/shell";
import { VersionFiles } from "@/components/browse/version-files";
import { ShortId } from "@/components/ui";
import { notFound, requireMember } from "@/lib/auth/guards.server";
import { loadVersionFilesData } from "@/lib/browse/version-files.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { sessionCurrent } from "@/lib/plane/reads.server";

const HEX64 = /^[0-9a-f]{64}$/;

export function meta({ params }: { params: { skill?: string; versionId?: string } }) {
  const short = (params.versionId ?? "").slice(0, 12);
  return [{ title: `${params.skill ?? "skill"} @${short} · files · Topos` }];
}

/**
 * One version's file listing + doc preview, for ANY version the vault holds — not just current.
 * The body is the shared VersionFiles (identical to the Current tab's inline listing); this page
 * adds only its own header (the skill-name link back to the Current tab + the version's short id)
 * and decides the "current" badge.
 *
 * Because this page can address any historical version, "current" is NOT the DB catalog row — it
 * is a LIVE comparison against the vault's pointer (`sessionCurrent`), which VersionFiles renders
 * as `currentChip`. Guard order mirrors the review page: requireMember first, a cheap shape check
 * on the version id, then the DB catalog probe (an unknown NAME is the uniform 404). Every vault
 * read rides the member-session lane on the guard-minted actor and keys on the immutable `skillId`.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws as string;
  const skill = params.skill as string;
  const versionId = params.versionId as string;
  const actor = await requireMember(request, ws);
  if (!HEX64.test(versionId)) {
    notFound();
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    notFound();
  }

  const [versionFiles, current] = await Promise.all([
    loadVersionFilesData(actor, row.skillId, versionId),
    sessionCurrent(actor.email, ws, row.skillId),
  ]);
  const isCurrent = current.ok && current.data.record.version_id === versionId;

  return { ws, skill, versionId, isCurrent, versionFiles };
}

export default function VersionFilesPage() {
  const { ws, skill, versionId, isCurrent, versionFiles } = useLoaderData<typeof loader>();
  return (
    <BrowseShell>
      <header className="flex flex-wrap items-center gap-x-2 gap-y-1">
        <Link
          to={`/workspaces/${ws}/skills/${skill}`}
          className="rounded-sm font-display font-semibold text-ink text-lg tracking-[-0.02em] underline decoration-hairline underline-offset-4 transition-colors hover:decoration-ink focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
        >
          {skill}
        </Link>
        <ShortId value={versionId} />
      </header>
      <VersionFiles
        ws={ws}
        skill={skill}
        versionId={versionId}
        currentChip={isCurrent}
        {...versionFiles}
      />
    </BrowseShell>
  );
}
