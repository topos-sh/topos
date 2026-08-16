import { vaultFetch } from "./client.server";

/**
 * The per-workspace storage stat — the vault's operational accounting read, keyed by the same
 * opaque workspace ids this tier minted (the vault knows numbers, never identity). Counts
 * 'present' custody only, vault-side. No OSS page consumes this yet — a downstream composition
 * displays it.
 *
 * Failures THROW with a fixed, credential-free message (this is a server-side accounting read,
 * not a page's PlaneResult surface), and parsing is defensive: a malformed body is an error,
 * never a NaN entry.
 */
export async function storageStats(): Promise<Map<string, number>> {
  const res = await vaultFetch({ method: "GET", template: "/internal/v1/storage" });
  if (!res.ok) {
    throw new Error(`storage stats read failed (status ${res.status})`);
  }
  let body: unknown;
  try {
    body = await res.json();
  } catch {
    throw new Error("storage stats read returned a non-JSON body");
  }
  return parseStorageStats(body);
}

/**
 * How long one stat read answers every quota check before the vault is asked again. The
 * admission is bounded-overshoot by design (storage-quota.server.ts), so a briefly stale
 * total only widens that bound by what lands within the window — and without it, a
 * multi-tenant store would pay the vault's whole-store accounting scan on EVERY capped
 * ingest (twice on a genesis: the route door and the shared genesis door both ask). A
 * workspace-scoped vault read would retire both the cache and the app-side filter.
 */
const STORED_BYTES_TTL_MS = 10_000;
let statCache: { at: number; stats: Map<string, number> } | null = null;

/**
 * ONE workspace's stored bytes, for the publish-ingest quota check — the same vault stat,
 * filtered app-side (the vault knows numbers, never identity; a workspace with no custody yet
 * simply has no entry, which is 0) and cached for a few seconds (above). FAIL-OPEN BY DESIGN:
 * a stat failure returns `null` and the caller allows — the ingest shares the same backend and
 * will fail on a real outage — but the failure is logged, because a quota that silently
 * stopped being enforced is a fact an operator needs. Failures are never cached.
 */
export async function workspaceStoredBytes(workspaceId: string): Promise<number | null> {
  try {
    if (statCache === null || Date.now() - statCache.at > STORED_BYTES_TTL_MS) {
      statCache = { at: Date.now(), stats: await storageStats() };
    }
    return statCache.stats.get(workspaceId) ?? 0;
  } catch (error) {
    // The deliberate fail-open trace (message only — storageStats errors are fixed,
    // credential-free strings).
    console.error(
      `storage quota check skipped (stat read failed): ${error instanceof Error ? error.message : "unknown"}`,
    );
    return null;
  }
}

/** Strict shape parse: `{workspaces: [{workspace_id, stored_bytes}]}`, nothing looser. */
function parseStorageStats(body: unknown): Map<string, number> {
  if (typeof body !== "object" || body === null || !("workspaces" in body)) {
    throw new Error("storage stats body is malformed");
  }
  const workspaces = (body as { workspaces: unknown }).workspaces;
  if (!Array.isArray(workspaces)) {
    throw new Error("storage stats body is malformed");
  }
  const stats = new Map<string, number>();
  for (const entry of workspaces) {
    if (typeof entry !== "object" || entry === null) {
      throw new Error("storage stats body is malformed");
    }
    const { workspace_id: workspaceId, stored_bytes: storedBytes } = entry as {
      workspace_id?: unknown;
      stored_bytes?: unknown;
    };
    // The vault's totals are u64: past 2^53 JSON.parse already rounded the value, which is
    // fine for an oversight display — accept any non-negative finite integer-valued number
    // (display-grade precision) rather than refusing a workspace merely for being huge.
    if (
      typeof workspaceId !== "string" ||
      workspaceId.length === 0 ||
      typeof storedBytes !== "number" ||
      !Number.isFinite(storedBytes) ||
      !Number.isInteger(storedBytes) ||
      storedBytes < 0
    ) {
      throw new Error("storage stats body is malformed");
    }
    stats.set(workspaceId, storedBytes);
  }
  return stats;
}
