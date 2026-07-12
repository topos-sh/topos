import { Link } from "react-router";
import { Card, Chip } from "@/components/ui";
import type { ListingEntry } from "@/lib/view/tree";

/**
 * A version's files as a flat, pre-ordered tree (dirs-first per level, lexicographic — the order
 * buildListing already fixed). Directory rows are inert labels; file rows are whole-row links
 * into the file view. Depth becomes a left indent (16px per level) via an inline padding —
 * Tailwind can't express a per-row dynamic step, and the value is a trusted small integer off
 * the manifest, never user bytes. `skill` is the catalog NAME (the URL key).
 */
export function FileListing({
  ws,
  skill,
  versionId,
  entries,
}: {
  ws: string;
  skill: string;
  versionId: string;
  entries: readonly ListingEntry[];
}) {
  if (entries.length === 0) {
    return (
      <Card className="px-4 py-3">
        <p className="text-faint text-sm">This version carries no files.</p>
      </Card>
    );
  }
  return (
    <Card>
      <ul>
        {entries.map((entry) => (
          <ListingRow key={entry.path} ws={ws} skill={skill} versionId={versionId} entry={entry} />
        ))}
      </ul>
    </Card>
  );
}

function ListingRow({
  ws,
  skill,
  versionId,
  entry,
}: {
  ws: string;
  skill: string;
  versionId: string;
  entry: ListingEntry;
}) {
  // A 16px base plus one 16px step per tree level.
  const indent = { paddingLeft: `${16 + entry.depth * 16}px` };

  if (entry.kind === "dir") {
    return (
      <li
        style={indent}
        className="flex items-center border-line-soft border-b py-2 pr-4 font-mono text-[13px] text-faint last:border-b-0"
      >
        {entry.name}/
      </li>
    );
  }

  // Re-encode each segment so a name with a URL-unsafe character round-trips through the
  // catch-all route unharmed.
  const encoded = entry.path.split("/").map(encodeURIComponent).join("/");
  const href = `/workspaces/${ws}/skills/${skill}/versions/${versionId}/files/${encoded}`;
  return (
    <li className="border-line-soft border-b last:border-b-0">
      <Link
        to={href}
        style={indent}
        className="flex min-h-9 items-center gap-2 py-2 pr-4 font-mono text-[13px] text-dim transition-colors hover:bg-panel2 hover:text-ink focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
      >
        <span className="min-w-0 truncate">{entry.name}</span>
        {entry.mode === "100755" && <Chip tone="neutral">executable</Chip>}
      </Link>
    </li>
  );
}
