import { FileListing } from "@/components/browse/file-listing";
import { BrowseEmpty } from "@/components/browse/shell";
import { firstLine } from "@/components/format";
import { Card, Chip, SectionHeading } from "@/components/ui";
import type { CustodyVersionMeta } from "@/lib/plane/wire";
import type { ListingEntry } from "@/lib/view/tree";

/**
 * One version's content, shared VERBATIM by the Current tab (the root skill page) and the
 * historical `/versions/{id}` page — the meta line, the server's bundle_digest, the file listing,
 * and the best-effort doc preview live in exactly one place. The route loader owns every read (the
 * version meta, the file listing it built, and the front-page doc it rendered to sanitized HTML);
 * this component only places what it's handed. A null `version` is the loader's honest "no readable
 * version" — the doc preview degrades to nothing on any failure, so a missing blob never blanks the
 * listing.
 *
 * `currentChip` is the callers' one framing knob: the Current tab passes it `true` (the catalog row
 * it probed IS the current pointer, so this IS current by construction), the versions page passes
 * its own live current comparison. Placing the chip HERE (beside the device line) keeps the body a
 * single shape — both callers frame the content identically and only the boolean differs. `skill`
 * is the catalog NAME (the URL key; the workspace prefix comes from `useWsPath` in FileListing).
 */
export function VersionFiles({
  skill,
  versionId,
  version,
  entries,
  currentChip = false,
  docHtml,
  docName,
  docTooLarge = false,
}: {
  skill: string;
  versionId: string;
  /** The version's immutable metadata; null when the server had no readable version for this id. */
  version: CustodyVersionMeta | null;
  /** `buildListing(version.files)`, computed by the loader. */
  entries: readonly ListingEntry[];
  currentChip?: boolean;
  /** The front-page doc rendered to sanitized HTML by the loader (best-effort; absent = none). */
  docHtml?: string;
  docName?: string;
  docTooLarge?: boolean;
}) {
  if (version === null) {
    return (
      <BrowseEmpty heading="This version isn't available">
        The server has no readable version with this id for this skill.
      </BrowseEmpty>
    );
  }
  const fileCount = version.files.length;

  return (
    <div className="space-y-6">
      <div className="space-y-3">
        <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
          <span className="font-mono text-faint text-xs">{version.author}</span>
          {currentChip && <Chip tone="accent">current</Chip>}
          <span className="min-w-0 flex-1 truncate text-dim text-sm">
            {firstLine(version.message)}
          </span>
          <span className="text-faint text-xs">
            {fileCount === 1 ? "1 file" : `${fileCount} files`}
          </span>
        </div>
        <div>
          <p className="text-faint text-xs">
            <code className="font-mono">bundle_digest</code> — as recorded by the server
          </p>
          <code className="mt-1 block break-all font-mono text-dim text-xs">
            {version.bundle_digest ?? "—"}
          </code>
        </div>
      </div>

      <FileListing skill={skill} versionId={versionId} entries={entries} />

      {docHtml !== undefined && docName !== undefined && (
        <section className="space-y-2">
          <SectionHeading>{docName}</SectionHeading>
          <Card className="p-5">
            {/* biome-ignore lint/security/noDangerouslySetInnerHtml: sanitized GFM HTML rendered by the loader (lib/view/markdown) — no raw HTML, no img survives its sanitizer */}
            <div className="doc-prose" dangerouslySetInnerHTML={{ __html: docHtml }} />
          </Card>
        </section>
      )}
      {docTooLarge && (
        <p className="text-faint text-xs">
          The doc file is larger than the 1 MiB preview budget — open it from the list to read it.
        </p>
      )}
    </div>
  );
}
