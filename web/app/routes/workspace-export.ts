import type { LoaderFunctionArgs } from "react-router";
import { requireOwnerInScope } from "@/lib/auth/guards.server";
import { type SkillIndexRow, skillIndexOf } from "@/lib/db/queries.server";
import { type ZipEntry, zipStream } from "@/lib/export/zip.server";
import { custodyObjectCapped, custodyVersionMeta } from "@/lib/plane/reads.server";

/**
 * `GET …/settings/export` — a workspace's whole catalog as a zip: one directory per skill at its
 * CURRENT version (`<skill-name>/<file-path>`), plus a top-level `manifest.json`. A resource
 * route (loader only, no page) — a native download link on the Settings page points here.
 *
 * Authorization is an OWNER seat, resolved the SAME way its neighbor settings ceremonies are:
 * `requireOwnerInScope` (a non-owner — down to a signed-in stranger —
 * is the uniform 404, never a 403; a signed-out visitor is bounced to login). Exporting every
 * skill at once is a workspace-wide act, so it sits at the owner grade the Settings page gates on.
 *
 * The bytes are read through the ONE custody transport's existing verified reads
 * (`custodyVersionMeta` for a version's file listing, `custodyObjectCapped` for each object's
 * bytes) — this route opens no new vault surface. The archive STREAMS: entries are pulled one at
 * a time, so at most one object's bytes live in memory at once (independent of the archive's byte
 * size), and an object over the cap fails the export rather than buffering unbounded. The zip's
 * own central-directory metadata plus this loader's catalog list grow with the file COUNT, not
 * byte size — a modest, unavoidable cost the ZIP format and any manifest share. The git file mode
 * rides each entry so executable scripts extract executable.
 */

/** A generous per-object ceiling — a skill file this large is pathological; refuse, don't buffer. */
const MAX_OBJECT_BYTES = 64 * 1024 * 1024;

/** A regular, non-executable file — the fallback when a git mode string is missing or malformed. */
const REGULAR_FILE_MODE = 0o100644;

/**
 * A custody file's mode arrives as a git octal STRING (`"100644"`, `"100755"`, `"120000"`); parse
 * it to the Unix `st_mode` number the zip records, falling back to a regular file on anything odd.
 */
function fileMode(mode: string): number {
  const parsed = Number.parseInt(mode, 8);
  return Number.isInteger(parsed) && parsed > 0 ? parsed : REGULAR_FILE_MODE;
}

interface ExportManifest {
  workspace: string;
  generated_at: string;
  skills: { name: string; version_id: string }[];
}

export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const { workspace, actor } = await requireOwnerInScope(request, params);

  const generatedAt = new Date();
  // The catalog IS the app's own rows; only skills holding a CURRENT version are exportable
  // (a name with nothing published yet has no bytes to archive).
  const published = (await skillIndexOf(actor, workspace.id)).filter(
    (row): row is SkillIndexRow & { versionId: string } => row.versionId !== null,
  );

  const manifest: ExportManifest = {
    workspace: workspace.name,
    generated_at: generatedAt.toISOString(),
    skills: published.map((row) => ({ name: row.name, version_id: row.versionId })),
  };

  const filename = `${workspace.name}-skills.zip`;
  return new Response(zipStream(exportEntries(workspace.id, published, manifest), generatedAt), {
    status: 200,
    headers: {
      "content-type": "application/zip",
      "content-disposition": `attachment; filename="${filename}"`,
      "cache-control": "no-store",
    },
  });
}

/**
 * The archive's entries, produced lazily: the manifest first (pure app data, always present),
 * then each skill's files at its current version. A vault read that fails mid-stream errors the
 * stream — the download breaks honestly rather than silently omitting bytes, matching how the
 * object-stream route surfaces an upstream fault.
 */
async function* exportEntries(
  ws: string,
  published: (SkillIndexRow & { versionId: string })[],
  manifest: ExportManifest,
): AsyncGenerator<ZipEntry> {
  yield {
    path: "manifest.json",
    bytes: new TextEncoder().encode(`${JSON.stringify(manifest, null, 2)}\n`),
  };
  for (const skill of published) {
    const meta = await custodyVersionMeta(ws, skill.skillId, skill.versionId);
    if (!meta.ok) {
      throw new Error("export: version metadata unavailable");
    }
    for (const file of meta.data.files) {
      const object = await custodyObjectCapped(ws, skill.skillId, file.object_id, MAX_OBJECT_BYTES);
      if (!object.ok) {
        throw new Error("export: object bytes unavailable");
      }
      yield { path: `${skill.name}/${file.path}`, bytes: object.data, mode: fileMode(file.mode) };
    }
  }
}
