import type { MemberActor, SessionActor } from "@/lib/auth/guards.server";
import { identitiesByBundle } from "@/lib/db/bundle-identity.server";
import {
  MAX_MCP_BUNDLES_SCANNED,
  type McpBundleRow,
  type McpDbClient,
  mcpBundlesWithCurrent,
} from "@/lib/db/queries.mcp.server";
import { MAX_SERVER_JSON_BYTES, validateServerJson } from "@/lib/mcp/validate.server";
import { custodyObjectCapped, custodyVersionMeta } from "@/lib/plane/reads.server";

/**
 * WHAT THE WORKSPACE'S MCP SERVERS SAY THEY ARE — the join between the catalog rows (which
 * bundles are `kind: 'mcp'` and where their `current` points) and the bytes the vault holds
 * for those versions. Two callers need it and need it identically:
 *
 *  · the REGISTRY LANE, which serves each document in the official read-API envelope, and
 *  · the backfill that records what an already-published server calls itself.
 *
 * A version is immutable and content-addressed, so the parsed document is cached per
 * (workspace, bundle, version) and a hit can never be stale — the same reasoning the version
 * metadata cache runs on. That is what keeps a scan of the whole catalog cheap enough to do on
 * a whole-catalog pass cheap.
 */

/** One server the workspace publishes: the catalog row plus the document it currently holds. */
export interface McpCatalogEntry {
  row: McpBundleRow;
  /** The parsed document, exactly as stored (the registry lane serves this object verbatim). */
  server: Record<string, unknown>;
  /** The embedded registry name (`server.name`) — the identity the read API keys on. */
  serverName: string;
}

/** The result of one capped pass over the catalog. */
export interface McpCatalogScan {
  entries: McpCatalogEntry[];
}

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
  // a publish and a read, and the lane must never serve a document today's rules would refuse.
  const validated = validateServerJson(blob.data);
  if (!validated.ok) {
    return null;
  }
  const parsed = JSON.parse(new TextDecoder().decode(blob.data)) as Record<string, unknown>;
  cacheSet(key, parsed);
  return parsed;
}

/**
 * One capped pass over the workspace's published MCP servers. Rows whose document cannot be read
 * are DROPPED: the registry lane serves what it can prove.
 *
 * `db` is the client the CATALOG reads run on; the vault reads below are HTTP, not pool clients.
 */
export async function scanMcpCatalog(
  actor: MemberActor | SessionActor,
  limit = MAX_MCP_BUNDLES_SCANNED,
  db?: McpDbClient,
): Promise<McpCatalogScan> {
  const ws = actor.workspaceId;
  const rows = await mcpBundlesWithCurrent(actor, limit, db);
  const entries: McpCatalogEntry[] = [];
  for (const row of rows) {
    const server = await serverDocumentOf(ws, row.bundleId, row.versionId);
    if (server === null || typeof server.name !== "string") {
      continue;
    }
    entries.push({ row, server, serverName: server.name });
  }
  return { entries };
}

/**
 * Decorate the session-lane catalog listing with each ACTIVE `kind: 'mcp'` entry's EMBEDDED
 * server name — read from the RECORDED claim (`web.bundle_identity`), which is the same name the
 * registry lane serves and the one the workspace holds it against. It used to be what let the
 * CLI resolve a registry-shaped `add` against the connected workspaces before the official
 * registry; that door is gone, and NO client reads the field today — it stands for
 * registry-shape consumers of this listing.
 *
 * One indexed read for the whole listing, no vault round-trips and no per-pass cap: the claim
 * IS the recorded answer. Additive by design — an entry with no recorded claim (a server whose
 * document was unreadable when its name was last due to be recorded) is left undecorated, and
 * the wire field is optional.
 */
export async function withMcpServerNames<
  T extends { skill_id: string; kind: string; status: string; version_id: string },
>(ws: string, entries: T[]): Promise<(T & { mcp_server_name?: string })[]> {
  if (!entries.some((entry) => entry.kind === "mcp" && entry.status === "active")) {
    return entries;
  }
  const identities = await identitiesByBundle(ws, "mcp");
  return entries.map((entry) => {
    if (entry.kind !== "mcp" || entry.status !== "active") {
      return entry;
    }
    const name = identities.get(entry.skill_id);
    return name === undefined ? entry : { ...entry, mcp_server_name: name };
  });
}
