import { and, asc, eq, sql } from "drizzle-orm";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { auditInTx } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { bundleMcp, mcpGatewayOptout, mcpToolPolicy, mcpToolSelection } from "@/lib/db/schema.app";
import { gatewayCredential, gatewayObservedTool } from "@/lib/db/schema.gateway";

/**
 * THE GATEWAY'S HALF OF THE CONNECTED SERVER'S PAGE — three reads over the gateway's own schema
 * (SELECT-only, grant-enforced) and ONE write over this tier's policy tables.
 *
 * Callers must have established that a gateway is deployed (`gatewayLane() !== null`) BEFORE
 * calling anything here that reads the `gateway` schema: on an install with no gateway that
 * schema does not exist, and these reads would be an error rather than an empty answer. That
 * check belongs at the top of the loader, where it also decides whether the sections render at
 * all. The ROUTING writes further down are the named exception — web rows only, deployment
 * independent.
 *
 * Every function takes the branded MemberActor and scopes to `actor.workspaceId`, so a server id
 * from another workspace reads as nothing — the same answer an id naming no server gets. Reading a
 * sign-in's METADATA is a member-level fact (the page says whose account is in use); the secret
 * itself is not in this schema at all and cannot be read from this tier by any query.
 */

// ── Sign-in state ────────────────────────────────────────────────────────────────────────────

/** One stored sign-in, as the page needs it: never a secret, only who and when. */
export interface McpCredentialRow {
  credentialId: string;
  authKind: string;
  createdAt: Date;
  lastRefreshedAt: Date | null;
}

/**
 * WHICH SIGN-IN THIS VIEWER'S AGENTS WOULD USE, and what else stands. Resolution mirrors the
 * gateway's own: a person's own credential first, the workspace service account second — so the
 * state line the page renders is the answer a call would get, not a list of rows.
 */
export interface McpSignInState {
  /** The viewer's own sign-in for this server, or null. */
  mine: McpCredentialRow | null;
  /** The workspace service account's sign-in for this server, or null. */
  workspace: McpCredentialRow | null;
}

function credentialRowOf(row: {
  id: string;
  authKind: string;
  createdAt: Date;
  lastRefreshedAt: Date | null;
}): McpCredentialRow {
  return {
    credentialId: row.id,
    authKind: row.authKind,
    createdAt: row.createdAt,
    lastRefreshedAt: row.lastRefreshedAt,
  };
}

/**
 * The two credentials that can answer for this viewer on this server — theirs and the workspace's.
 * Both may be absent, which is the honest "nobody has connected one" state the page names.
 */
export async function mcpSignInState(
  actor: MemberActor,
  serverId: string,
): Promise<McpSignInState> {
  const rows = await getDb()
    .select({
      id: gatewayCredential.id,
      userId: gatewayCredential.userId,
      authKind: gatewayCredential.authKind,
      createdAt: gatewayCredential.createdAt,
      lastRefreshedAt: gatewayCredential.lastRefreshedAt,
    })
    .from(gatewayCredential)
    .where(
      and(
        eq(gatewayCredential.workspaceId, actor.workspaceId),
        eq(gatewayCredential.serverId, serverId),
        sql`(${gatewayCredential.userId} = ${actor.userId} OR ${gatewayCredential.userId} IS NULL)`,
      ),
    );
  const mine = rows.find((row) => row.userId === actor.userId);
  const workspace = rows.find((row) => row.userId === null);
  return {
    mine: mine === undefined ? null : credentialRowOf(mine),
    workspace: workspace === undefined ? null : credentialRowOf(workspace),
  };
}

/**
 * Resolve the credential id a DISCONNECT names, from the scope the form asked for — so a route
 * never takes an id from the browser. A scope with no credential answers null and the ceremony
 * refuses; the workspace scope's own owner gate is the caller's, run before this.
 */
export async function mcpCredentialIdFor(
  actor: MemberActor,
  serverId: string,
  scope: "mine" | "workspace",
): Promise<string | null> {
  const state = await mcpSignInState(actor, serverId);
  const row = scope === "mine" ? state.mine : state.workspace;
  return row === null ? null : row.credentialId;
}

// ── Observed tools + the policy they are checked against ────────────────────────────────────

/** One tool the gateway has seen this server offer, and whether policy enables it. */
export interface McpToolRow {
  name: string;
  description: string | null;
  /** Still in the server's most recent listing. */
  currentlyOffered: boolean;
  /** Enabled under `selected` mode (under `all` every tool is enabled and this is unread). */
  selected: boolean;
}

export interface McpToolsView {
  mode: "all" | "selected";
  tools: McpToolRow[];
  /** Names carried by the selection that the server no longer offers — kept, never shown as tools. */
  selectedButUnseen: string[];
}

/**
 * THE TOOLS PANEL'S ONE READ: the observed list merged with the workspace's selection. No policy
 * row means `all` — the state a connection has always had, so a workspace that never opened this
 * panel reads exactly as it behaves.
 */
