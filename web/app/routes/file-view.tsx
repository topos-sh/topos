import type { LoaderFunctionArgs } from "react-router";
import { useLoaderData } from "react-router";
import { PathBreadcrumb } from "@/components/browse/path-breadcrumb";
import { BrowseEmpty, BrowseShell } from "@/components/browse/shell";
import { ViewToggle } from "@/components/browse/view-toggle";
import { Card, Chip } from "@/components/ui";
import { notFound, requireMember, workspaceInScope } from "@/lib/auth/guards.server";
import { skillIndexRow } from "@/lib/db/queries.server";
import { classifyBytes, decodeTextVerbatim } from "@/lib/diff/classify";
import { MAX_BLOB_BYTES, MAX_HIGHLIGHT_BYTES } from "@/lib/diff/model";
import { custodyObjectCapped, custodyVersionMeta } from "@/lib/plane/reads.server";
import { renderCodeHTML } from "@/lib/view/highlight.server";
import { languageForPath } from "@/lib/view/language";
import { renderMarkdownHTML } from "@/lib/view/markdown.server";
import { wsPathServer } from "@/lib/ws-url.server";

const HEX64 = /^[0-9a-f]{64}$/;

/**
 * The splat's segments percent-decoded, or undefined when they don't decode (a literal `%` outside
 * an escape). Both the as-delivered and decoded forms are tried against the manifest; a file whose
 * NAME contains an escape sequence still resolves via the as-delivered form.
 */
function decodedSegments(segments: readonly string[]): string[] | undefined {
  try {
    return segments.map((segment) => decodeURIComponent(segment));
  } catch {
    return undefined;
  }
}

export function meta({ params }: { params: { skill?: string; versionId?: string; "*"?: string } }) {
  const short = (params.versionId ?? "").slice(0, 12);
  const splat = params["*"] ?? "";
  const segments = splat.length > 0 ? splat.split("/") : [];
  const display = decodedSegments(segments) ?? segments;
  const last = display[display.length - 1] ?? "";
  return [{ title: `${params.skill ?? "skill"} @${short} · ${last} · Topos` }];
}

/** How a file's bytes are presented once fetched (or why they weren't). */
type FileContent =
  | { presentation: "too-large" }
  | { presentation: "fetch-failed"; message: string }
  | { presentation: "binary" }
  | { presentation: "markdown"; html: string }
  | { presentation: "code"; html: string }
  | { presentation: "pre"; text: string; capSkipped: boolean };

/**
 * One file of a version, rendered inline: markdown as sanitized HTML, code as sanitized highlighted
 * HTML (both under a Rendered | Raw toggle), everything else as an escaped <pre>. The async render
 * runs HERE in the loader (a route component fetches and renders nothing itself). Guard order
 * mirrors the review page (requireMember → id shape → catalog probe). The path is rebuilt from the
 * splat and used ONLY as a manifest lookup key — never as a filesystem path — so there is no
 * traversal surface: a "../x" simply fails to match a manifest entry and 404s. The blob rides the
 * internal custody lane under the per-file byte cap, and each failure mode degrades to an honest
 * card.
 */
