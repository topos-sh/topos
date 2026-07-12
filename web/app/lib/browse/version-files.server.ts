import type { MemberActor } from "@/lib/auth/guards.server";
import { classifyBytes, decodeTextVerbatim } from "@/lib/diff/classify";
import { MAX_BLOB_BYTES } from "@/lib/diff/model";
import { sessionBundleCapped, sessionVersionMeta } from "@/lib/plane/reads.server";
import type { WireVersionMeta } from "@/lib/plane/wire";
import { renderMarkdownHTML } from "@/lib/view/markdown.server";
import { buildListing, docFileOf, type ListingEntry } from "@/lib/view/tree";

/**
 * One version's file body, fully resolved server-side so the VersionFiles component renders it
 * synchronously (a route component fetches nothing; the loader owns every await). The Current tab
 * and the historical `versions/{id}` browse page both build this same shape and frame it
 * identically — only the current-chip boolean the callers add differs.
 */
export interface VersionFilesData {
  /** The version's immutable metadata; null when the vault had no readable version for this id. */
  version: WireVersionMeta | null;
  /** `buildListing(version.files)`, computed here so the component stays pure. */
  entries: ListingEntry[];
  /** Sanitized GFM HTML for the front-page doc, or undefined when there's nothing to preview. */
  docHtml?: string;
  /** The doc file's path (the preview heading), present iff docHtml is. */
  docName?: string;
  /** The front-page doc exists but exceeds the preview budget — one honest line. */
  docTooLarge: boolean;
}

/**
 * Assemble the meta, the file listing, and the best-effort front-page doc preview (root SKILL.md /
 * README.md under the per-file byte cap, rendered only when it decodes as text). Every read rides
 * the member-session lane on the guard-minted actor and keys on the immutable `skillId`. The
 * version-meta read can empty the whole body; the doc preview degrades to nothing on any failure,
 * so a missing blob never blanks the listing, and only a too-large doc earns its one honest line.
 */
export async function loadVersionFilesData(
  actor: MemberActor,
  skillId: string,
  versionId: string,
): Promise<VersionFilesData> {
  const meta = await sessionVersionMeta(actor.email, actor.workspaceId, skillId, versionId);
  if (!meta.ok) {
    return { version: null, entries: [], docTooLarge: false };
  }
  const version = meta.data;
  const entries = buildListing(version.files);

  const doc = docFileOf(version.files);
  let docHtml: string | undefined;
  let docName: string | undefined;
  let docTooLarge = false;
  if (doc !== undefined) {
    const blob = await sessionBundleCapped(
      actor.email,
      actor.workspaceId,
      skillId,
      doc.object_id,
      MAX_BLOB_BYTES,
    );
    if (blob.ok && classifyBytes(blob.data) === "text") {
      docHtml = await renderMarkdownHTML(decodeTextVerbatim(blob.data));
      docName = doc.path;
    } else if (!blob.ok && blob.kind === "too_large") {
      docTooLarge = true;
    }
  }

  return { version, entries, docHtml, docName, docTooLarge };
}