export async function mcpToolsView(actor: MemberActor, serverId: string): Promise<McpToolsView> {
  const db = getDb();
  const [policyRows, selectionRows, observedRows] = await Promise.all([
    db
      .select({ mode: mcpToolPolicy.mode })
      .from(mcpToolPolicy)
      .where(
        and(eq(mcpToolPolicy.workspaceId, actor.workspaceId), eq(mcpToolPolicy.serverId, serverId)),
      )
      .limit(1),
    db
      .select({ toolName: mcpToolSelection.toolName })
      .from(mcpToolSelection)
      .where(
        and(
          eq(mcpToolSelection.workspaceId, actor.workspaceId),
          eq(mcpToolSelection.serverId, serverId),
        ),
      ),
    db
      .select({
        name: gatewayObservedTool.name,
        description: gatewayObservedTool.description,
        currentlyOffered: gatewayObservedTool.currentlyOffered,
      })
      .from(gatewayObservedTool)
      .where(
        and(
          eq(gatewayObservedTool.workspaceId, actor.workspaceId),
          eq(gatewayObservedTool.serverId, serverId),
        ),
      )
      .orderBy(asc(gatewayObservedTool.name)),
  ]);
  const selected = new Set(selectionRows.map((row) => row.toolName));
  const observed = new Set(observedRows.map((row) => row.name));
  return {
    mode: policyRows[0]?.mode === "selected" ? "selected" : "all",
    tools: observedRows.map((row) => ({
      name: row.name,
      description: row.description,
      currentlyOffered: row.currentlyOffered,
      selected: selected.has(row.name),
    })),
    selectedButUnseen: [...selected].filter((name) => !observed.has(name)).sort(),
  };
}

export type ToolPolicyOutcome = "saved" | "not_connected" | "empty_selection";

/**
 * SET THE POLICY — member-writable, and one transaction: the policy row is upserted, the selection
 * is replaced wholesale by what the form posted, and the audit row lands beside them.
 *
 * Replacing rather than diffing is deliberate: the checklist IS the answer, so a tool unchecked
 * anywhere in the form is unchecked in the table, and a concurrent save is a last-writer-wins on a
 * complete state rather than two half-applied edits. A server this workspace is not connected to
 * has no policy to set — the composite FK refuses it, and that refusal is typed back rather than
 * thrown.
 *
 * ONE STATE IS REFUSED: `selected` with nothing selected. It is spellable, and it means every tool
 * on the server is off — which is a thing a person may well want, but never a thing they meant by
 * saving an empty checklist. It reads as "narrow this" and lands as "switch this server off", so
 * the write refuses and the caller says why. Turning a whole server off is disconnecting it.
 */
export async function setMcpToolPolicy(
  actor: MemberActor,
  serverId: string,
  next: { mode: "all" | "selected"; tools: readonly string[] },
): Promise<ToolPolicyOutcome> {
  const wanted = [...new Set(next.tools)].sort();
  if (next.mode === "selected" && wanted.length === 0) {
    return "empty_selection";
  }
  return await getDb().transaction(async (tx) => {
    const connected = await tx.execute(sql`
      SELECT 1 FROM web.bundle_mcp
      WHERE workspace_id = ${actor.workspaceId} AND server_id = ${serverId}
      LIMIT 1
    `);
    if (connected.rows.length === 0) {
      return "not_connected";
    }
    await tx
      .insert(mcpToolPolicy)
      .values({
        workspaceId: actor.workspaceId,
        serverId,
        mode: next.mode,
        updatedBy: actor.userId,
      })
      .onConflictDoUpdate({
        target: [mcpToolPolicy.workspaceId, mcpToolPolicy.serverId],
        set: { mode: next.mode, updatedBy: actor.userId, updatedAt: new Date() },
      });
    await tx
      .delete(mcpToolSelection)
      .where(
        and(
          eq(mcpToolSelection.workspaceId, actor.workspaceId),
          eq(mcpToolSelection.serverId, serverId),
        ),
      );
    if (wanted.length > 0) {
      await tx.insert(mcpToolSelection).values(
        wanted.map((toolName) => ({
          workspaceId: actor.workspaceId,
          serverId,
          toolName,
        })),
      );
    }
    await auditInTx(tx, {
      workspaceId: actor.workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "mcp_tools_set",
      subject: serverId,
      outcome: "ok",
      // Counts, never the names: the trail says how far a policy narrowed, and the rows say which.
      details: { mode: next.mode, selected: wanted.length },
    });
    return "saved";
  });
}

// ── Routing — the connection's mandate and the member's own choice ──────────────────────────
//
// These two write WEB rows only (the connection's `gateway_policy` and the member's opt-out) and
// are deliberately deployment-independent: routing is durable configuration, meaningful before a
// gateway exists and after one is removed, so neither asks `gatewayLane()` first. Delivery reads
// both; the gateway's resolver enforces the mandate half.

export type GatewayPolicyOutcome = "saved" | "not_connected";

/**
 * THE OWNER'S ROUTING MANDATE for one connection: 'auto' (the NULL column — route where a
 * gateway is deployed and a sign-in stands, member's choice honored), 'direct' (never the
 * gateway, for anyone), 'required' (always the gateway, for everyone — a machine that cannot be
 * handed a gateway address receives no entry at all). One transaction: the column, then the
 * audit row.
 */
