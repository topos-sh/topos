import type { MemberActor, SessionActor } from "@/lib/auth/guards.server";
import {
  MAX_MCP_BUNDLES_SCANNED,
  type McpBundleRow,
  type McpDbClient,
  mcpBundleCount,
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
 *  · the PUBLISH GATE, which refuses a second bundle claiming a server name already taken here.
 *
 * A version is immutable and content-addressed, so the parsed document is cached per
 * (workspace, bundle, version) and a hit can never be stale — the same reasoning the version
 * metadata cache runs on. That is what keeps a scan of the whole catalog cheap enough to do on
 * every MCP publish.
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
  /**
   * True when the catalog holds more published MCP bundles than this pass read, or when a
   * document could not be read at all — either way the pass proves nothing about what it did
   * NOT see, and a caller asking "is this name free" must refuse rather than assume.
   */
  incomplete: boolean;
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
 * One capped pass over the workspace's published MCP servers. Rows whose document cannot be
 * read are DROPPED from the entries and mark the pass incomplete — the registry lane serves
 * what it can prove, and the uniqueness gate refuses what it cannot.
 *
 * `db` is the client the CATALOG reads run on. A caller scanning under `lockMcpNamesInTx` MUST
 * pass the transaction that holds the lock — the reads then ride the held client instead of
 * checking a second one out of the pool while the first sits locked (the pool-exhaustion shape).
 * The vault reads below are HTTP, not pool clients, and stay as they are. Note the two reads run
 * SEQUENTIALLY on a transaction client (one session, no pipelining) — the `Promise.all` here is
 * ordering, not parallelism, when `db` is a transaction.
 */
export async function scanMcpCatalog(
  actor: MemberActor | SessionActor,
  limit = MAX_MCP_BUNDLES_SCANNED,
  db?: McpDbClient,
): Promise<McpCatalogScan> {
  const ws = actor.workspaceId;
  const [rows, total] = await Promise.all([
    mcpBundlesWithCurrent(actor, limit, db),
    mcpBundleCount(actor, db),
  ]);
  let incomplete = total > rows.length;
  const entries: McpCatalogEntry[] = [];
  for (const row of rows) {
    const server = await serverDocumentOf(ws, row.bundleId, row.versionId);
    if (server === null || typeof server.name !== "string") {
      incomplete = true;
      continue;
    }
    entries.push({ row, server, serverName: server.name });
  }
  return { entries, incomplete };
}

export type McpNameCheck =
  | { kind: "free" }
  | { kind: "taken"; by: McpBundleRow }
  | { kind: "indeterminate" };

/**
 * Is `serverName` free in this workspace, ignoring `exceptBundleId` (the bundle being
 * republished — a new version of the same server is not a collision with itself)?
 *
 * Uniqueness is on the EMBEDDED name, not the catalog name, because that is the identity the
 * registry read API resolves: two bundles declaring `io.github.acme/weather` would make
 * `…/servers/io.github.acme%2Fweather/versions/latest` ambiguous, and an agent would get
 * whichever one sorted first. So the collision is refused at the door instead of resolved by
 * accident. An INDETERMINATE answer (a scan that could not see the whole catalog) is not a
 * free one — the gate fails toward the refusal.
 *
 * `db`: the locked re-checks pass their transaction so the scan reads on the HELD client (see
 * [`scanMcpCatalog`]); the unlocked pre-checks omit it.
 */
export async function mcpNameTaken(
  actor: MemberActor | SessionActor,
  serverName: string,
  exceptBundleId: string | null,
  db?: McpDbClient,
): Promise<McpNameCheck> {
  const scan = await scanMcpCatalog(actor, MAX_MCP_BUNDLES_SCANNED, db);
  const hit = scan.entries.find(
    (entry) => entry.serverName === serverName && entry.row.bundleId !== exceptBundleId,
  );
  if (hit !== undefined) {
    return { kind: "taken", by: hit.row };
  }
  return scan.incomplete ? { kind: "indeterminate" } : { kind: "free" };
}
