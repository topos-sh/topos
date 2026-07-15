import type { CustodyVersionMeta } from "./wire";

/**
 * In-process LRU for version metadata. A version is an immutable, content-addressed snapshot,
 * so a hit can never be stale. Keys are (workspace, bundle, version_id) ONLY — no credential
 * or acting identity may ever appear in a cache key.
 */
const CAP = 500;

const entries = new Map<string, CustodyVersionMeta>();

export function versionCacheKey(ws: string, skill: string, versionId: string): string {
  // A newline can't appear in an id, so the composite key is unambiguous.
  return `${ws}\n${skill}\n${versionId}`;
}

export function versionCacheGet(key: string): CustodyVersionMeta | undefined {
  const value = entries.get(key);
  if (value !== undefined) {
    // Refresh recency: Map iterates in insertion order, so re-insert moves it to the back.
    entries.delete(key);
    entries.set(key, value);
  }
  return value;
}

export function versionCacheSet(key: string, value: CustodyVersionMeta): void {
  entries.delete(key);
  entries.set(key, value);
  if (entries.size > CAP) {
    const oldest = entries.keys().next().value;
    if (oldest !== undefined) {
      entries.delete(oldest);
    }
  }
}
