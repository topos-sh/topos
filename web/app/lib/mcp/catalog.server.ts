import { MAX_SERVER_JSON_BYTES, validateServerJson } from "@/lib/mcp/validate.server";
import { custodyObjectCapped, custodyVersionMeta } from "@/lib/plane/reads.server";

/**
 * WHAT A VERSION IN THE VAULT SAYS A SERVER IS — the one reader of an MCP bundle's stored bytes.
 *
 * ONE CALLER REMAINS, and it is a migration: the backfill that connects every MCP bundle written
 * before the server catalog existed to the row it names. The name it needs lives inside those
 * bytes, and bytes live behind an HTTP lane no statement can reach — so the reading happens here,
 * and everything downstream of it reads rows.
 *
 * A version is immutable and content-addressed, so the parsed document is cached per
 * (workspace, bundle, version) and a hit can never be stale — the same reasoning the version
 * metadata cache runs on.
 */

const CACHE_CAP = 500;
/** `${ws}\n${bundleId}\n${versionId}` → the parsed document (immutable by construction). */
const documents = new Map<string, Record<string, unknown>>();

function cacheGet(key: string): Record<string, unknown> | undefined {
  const hit = documents.get(key);
  if (hit !== undefined) {
    documents.delete(key);
    documents.set(key, hit);
  }
  return hit;
}

function cacheSet(key: string, value: Record<string, unknown>): void {
  documents.delete(key);
  documents.set(key, value);
  if (documents.size > CACHE_CAP) {
    const oldest = documents.keys().next().value;
    if (oldest !== undefined) {
      documents.delete(oldest);
    }
  }
}

/** The document a version stores, or null when the version holds none this tier can read. */
export async function serverDocumentOf(
  ws: string,
  bundleId: string,
  versionId: string,
): Promise<Record<string, unknown> | null> {
  const key = `${ws}\n${bundleId}\n${versionId}`;
  const cached = cacheGet(key);
  if (cached !== undefined) {
    return cached;
  }
  const meta = await custodyVersionMeta(ws, bundleId, versionId);
  if (!meta.ok) {
    return null;
  }
  // `server.json` at the bundle ROOT is the whole file set an MCP bundle is allowed; anything
  // else is a bundle that was not born through this gate.
  const file = meta.data.files.find((f) => f.path === "server.json");
  if (file === undefined) {
    return null;
  }
  const blob = await custodyObjectCapped(ws, bundleId, file.object_id, MAX_SERVER_JSON_BYTES);
  if (!blob.ok) {
    return null;
  }
  // Re-validated on the way OUT, not just on the way in: the gate's rules can tighten between
  // a publish and a read, and the lane must never serve a document today's rules would refuse. A
  // missing version is NOT such a refusal — the backfill files these as our own (`owner`) rows, and
  // a self-authored document may honestly carry none.
  const validated = validateServerJson(blob.data, { requireVersion: false });
  if (!validated.ok) {
    return null;
  }
  const parsed = JSON.parse(new TextDecoder().decode(blob.data)) as Record<string, unknown>;
  cacheSet(key, parsed);
  return parsed;
}
