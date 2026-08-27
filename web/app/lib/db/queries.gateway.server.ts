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

// ── Is the gateway's schema actually there? ──────────────────────────────────────────────────

/** Memoized POSITIVE only: a schema that exists never stops existing, while an absent one may
 *  appear at any moment (the gateway migrates its own lineage at boot, and web may be up first). */
let gatewaySchemaSeen = false;

/**
 * Whether this tier can actually READ `gateway.credential` — the one gateway table the routing
 * ruling consults.
 *
 * `GATEWAY_PUBLIC_URL` is a deployment's INTENTION, not proof: a self-hoster can set it without
 * ever starting a gateway, and on any rolling deploy the web app can be serving before the
 * gateway container has run its lineage. Naming a relation that does not exist fails at PARSE
 * time, taking the whole read down — and since routing moved into the catalog and lock lanes,
 * that would be every project's MCP install, not just one machine's feed. So the question is
 * asked first, and its "no" means "route direct", which is the same shape an install with no
 * gateway has always had.
 *
 * `has_table_privilege` over `to_regclass` answers existence AND the grant in one strict call:
 * a missing relation makes the oid NULL, the function returns NULL, and the COALESCE says no.
 */
export async function gatewayCredentialsReadable(): Promise<boolean> {
  if (gatewaySchemaSeen) {
    return true;
  }
  const rows = await getDb().execute(sql`
    SELECT COALESCE(has_table_privilege(to_regclass('gateway.credential'), 'SELECT'), false)
             AS readable
  `);
  gatewaySchemaSeen = (rows.rows[0] as { readable: boolean } | undefined)?.readable === true;
  return gatewaySchemaSeen;
}

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

/**
 * ONE MACHINE'S WHOLE HISTORY with one server, as the Usage table renders it.
 *
 * The ledger is per-CALL, and a working agent makes hundreds of them — the raw list rendered a
 * hundred visually identical lines and told a reader nothing they could act on. A row here is one
 * SESSION (a machine's login), which is the unit a person can actually do something about: they
 * can find that machine, or end that session.
 */
export interface McpUsageSessionRow {
  /** The session — the row's identity, and what the Sessions page calls the same machine. */
  sessionId: string;
  /** Who called — the display of the person whose session it was, or a stand-in for a gone account. */
  person: string;
  /** Which machine — the session's own display label. */
  machine: string;
  /** Every call this session made through the gateway to this server. */
  calls: number;
  /** How many ended `ok`, and how many ended any other way. The two add up to `calls`. */
  ok: number;
  failed: number;
  /** WHY the failed ones failed — the ledger's own outcome names with their counts, biggest
   *  first. Empty when nothing failed, and it always sums to `failed`. "3 failed" alone is a
   *  number nobody can act on: a tool the workspace switched off and a server that is down are
   *  different problems with different fixes. */
  failures: { kind: string; count: number }[];
  /** The distinct tools this session called, alphabetical. EMPTY where the ledger holds only
   *  non-tool methods (initialize, a listing) — the table says so once per row rather than
   *  carrying a column of dashes. */
  tools: string[];
  firstCallMs: number;
  lastCallMs: number;
}

export interface McpUsagePage {
  sessions: McpUsageSessionRow[];
  /** 1-based, and already CLAMPED into range: a page number past the end reads the last page. */
  page: number;
  /** At least 1, even with nothing to show — a reader is never on "page 1 of 0". */
  pageCount: number;
  /** Every session that has ever called this server, not just the ones on this page. */
  total: number;
}

/** One page of the Usage table. A number, not a "recent window" — the ledger is reachable whole. */
export const MCP_USAGE_PAGE_SIZE = 25;

/** The per-kind failure counts, biggest first and ties broken by name so a row never reorders
 *  itself between two reads of the same data. */
function failureKindsOf(stored: unknown): { kind: string; count: number }[] {
  if (stored === null || typeof stored !== "object") {
    return [];
  }
  return Object.entries(stored as Record<string, number>)
    .map(([kind, count]) => ({ kind, count: Number(count) }))
    .sort((a, b) => b.count - a.count || a.kind.localeCompare(b.kind));
}

/**
 * THE USAGE TABLE'S ONE READ — every session that has called this server, one row each, newest
 * activity first, a page at a time.
 *
 * Grouping is by (session, person) rather than session alone: a session is minted for exactly one
 * person, so this is one row per session, and it can never blend two people's calls into a row
 * that names one of them. The person and the machine are joined from THIS tier's rows — the
 * gateway records ids, and displays are ours.
 *
 * A row with NO person is a MACHINE TOKEN's call (CI, calling with the workspace's sign-in): its
 * session id names a service session instead of a person's, so both joins are tried and the
 * person column says so outright. Grouping on a null user is why the per-failure join matches
 * with IS NOT DISTINCT FROM — plain equality drops every machine row's failure breakdown.
 *
 * The count is taken first so the page number can be clamped before the offset is spent: asking
 * for page 9 of a 3-page ledger reads page 3, never an empty table that looks like "no calls".
 */
