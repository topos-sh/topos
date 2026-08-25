import type { LoaderFunctionArgs } from "react-router";
import { Link, useLoaderData } from "react-router";
import { BrowseShell } from "@/components/browse/shell";
import { VersionFiles } from "@/components/browse/version-files";
import { Breadcrumbs } from "@/components/shell/breadcrumbs";
import { ShortId } from "@/components/ui";
import { memberPageInScope, notFound } from "@/lib/auth/guards.server";
import { loadVersionFilesData } from "@/lib/browse/version-files.server";
import { baseOf, bundleNameOf, bundlePath, useBundleBase } from "@/lib/bundle-base";
import { requireCanonicalBase } from "@/lib/bundle-base.server";
import { isVersionRef, resolveVersionRef } from "@/lib/db/queries.custody.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { custodyCurrent } from "@/lib/plane/reads.server";
import { useWsPath } from "@/lib/ws-path";

export function meta({
  params,
}: {
  params: { skill?: string; server?: string; versionId?: string };
}) {
  const short = (params.versionId ?? "").slice(0, 12);
  return [{ title: `${params.server ?? params.skill ?? "skill"} @${short} · files · Topos` }];
}

/**
 * One version's file listing + doc preview, for ANY version the vault holds — not just current.
 * The body is the shared VersionFiles (identical to the Current tab's inline listing); this page
 * adds only its own header (the skill-name link back to the Current tab + the version's short id)
 * and decides the "current" badge.
 *
 * Because this page can address any historical version, "current" is NOT the DB catalog row — it
 * is a LIVE comparison against the vault's pointer (`custodyCurrent`), which VersionFiles renders
 * as `currentChip`. Guard order mirrors the bundle face: membership first — and a signed-out
 * visitor gets the house 404 there, not a bounce to /login, because this address is members-only
 * in every face — then a cheap shape check on the version id, then the DB catalog probe (an
 * unknown NAME is the uniform 404). Every vault
 * read rides the internal custody lane and keys on the immutable `skillId` — authorization
 * already happened in the guard.
 *
 * The URL addresses a version the way git addresses an object: the full 64-hex id, or a unique
 * prefix of at least eight hex characters (`resolveVersionRef`). The 12-hex SHORT form is the one
 * every surface shows — this page's own header, History, `topos log`, every CLI receipt — so it
 * is the form a person copies, and it opens. An ambiguous or unmatched prefix is the uniform 404.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const { workspace, actor } = await memberPageInScope(request, params);
  const ws = workspace.id;
  const base = baseOf(params);
  const skill = bundleNameOf(params);
  const typed = params.versionId as string;
  if (!isVersionRef(typed)) {
    notFound();
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    notFound();
  }
  requireCanonicalBase({
    wsName: workspace.name,
    base,
    kind: row.kind,
    name: skill,
    tail: `/versions/${typed}`,
  });
  // From here the FULL id is the one truth: the vault reads, the current comparison, and every
  // link this page renders are built from it, never from the prefix the URL happened to carry.
  const versionId = await resolveVersionRef(actor, row.skillId, typed);
  if (versionId === null) {
    notFound();
  }

  const [versionFiles, current] = await Promise.all([
    loadVersionFilesData(actor, row.skillId, versionId),
    custodyCurrent(ws, row.skillId),
  ]);
  const isCurrent = current.ok && current.data.version_id === versionId;

  // `versionId` is the truth every read used; `versionRef` is the spelling the visitor arrived
  // with, and it is what every link on this page is built from. A page opened on the 12-hex short
  // id that linked on with the full 64-hex one swapped the address under the reader: the id in
  // their bar stopped matching the one they had copied, and a shared link changed shape on the
  // first click.
  return { skill, versionId, versionRef: typed, isCurrent, versionFiles };
}

export default function VersionFilesPage() {
  const { skill, versionId, versionRef, isCurrent, versionFiles } = useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  const base = useBundleBase();
  return (
    <BrowseShell>
      <div>
        <header className="flex flex-wrap items-center gap-x-2 gap-y-1">
          <Link
            to={wsPath(bundlePath(base, skill))}
            className="rounded-sm font-display font-semibold text-ink text-lg tracking-[-0.02em] underline decoration-hairline underline-offset-4 transition-colors hover:decoration-ink focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
          >
            {skill}
          </Link>
          <ShortId value={versionId} />
        </header>
        <Breadcrumbs className="mt-1" />
      </div>
      <VersionFiles
        skill={skill}
        versionId={versionId}
        versionRef={versionRef}
        currentChip={isCurrent}
        {...versionFiles}
      />
    </BrowseShell>
  );
}