export async function loader({ request, params }: LoaderFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const ws = workspace.id;
  const skill = params.skill as string;
  const versionId = params.versionId as string;
  const splat = params["*"] ?? "";
  const raw = new URL(request.url).searchParams.get("view") === "raw";

  const actor = await requireMember(request, ws);
  if (!HEX64.test(versionId)) {
    notFound();
  }
  const row = await skillIndexRow(actor, skill);
  if (row === undefined) {
    notFound();
  }

  const meta = await custodyVersionMeta(ws, row.skillId, versionId);
  if (!meta.ok) {
    return { kind: "meta_missing" as const, skill, versionId };
  }

  // The path is a pure lookup key into the version manifest (never a filesystem path). Both delivery
  // forms are tried — as-delivered first, decoded second — and a miss is the uniform 404.
  const segments = splat.length > 0 ? splat.split("/") : [];
  const asDelivered = segments.join("/");
  const decoded = decodedSegments(segments)?.join("/");
  const file =
    meta.data.files.find((f) => f.path === asDelivered) ??
    (decoded !== undefined && decoded !== asDelivered
      ? meta.data.files.find((f) => f.path === decoded)
      : undefined);
  if (file === undefined) {
    notFound();
  }
  // From here on the MANIFEST path is the one truth — display and links derive from it, never from
  // the raw URL segments.
  const filePath = file.path;
  const displaySegments = filePath.split("/");
  const encodedPath = displaySegments.map(encodeURIComponent).join("/");
  const fileBasePath = wsPathServer(
    workspace.name,
    `skills/${skill}/versions/${versionId}/files/${encodedPath}`,
  );
  const executable = file.mode === "100755";

  const blob = await custodyObjectCapped(ws, row.skillId, file.object_id, MAX_BLOB_BYTES);

  let sizeBytes: number | undefined;
  let showToggle = false;
  let content: FileContent;

  if (!blob.ok) {
    content =
      blob.kind === "too_large"
        ? { presentation: "too-large" }
        : { presentation: "fetch-failed", message: blob.message };
  } else {
    sizeBytes = blob.data.byteLength;
    if (classifyBytes(blob.data) === "binary") {
      content = { presentation: "binary" };
    } else {
      const text = decodeTextVerbatim(blob.data);
      const info = languageForPath(filePath);
      const withinHighlightCap = blob.data.length <= MAX_HIGHLIGHT_BYTES;
      // A rendered form exists for markdown and for code within the highlight cap; that (not the
      // current view) earns the toggle, so Raw can always switch back to Rendered.
      showToggle = info.kind === "markdown" || (info.kind === "code" && withinHighlightCap);

      if (info.kind === "markdown" && !raw) {
        content = { presentation: "markdown", html: await renderMarkdownHTML(text) };
      } else if (info.kind === "code" && !raw && withinHighlightCap) {
        content = { presentation: "code", html: await renderCodeHTML(text, info.lang) };
      } else {
        // plain kind, raw view, or code past the highlight cap → an escaped <pre> (a React text
        // child, never innerHTML). Only the cap case — not the raw toggle — earns the skip note.
        content = {
          presentation: "pre",
          text,
          capSkipped: info.kind === "code" && !withinHighlightCap,
        };
      }
    }
  }

  return {
    kind: "file" as const,
    skill,
    versionId,
    displaySegments,
    fileBasePath,
    executable,
    sizeBytes,
    showToggle,
    raw,
    content,
  };
}

export default function FileViewPage() {
  const data = useLoaderData<typeof loader>();
  if (data.kind === "meta_missing") {
    return (
      <BrowseShell>
        <BrowseEmpty heading="This version isn't available">
          The server has no readable version with this id for this skill.
        </BrowseEmpty>
      </BrowseShell>
    );
  }

  const {
    skill,
    versionId,
    displaySegments,
    fileBasePath,
    executable,
    sizeBytes,
    showToggle,
    raw,
    content,
  } = data;
  return (
    <BrowseShell>
      <header className="space-y-2">
        <PathBreadcrumb skill={skill} versionId={versionId} segments={displaySegments} />
        {(sizeBytes !== undefined || executable) && (
          <p className="flex items-center gap-2 text-faint text-xs">
            {sizeBytes !== undefined && <span>{sizeBytes} bytes</span>}
            {executable && <Chip tone="neutral">executable</Chip>}
          </p>
        )}
      </header>
      {showToggle && <ViewToggle basePath={fileBasePath} raw={raw} />}
      <FileBody content={content} />
    </BrowseShell>
  );
}

function FileBody({ content }: { content: FileContent }) {
  if (content.presentation === "too-large") {
    return (
      <Card className="px-4 py-3">
        <p className="text-dim text-sm">
          This file is larger than the 1 MiB per-file view budget, so its bytes weren&apos;t fetched
          here. Pull the skill with the topos CLI to read it in full.
        </p>
      </Card>
    );
  }
  if (content.presentation === "fetch-failed") {
    return (
      <Card className="px-4 py-3">
        <p className="text-dim text-sm">
          This file couldn&apos;t be fetched — {content.message}. Reload to retry.
        </p>
      </Card>
    );
  }
  if (content.presentation === "binary") {
    return (
      <Card className="px-4 py-3">
        <p className="text-dim text-sm">binary file — not rendered here.</p>
      </Card>
    );
  }
  if (content.presentation === "markdown") {
    return (
      <Card className="p-5">
        {/* biome-ignore lint/security/noDangerouslySetInnerHtml: sanitized GFM HTML from renderMarkdownHTML (lib/view/markdown.ts) — no raw HTML, no img survives its sanitizer */}
        <div className="doc-prose" dangerouslySetInnerHTML={{ __html: content.html }} />
      </Card>
    );
  }
  if (content.presentation === "code") {
    return (
      // biome-ignore lint/security/noDangerouslySetInnerHtml: sanitized github-light HTML from renderCodeHTML (lib/view/highlight.ts) — inline-styled spans only, no user bytes in attributes
      <div className="code-view" dangerouslySetInnerHTML={{ __html: content.html }} />
    );
  }
  return (
    <div className="space-y-2">
      <pre className="overflow-x-auto whitespace-pre rounded-md border border-line-soft bg-panel p-4 font-mono text-[13px] text-ink">
        {content.text}
      </pre>
      {content.capSkipped && (
        <p className="text-faint text-xs">Syntax highlighting is skipped for files over 128 KB.</p>
      )}
    </div>
  );
}