export async function mcpUsageSessions(
  actor: MemberActor,
  serverId: string,
  opts: { page?: number } = {},
): Promise<McpUsagePage> {
  const requested =
    opts.page !== undefined && Number.isFinite(opts.page) ? Math.max(1, Math.floor(opts.page)) : 1;
  const counted = await getDb().execute(sql`
    SELECT count(*)::int AS total
    FROM (
      SELECT 1 FROM gateway.usage_event
      WHERE workspace_id = ${actor.workspaceId} AND server_id = ${serverId}
      GROUP BY session_id, user_id
    ) grouped
  `);
  const total = Number((counted.rows[0] as { total: number } | undefined)?.total ?? 0);
  const pageCount = Math.max(1, Math.ceil(total / MCP_USAGE_PAGE_SIZE));
  const page = Math.min(requested, pageCount);
  if (total === 0) {
    return { sessions: [], page, pageCount, total };
  }
  const rows = await getDb().execute(sql`
    WITH per_session AS (
      SELECT e.session_id, e.user_id,
             count(*)::int AS calls,
             count(*) FILTER (WHERE e.outcome = 'ok')::int AS ok,
             count(*) FILTER (WHERE e.outcome <> 'ok')::int AS failed,
             array_agg(DISTINCT e.tool_name ORDER BY e.tool_name)
               FILTER (WHERE e.tool_name IS NOT NULL) AS tools,
             min(e.created_at) AS first_at,
             max(e.created_at) AS last_at
      FROM gateway.usage_event e
      WHERE e.workspace_id = ${actor.workspaceId} AND e.server_id = ${serverId}
      GROUP BY e.session_id, e.user_id
    ),
    -- WHY the failures failed, counted per kind. The gateway owns the set of outcome names (its
    -- own CHECK constraint), so this reads whatever is there rather than naming them here: a kind
    -- added upstream shows up as itself instead of vanishing into a total.
    per_failure AS (
      SELECT session_id, user_id, jsonb_object_agg(outcome, n) AS kinds
      FROM (
        SELECT e.session_id, e.user_id, e.outcome, count(*)::int AS n
        FROM gateway.usage_event e
        WHERE e.workspace_id = ${actor.workspaceId} AND e.server_id = ${serverId}
          AND e.outcome <> 'ok'
        GROUP BY e.session_id, e.user_id, e.outcome
      ) counted
      GROUP BY session_id, user_id
    )
    SELECT g.session_id, g.calls, g.ok, g.failed, g.tools, f.kinds AS failure_kinds,
           (extract(epoch from g.first_at) * 1000)::bigint AS first_call_ms,
           (extract(epoch from g.last_at) * 1000)::bigint AS last_call_ms,
           -- The display rule, spelled the way every other raw-SQL reader here spells it: the
           -- profile name, else the email, else a stand-in for an account that is gone — or, for
           -- a call no person made, the plain truth instead of a phantom person.
           CASE WHEN g.user_id IS NULL THEN 'a machine token'
                ELSE COALESCE(NULLIF(btrim(u.name), ''), u.email, 'former member') END AS person,
           COALESCE(s.display_name, ss.display_name, 'a removed machine') AS machine
    FROM per_session g
    LEFT JOIN per_failure f
      ON f.session_id = g.session_id AND f.user_id IS NOT DISTINCT FROM g.user_id
    LEFT JOIN web."user" u ON u.id = g.user_id
    LEFT JOIN web.cli_session s ON s.id = g.session_id
    LEFT JOIN web.service_session ss ON ss.id = g.session_id
    ORDER BY g.last_at DESC, g.session_id DESC
    LIMIT ${MCP_USAGE_PAGE_SIZE} OFFSET ${(page - 1) * MCP_USAGE_PAGE_SIZE}
  `);
  const sessions = (rows.rows as Record<string, unknown>[]).map((row) => ({
    sessionId: row.session_id as string,
    person: row.person as string,
    machine: row.machine as string,
    calls: Number(row.calls),
    ok: Number(row.ok),
    failed: Number(row.failed),
    failures: failureKindsOf(row.failure_kinds),
    tools: (row.tools as string[] | null) ?? [],
    firstCallMs: Number(row.first_call_ms),
    lastCallMs: Number(row.last_call_ms),
  }));
  return { sessions, page, pageCount, total };
}
