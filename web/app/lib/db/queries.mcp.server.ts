import { and, asc, eq, sql } from "drizzle-orm";
import type { MemberActor, SessionActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
import type { PublishActor } from "@/lib/db/queries.custody.server";
import { bundle, mcpProbe } from "@/lib/db/schema.app";
import { planeCurrentPointer } from "@/lib/db/schema.custody";
import type { McpProbeOutcome, McpProbeRecord } from "@/lib/mcp/probe-state";

/**
 * The MCP half of the data access layer: which bundles in a workspace are `kind: 'mcp'` and
 * what each one's `current` points at. The DOCUMENT itself is not here — bytes live in the
 * vault and are read through the custody lane (app/lib/mcp/catalog.server.ts joins the two).
 *
 * Its one caller is workspace-scoped by the actor it carries: the registry lane serving this
 * workspace's catalog. (Embedded-name uniqueness is no longer a scan — it is an indexed claim,
 * app/lib/db/bundle-identity.server.ts.)
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
 * workspace catalog is tens of servers.
 *
 * Only the REGISTRY LANE scans documents now, and it serves what it can read. Uniqueness — the
 * one caller that needed a COMPLETE answer, and could only refuse when the catalog outgrew a
 * pass — is an indexed row (`web.bundle_identity`), which is what that older comment called the
 * honest fix.
 */
export const MAX_MCP_BUNDLES_SCANNED = 500;

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/** The one client a scan's reads run on. The registry lane lets the shared pool answer; the
 *  parameter stays so a caller inside a transaction can read its own uncommitted rows. */
export type McpDbClient = ReturnType<typeof getDb> | Tx;

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

// ── The advisory probe's one row per version ────────────────────────────────────────────────

/** What one probe writes down. The vocabulary is the probe's own (app/lib/mcp/probe-state.ts). */
export interface McpProbeWrite {
  bundleId: string;
  versionId: string;
  outcome: McpProbeOutcome;
  detail: string | null;
}

/**
 * Record what the plane saw when it asked. Idempotent by (bundle, version): a re-probe of the same
 * immutable bytes REPLACES the older answer rather than accumulating a history nobody reads — the
 * question is "does this work now", and the row carries its own timestamp.
 *
 * NO AUDIT ROW, and deliberately: an audit line records what a PERSON did, and nobody did this.
 * The publish that preceded it is already audited; this is the plane's own observation of somebody
 * else's server, the same kind of write as the mail transport's metadata-only send log. The actor
 * is still required — it is what scopes the row to a workspace the caller actually stands in —
 * and it is the PUBLISH doors' actor, since the probe only ever follows one of their publishes.
 */
export async function recordMcpProbe(actor: PublishActor, write: McpProbeWrite): Promise<void> {
  await getDb()
    .insert(mcpProbe)
    .values({
      workspaceId: actor.workspaceId,
      bundleId: write.bundleId,
      versionId: write.versionId,
      outcome: write.outcome,
      detail: write.detail,
    })
    .onConflictDoUpdate({
      target: [mcpProbe.bundleId, mcpProbe.versionId],
      set: { outcome: write.outcome, detail: write.detail, probedAt: sql`now()` },
      // The bundle's own workspace, never the caller's word for it: the composite key is what
      // stops a row from being rewritten from another workspace's request.
      setWhere: eq(mcpProbe.workspaceId, actor.workspaceId),
    });
}

/** The probe recorded for one version, or null — which reads as "not checked yet". */
export async function mcpProbeFor(
  actor: MemberActor | SessionActor,
  bundleId: string,
  versionId: string,
): Promise<McpProbeRecord | null> {
  const rows = await getDb()
    .select({ outcome: mcpProbe.outcome, probedAt: mcpProbe.probedAt })
    .from(mcpProbe)
    .where(
      and(
        eq(mcpProbe.workspaceId, actor.workspaceId),
        eq(mcpProbe.bundleId, bundleId),
        eq(mcpProbe.versionId, versionId),
      ),
    )
    .limit(1);
  const row = rows[0];
  if (row === undefined) {
    return null;
  }
  return {
    outcome: row.outcome as McpProbeOutcome,
    probedAt: row.probedAt.toISOString(),
  };
}
