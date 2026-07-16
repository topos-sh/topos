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
    if (
      typeof workspaceId !== "string" ||
      workspaceId.length === 0 ||
      typeof storedBytes !== "number" ||
      !Number.isSafeInteger(storedBytes) ||
      storedBytes < 0
    ) {
      throw new Error("storage stats body is malformed");
    }
    stats.set(workspaceId, storedBytes);
  }
  return stats;
}
