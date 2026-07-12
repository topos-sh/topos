import { formatBytes } from "@/components/format";
import { Card, Chip, ShortId } from "@/components/ui";
import { buildDiffCommand } from "@/lib/diff/command";
import type { FileDiffModel } from "@/lib/diff/model";

/**
 * One changed file. The sticky header shows the path as a TEXT NODE only; the body is either
 * the SANITIZED renderer HTML (text files), or an honest non-render card (binary, over a cap,
 * a failed blob fetch, a pure move, a pure mode change). Raw binary bytes never reach the page.
 *
 * The route loader owns fetching AND rendering: it turns the plan into models and renders the
 * text case to sanitized HTML, passed here as `html`. This card only places what it's handed —
 * so it stays a pure, synchronous component (no plane read of its own). `skill` is the catalog
 * NAME (the too-large hand-off addresses the skill by name).
 */
export function DiffFileCard({
  model,
  anchorId,
  skill,
  versionId,
  html,
}: {
  model: FileDiffModel;
  anchorId: string;
  skill: string;
  versionId: string;
  /** The sanitized diff HTML for a `text` presentation; null/absent for every other presentation. */
  html?: string | null;
}) {
  const { entry } = model;
  return (
    <section id={anchorId} className="scroll-mt-4">
      <Card className="overflow-hidden">
        <div className="sticky top-0 z-10 flex min-h-9 flex-wrap items-center gap-2 border-line-soft border-b bg-ground px-3 py-1.5">
          <span className="break-all font-mono text-xs text-dim">
            {entry.kind === "moved" ? `${entry.prevPath ?? ""} → ${entry.path}` : entry.path}
          </span>
          {entry.kind === "mode-only" ? (
            <Chip tone="neutral">
              mode {entry.modes.old ?? ""} → {entry.modes.new ?? ""}
            </Chip>
          ) : null}
          {entry.kind === "added" ? <Chip tone="neutral">added</Chip> : null}
          {entry.kind === "deleted" ? <Chip tone="neutral">deleted</Chip> : null}
          {entry.kind === "moved" ? <Chip tone="neutral">moved</Chip> : null}
        </div>
        <CardBody model={model} skill={skill} versionId={versionId} html={html} />
      </Card>
    </section>
  );
}

function CardBody({
  model,
  skill,
  versionId,
  html,
}: {
  model: FileDiffModel;
  skill: string;
  versionId: string;
  html?: string | null;
}) {
  const { entry, presentation } = model;

  if (entry.kind === "moved") {
    return <Note>moved — contents unchanged</Note>;
  }
  if (entry.kind === "mode-only") {
    return (
      <Note>
        Only the file mode changed ({entry.modes.old ?? ""} → {entry.modes.new ?? ""}) — the
        contents are byte-identical.
      </Note>
    );
  }
  if (presentation === "binary") {
    return (
      <Note>
        Binary file changed
        <span className="block pt-1 text-xs text-faint">
          {sizesLine(model)} · {objectIdsLine(model)}
        </span>
      </Note>
    );
  }
  if (presentation === "too-large") {
    return (
      <Note>
        {model.reason === "file-count"
          ? "This change has more files than the page renders."
          : "This file is larger than the page renders."}{" "}
        Inspect it on an enrolled device:{" "}
        <code className="font-mono text-xs text-dim">{buildDiffCommand(skill, versionId)}</code>
      </Note>
    );
  }
  if (presentation === "fetch-failed") {
    return (
      <Note>
        Couldn&apos;t fetch this file&apos;s bytes from the server — reload the page to retry. The
        rest of the change rendered normally.
      </Note>
    );
  }
  return (
    <div className="overflow-x-auto text-sm">
      {/* biome-ignore lint/security/noDangerouslySetInnerHtml: sanitized server-side by the loader's diff renderer (allowlist + adversarial tests) before it reaches this prop */}
      <div dangerouslySetInnerHTML={{ __html: html ?? "" }} />
    </div>
  );
}

function Note({ children }: { children: React.ReactNode }) {
  return <p className="px-3 py-3 text-sm text-dim">{children}</p>;
}

function sizesLine(model: FileDiffModel): string {
  const parts: string[] = [];
  if (model.sizes.old !== undefined) {
    parts.push(formatBytes(model.sizes.old));
  }
  if (model.sizes.new !== undefined) {
    parts.push(formatBytes(model.sizes.new));
  }
  return parts.join(" → ");
}

function objectIdsLine(model: FileDiffModel): React.ReactNode {
  return (
    <>
      {model.entry.objectIds.old !== undefined ? (
        <ShortId value={model.entry.objectIds.old} />
      ) : null}
      {model.entry.objectIds.old !== undefined && model.entry.objectIds.new !== undefined
        ? " → "
        : null}
      {model.entry.objectIds.new !== undefined ? (
        <ShortId value={model.entry.objectIds.new} />
      ) : null}
    </>
  );
}
