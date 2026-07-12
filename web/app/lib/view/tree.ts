/**
 * Pure bundle-listing model for the skill file browser — no IO, no React. A version's flat file
 * list (bundle-relative forward-slash paths) becomes a flattened, pre-order tree listing the page
 * renders row by row, and `docFileOf` picks the prose file to show first. Mirrors the pure diff
 * modules: deterministic, unit-pinned, shared by loaders without touching bytes.
 */

/** One file as carried in a version's file list on the wire. */
export interface VersionFileRef {
  path: string;
  mode: string;
  object_id: string;
}

/** One row in the rendered listing — a synthesized directory or a real file. */
export interface ListingEntry {
  kind: "dir" | "file";
  /** The last path segment (the directory or file name). */
  name: string;
  /** The full bundle-relative path ("scripts" for a dir, "scripts/run.sh" for a file). */
  path: string;
  /** 0 at the bundle root, +1 per nesting level. */
  depth: number;
  /** Present only on files. */
  mode?: string;
  /** Present only on files. */
  objectId?: string;
}

interface TreeNode {
  dirs: Map<string, TreeNode>;
  files: VersionFileRef[];
}

function emptyNode(): TreeNode {
  return { dirs: new Map(), files: [] };
}

// Lexicographic by UTF-16 code unit — stable and locale-independent (no localeCompare surprises).
function byCodeUnit(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

function basenameOf(path: string): string {
  const slash = path.lastIndexOf("/");
  return slash === -1 ? path : path.slice(slash + 1);
}

/**
 * Build a flattened pre-order listing: at each level directories come first (sorted
 * lexicographically), then files (sorted lexicographically); a directory is immediately followed by
 * its whole subtree before the next sibling. Directory rows are synthesized from the file paths —
 * the wire only carries files.
 */
export function buildListing(files: readonly VersionFileRef[]): ListingEntry[] {
  const root = emptyNode();
  for (const file of files) {
    const segments = file.path.split("/");
    // Every segment but the last is a directory; for-of yields a non-undefined element type,
    // so the walk is index-access-free (satisfies noUncheckedIndexedAccess).
    const dirSegments = segments.slice(0, -1);
    let node = root;
    for (const seg of dirSegments) {
      let child = node.dirs.get(seg);
      if (child === undefined) {
        child = emptyNode();
        node.dirs.set(seg, child);
      }
      node = child;
    }
    node.files.push(file);
  }

  const out: ListingEntry[] = [];
  const walk = (node: TreeNode, prefix: string, depth: number): void => {
    const dirEntries = [...node.dirs.entries()].sort((a, b) => byCodeUnit(a[0], b[0]));
    for (const [name, child] of dirEntries) {
      const path = prefix === "" ? name : `${prefix}/${name}`;
      out.push({ kind: "dir", name, path, depth });
      walk(child, path, depth + 1);
    }
    const sortedFiles = [...node.files].sort((a, b) => byCodeUnit(a.path, b.path));
    for (const file of sortedFiles) {
      out.push({
        kind: "file",
        name: basenameOf(file.path),
        path: file.path,
        depth,
        mode: file.mode,
        objectId: file.object_id,
      });
    }
  };
  walk(root, "", 0);
  return out;
}

/**
 * The prose file to render first: root-level SKILL.md if present, else root-level README.md, else
 * undefined. Matched case-insensitively on the basename and ONLY at the root — "docs/SKILL.md" does
 * not count (it is nested, not the bundle's front-page doc).
 */
export function docFileOf(files: readonly VersionFileRef[]): VersionFileRef | undefined {
  const rootFiles = files.filter((f) => !f.path.includes("/"));
  return (
    rootFiles.find((f) => f.path.toLowerCase() === "skill.md") ??
    rootFiles.find((f) => f.path.toLowerCase() === "readme.md")
  );
}