export async function setMcpGatewayPolicy(
  actor: OwnerActor,
  serverId: string,
  value: "auto" | "direct" | "required",
): Promise<GatewayPolicyOutcome> {
  return await getDb().transaction(async (tx) => {
    const updated = await tx
      .update(bundleMcp)
      .set({ gatewayPolicy: value === "auto" ? null : value })
      .where(and(eq(bundleMcp.workspaceId, actor.workspaceId), eq(bundleMcp.serverId, serverId)))
      .returning({ serverId: bundleMcp.serverId });
    if (updated.length === 0) {
      return "not_connected";
    }
    await auditInTx(tx, {
      workspaceId: actor.workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "mcp_gateway_policy",
      subject: serverId,
      outcome: "ok",
      details: { policy: value },
    });
    return "saved";
  });
}

export type GatewayRouteOutcome = "saved" | "not_connected";

/**
 * THE MEMBER'S OWN ROUTE for one connection — gateway (the default: no row) or direct (a row).
 * Self-scoped by construction: the row it writes or deletes is the actor's own, and it changes
 * which document THEIR machines are delivered, nothing anyone else receives. Setting the state
 * it already has is a no-op that still answers "saved" — the answer is the state, not the write.
 */
export async function setMcpGatewayRoute(
  actor: MemberActor,
  serverId: string,
  useGateway: boolean,
): Promise<GatewayRouteOutcome> {
  return await getDb().transaction(async (tx) => {
    const connected = await tx.execute(sql`
      SELECT 1 FROM web.bundle_mcp
      WHERE workspace_id = ${actor.workspaceId} AND server_id = ${serverId}
      LIMIT 1
    `);
    if (connected.rows.length === 0) {
      return "not_connected";
    }
    if (useGateway) {
      await tx
        .delete(mcpGatewayOptout)
        .where(
          and(
            eq(mcpGatewayOptout.workspaceId, actor.workspaceId),
            eq(mcpGatewayOptout.serverId, serverId),
            eq(mcpGatewayOptout.userId, actor.userId),
          ),
        );
    } else {
      await tx
        .insert(mcpGatewayOptout)
        .values({ workspaceId: actor.workspaceId, serverId, userId: actor.userId })
        .onConflictDoNothing();
    }
    await auditInTx(tx, {
      workspaceId: actor.workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "mcp_gateway_route",
      subject: serverId,
      outcome: "ok",
      details: { use_gateway: useGateway },
    });
    return "saved";
  });
}

// ── Usage ────────────────────────────────────────────────────────────────────────────────────

/** One call through the gateway, as the Usage table renders it. */
export interface McpUsageRow {
  id: number;
  /** Who called — the display of the person whose session it was, or a stand-in for a gone account. */
  person: string;
  /** Which machine — the session's own display label. */
  machine: string;
  toolName: string | null;
  method: string;
  outcome: string;
  createdAtMs: number;
}

export interface McpUsageWindow {
  events: McpUsageRow[];
  /** True when older calls exist beyond the window — recorded, just not shown. */
  hasMore: boolean;
}

/** The usage window's ceiling — the audit reader's number, for the same reason. */
export const MCP_USAGE_MAX_LIMIT = 100;

/**
 * THE LAST CALLS through the gateway for one server, newest first, bounded with the same +1 probe
 * the audit windows use so a full window can honestly say there are older ones. The person and the
 * machine are joined from THIS tier's rows — the gateway records ids, and displays are ours.
 */
export async function mcpUsageWindow(
  actor: MemberActor,
  serverId: string,
  opts: { limit?: number } = {},
): Promise<McpUsageWindow> {
  const limit = Math.min(Math.max(opts.limit ?? MCP_USAGE_MAX_LIMIT, 1), MCP_USAGE_MAX_LIMIT);
  const rows = await getDb().execute(sql`
    SELECT e.id, e.tool_name, e.method, e.outcome,
           (extract(epoch from e.created_at) * 1000)::bigint AS created_at_ms,
           -- The display rule, spelled the way every other raw-SQL reader here spells it: the
           -- profile name, else the email, else a stand-in for an account that is gone.
           COALESCE(NULLIF(btrim(u.name), ''), u.email, 'former member') AS person,
           COALESCE(s.display_name, 'a removed machine') AS machine
    FROM gateway.usage_event e
    LEFT JOIN web."user" u ON u.id = e.user_id
    LEFT JOIN web.cli_session s ON s.id = e.session_id
    WHERE e.workspace_id = ${actor.workspaceId} AND e.server_id = ${serverId}
    ORDER BY e.id DESC
    LIMIT ${limit + 1}
  `);
  const all = (rows.rows as Record<string, unknown>[]).map((row) => ({
    id: Number(row.id),
    person: row.person as string,
    machine: row.machine as string,
    toolName: (row.tool_name as string | null) ?? null,
    method: row.method as string,
    outcome: row.outcome as string,
    createdAtMs: Number(row.created_at_ms),
  }));
  const hasMore = all.length > limit;
  return { events: hasMore ? all.slice(0, limit) : all, hasMore };
}
