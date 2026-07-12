import { Chip, SectionHeading } from "@/components/ui";
import type { DiffEntry, DiffKind } from "@/lib/diff/model";

const KIND_LABEL: Record<Exclude<DiffKind, "unchanged">, string> = {
  added: "added",
  deleted: "deleted",
  modified: "modified",
  "mode-only": "mode change",
  moved: "moved",
};

/**
 * The anchor list for long diffs: one row per CHANGED file (`entries` excludes unchanged),
 * linking to its ordinal card id (file-0, file-1, … — never a raw path in an anchor).
 * Paths render as text nodes. The index here matches the DiffFileCard order one-to-one.
 */
export function FileSummaryList({
  entries,
  unchangedCount,
}: {
  entries: readonly DiffEntry[];
  unchangedCount: number;
}) {
  const counts = new Map<string, number>();
  for (const e of entries) {
    if (e.kind !== "unchanged") {
      const label = KIND_LABEL[e.kind];
      counts.set(label, (counts.get(label) ?? 0) + 1);
    }
  }
  const summary = [...counts.entries()].map(([label, n]) => `${n} ${label}`).join(" · ");
  return (
    <nav aria-label="Changed files" className="flex flex-col gap-2">
      <div className="flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <SectionHeading>Changed files</SectionHeading>
        <span className="text-xs text-faint">
          {summary}
          {unchangedCount > 0 ? ` · ${unchangedCount} unchanged` : null}
        </span>
      </div>
      <ul className="flex flex-col">
        {entries.map((entry, i) =>
          entry.kind === "unchanged" ? null : (
            <li key={entry.path}>
              <a
                href={`#file-${i}`}
                className="flex min-h-9 items-center gap-2 rounded px-2 py-1 hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
              >
                <Chip tone="neutral">{KIND_LABEL[entry.kind]}</Chip>
                <span className="truncate font-mono text-xs text-dim">{entry.path}</span>
              </a>
            </li>
          ),
        )}
      </ul>
    </nav>
  );
}
