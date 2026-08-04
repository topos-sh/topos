import { and, asc, eq, sql } from "drizzle-orm";
import type { MemberActor, SessionActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
import { bundle } from "@/lib/db/schema.app";
import { planeCurrentPointer } from "@/lib/db/schema.custody";

/**
 * The MCP half of the data access layer: which bundles in a workspace are `kind: 'mcp'` and
 * what each one's `current` points at. The DOCUMENT itself is not here — bytes live in the
 * vault and are read through the custody lane (app/lib/mcp/catalog.server.ts joins the two).
 *
 * Both callers are workspace-scoped by the actor they carry: the registry lane serving this
 * workspace's catalog, and the publish gate proving no two bundles claim the same embedded
 * server name.
 */

/** One catalog row a `kind: 'mcp'` bundle contributes, with the version its `current` names. */
export interface McpBundleRow {
  bundleId: string;
  /** The catalog name — the workspace-local handle, not the document's embedded name. */
  name: string;
  versionId: string;
  /** When `current` last moved (epoch-ms) — the registry envelope's `updatedAt`. */
  updatedAtMs: number;
}

/**
 * A ceiling on how many MCP bundles one pass reads documents for. Every row past this costs a
 * vault round-trip pair (version meta + the `server.json` object), warmed by an immutable
 * per-version cache, so the cap bounds the WORST case rather than an expected size — a real
 * workspace catalog is tens of servers. Callers that need a COMPLETE answer (the embedded-name
 * uniqueness gate) compare against `mcpBundleCount` and refuse rather than guess when the
 * catalog outgrows one pass; the honest fix at that scale is an indexed embedded name, not a
 * bigger scan.
 */
export const MAX_MCP_BUNDLES_SCANNED = 500;

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/**
 * The one client a scan's reads run on. Every caller that scans UNDER the advisory lock passes
 * the transaction that holds it: the lock-holder's reads must ride the client it already owns —
 * a second `getDb()` checkout there is a pool client acquired while one is held, and N
 * concurrent publishes of one workspace would exhaust the pool with the lock-holder starved
 * behind its own waiters. Absent (the unlocked pre-checks, the registry lane), the shared pool
 * answers.
 */
export type McpDbClient = ReturnType<typeof getDb> | Tx;

/**
 * Serialize this workspace's embedded-name decisions. The uniqueness rule reads BYTES (a
 * version's document says what name it claims), so there is no column to put a unique index on
 * — the lock is what makes "scan, then register" atomic against another publish doing the same
 * thing at the same moment. Every door that ends up REGISTERING or RESTORING an `mcp` bundle
 * takes it inside its final transaction and re-runs the scan under it — ON the same transaction
 * client (see [`McpDbClient`]); a transaction-scoped lock needs no release path (a rollback
 * drops it with everything else).
 */
export async function lockMcpNamesInTx(tx: Tx, workspaceId: string): Promise<void> {
  await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${workspaceId} || ':mcp-name'))`);
}

/**
 * Every ACTIVE `kind: 'mcp'` bundle in the actor's workspace that has something published,
 * catalog-name order, capped. An archived or never-published bundle is absent: it delivers
 * nothing, so it neither serves on the registry lane nor holds a name against a new publish.
 */
export async function mcpBundlesWithCurrent(
  actor: MemberActor | SessionActor,
  limit = MAX_MCP_BUNDLES_SCANNED,
  db: McpDbClient = getDb(),
): Promise<McpBundleRow[]> {
  const ws = actor.workspaceId;
  const rows = await db
    .select({
      bundleId: bundle.id,
      name: bundle.name,
      versionId: planeCurrentPointer.versionId,
      updatedAtMs: sql<string>`(extract(epoch from ${planeCurrentPointer.movedAt}) * 1000)::bigint`,
    })
    .from(bundle)
    .innerJoin(
      planeCurrentPointer,
      and(eq(planeCurrentPointer.workspaceId, ws), eq(planeCurrentPointer.bundleId, bundle.id)),
    )
    .where(and(eq(bundle.workspaceId, ws), eq(bundle.kind, "mcp"), eq(bundle.status, "active")))
    .orderBy(asc(bundle.name))
    .limit(limit);
  return rows.map((row) => ({
    bundleId: row.bundleId,
    name: row.name,
    versionId: row.versionId,
    updatedAtMs: Number(row.updatedAtMs),
  }));
}

/** How many published, active `kind: 'mcp'` bundles the workspace holds — the completeness
 * check a capped scan compares itself against. */
export async function mcpBundleCount(
  actor: MemberActor | SessionActor,
  db: McpDbClient = getDb(),
): Promise<number> {
  const rows = await db.execute(sql`
    SELECT count(*)::int AS n
    FROM web.bundle b
    JOIN plane.current_pointer cp
      ON cp.workspace_id = b.workspace_id AND cp.bundle_id = b.id
    WHERE b.workspace_id = ${actor.workspaceId} AND b.kind = 'mcp' AND b.status = 'active'
  `);
  return Number((rows.rows[0] as { n: number | string } | undefined)?.n ?? 0);
}
