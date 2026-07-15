/**
 * Pure first-parent history walk. Zero IO: the fetcher is injected, so the walk is unit-testable
 * with plain stubs and the caller decides how metadata is actually fetched (in production the
 * skill page binds reads.custodyVersionMeta over its workspace + bundle id). Merge commits are
 * traversed along their FIRST parent (the spine); the full parent set still rides on each step
 * so the UI can mark merges.
 */

/** The minimal metadata shape the walk needs — structurally satisfied by CustodyVersionMeta. */
export interface HistoryMetaLike {
  version_id: string;
  parents: string[];
  author: string;
  message: string;
  files: readonly unknown[];
}

export type HistoryFetchResult = { ok: true; data: HistoryMetaLike } | { ok: false };

export type HistoryFetcher = (versionId: string) => Promise<HistoryFetchResult>;

export interface HistoryStep {
  versionId: string;
  author: string;
  message: string;
  /** The COMPLETE parent set (2 entries marks a merge); the walk follows parents[0]. */
  parents: string[];
  fileCount: number;
}

export interface HistoryPage {
  steps: HistoryStep[];
  /**
   * The next first-parent id when the walk stopped at the depth cap — pass as `from` to resume.
   * Null when the walk reached genesis or was truncated.
   */
  cursor: string | null;
  /** True when a mid-walk fetch failed or a cycle was detected: `steps` is what was reachable. */
  truncated: boolean;
}

export interface WalkOptions {
  /** Maximum number of steps to return (must be >= 1). */
  depth: number;
  /** Resume point (a cursor from a previous page); defaults to `headId`. */
  from?: string;
}

export async function walkHistory(
  fetchMeta: HistoryFetcher,
  headId: string,
  options: WalkOptions,
): Promise<HistoryPage> {
  const depth = Math.max(1, Math.floor(options.depth));
  const steps: HistoryStep[] = [];
  const seen = new Set<string>();
  let next: string | null = options.from ?? headId;

  while (next !== null && steps.length < depth) {
    if (seen.has(next)) {
      // A cycle can only mean corrupt input; stop honestly instead of looping.
      return { steps, cursor: null, truncated: true };
    }
    seen.add(next);
    const result = await fetchMeta(next);
    if (!result.ok) {
      return { steps, cursor: null, truncated: true };
    }
    const meta = result.data;
    steps.push({
      versionId: meta.version_id,
      author: meta.author,
      message: meta.message,
      parents: [...meta.parents],
      fileCount: meta.files.length,
    });
    next = meta.parents[0] ?? null;
  }

  return { steps, cursor: next, truncated: false };
}
