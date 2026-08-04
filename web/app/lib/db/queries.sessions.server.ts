import { and, asc, eq, inArray, sql } from "drizzle-orm";
import type { MemberActor, UserActor } from "@/lib/auth/guards.server";
import { sessionUnexpiredSql } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import type { ReportedHarnessState } from "@/lib/db/queries.lane.server";
import { bundle, cliSession, sessionBundleState, workspace } from "@/lib/db/schema.app";
import { planeCurrentPointer } from "@/lib/db/schema.custody";

/**
 * The SESSIONS data access layer — a session is user × workspace × installation, the ONE
 * credentialed principal. Two surfaces read it:
 *
 *  · the ACCOUNT page ("your sessions"): every session the signed-in person holds, across
 *    workspaces, each with a self-revoke arm;
 *  · the workspace SESSIONS page (a Settings tab, replacing the device fleet): every session
 *    in the workspace with per-bundle applied state, the pending-approval queue, and the
 *    owner arms (approve / reject / remove — the ceremonies live in identity.server.ts).
 *
 * Sessions are DELETED, never tombstoned; an ended session simply no longer appears (history
 * is the cause-tagged audit trail). Bytes already on a machine stay there — the page copy
 * says so.
 */

// ── The account page ("your sessions") ──────────────────────────────────────────────────────

export interface AccountSession {
  sessionId: string;
  displayName: string;
  workspaceId: string;
  workspaceName: string;
  workspaceDisplayName: string;
  status: "active" | "pending";
  createdAtMs: number;
  lastSeenAtMs: number | null;
  /** Past the workspace's owner-set expiry — the guard's own age predicate: the credential no
   * longer resolves, so the machine must log in again. */
  expired: boolean;
}

/** Every session the signed-in person holds, newest first. */
export async function sessionsFor(actor: UserActor): Promise<AccountSession[]> {
  const rows = await getDb()
    .select({
      sessionId: cliSession.id,
      displayName: cliSession.displayName,
      workspaceId: cliSession.workspaceId,
      workspaceName: workspace.name,
      workspaceDisplayName: workspace.displayName,
      status: cliSession.status,
      createdAtMs: sql<string>`(extract(epoch from ${cliSession.createdAt}) * 1000)::bigint`,
      lastSeenAtMs: sql<string>`(extract(epoch from ${cliSession.lastSeenAt}) * 1000)::bigint`,
      sessionMaxAgeMs: workspace.sessionMaxAgeMs,
    })
    .from(cliSession)
    .innerJoin(workspace, eq(workspace.id, cliSession.workspaceId))
    .where(eq(cliSession.userId, actor.userId))
    .orderBy(sql`${cliSession.createdAt} DESC`);
  const now = Date.now();
  return rows.map((r) => ({
    sessionId: r.sessionId,
    displayName: r.displayName,
    workspaceId: r.workspaceId,
    workspaceName: r.workspaceName,
    workspaceDisplayName: r.workspaceDisplayName,
    status: r.status as AccountSession["status"],
    createdAtMs: Number(r.createdAtMs),
    lastSeenAtMs: r.lastSeenAtMs === null ? null : Number(r.lastSeenAtMs),
    // The guard's own age predicate, mirrored per workspace policy.
    expired: r.sessionMaxAgeMs !== null && now - Number(r.createdAtMs) > r.sessionMaxAgeMs,
  }));
}

// ── The workspace sessions page ─────────────────────────────────────────────────────────────

/** How this session's copy of one bundle sits against the workspace's current pointer. */
export type SessionSkillStatus = "current" | "behind";

/** One (session × bundle) applied-state row, joined to the catalog and the current pointer. */
export interface SessionSkillState {
  skillId: string;
  /** The catalog name, or null when the id is no longer cataloged (a purged tombstone). */
  skillName: string | null;
  skillStatus: "active" | "archived" | "deleted" | null;
  /** The version this session last applied. */
  appliedVersionId: string;
  /** The workspace's current version, or null when nothing is published (or withdrawn). */
  currentVersionId: string | null;
  status: SessionSkillStatus;
  /** The per-harness applied states a CONFIG-placed ('mcp') bundle reports — which detected
   * agents hold the entry and how. NULL for a file bundle, whose one applied version says
   * everything. The state word is the client's, kept verbatim (an open vocabulary). */
  harnesses: ReportedHarnessState[] | null;
  /** When this row was last reported (epoch-ms). */
  reportedAtMs: number;
}

/** How fresh a session's last contact is against the workspace staleness window. */
export type SessionFreshness = "fresh" | "stale" | "never";

/** One session: its person, its status, its liveness, and its per-bundle applied state. */
export interface WorkspaceSession {
  sessionId: string;
  displayName: string;
  /** The owning person (display + login address — attribution, never an authority key). */
  ownerDisplay: string;
  ownerEmail: string;
  ownerUserId: string;
  /** 'pending' awaits an owner's approval (the session-approval knob). */
  status: "active" | "pending";
  /** When the session was minted (epoch-ms). */
  createdAtMs: number;
  /** The session's last-seen time (epoch-ms), or null when it has never phoned home. */
  lastSeenAtMs: number | null;
  /** Whether the session is past the workspace's owner-set expiry (the SAME age predicate the
   * session guard enforces): its credential no longer resolves, so the machine must log in
   * again. Always false when the policy is unset. */
  expired: boolean;
  freshness: SessionFreshness;
  /** The bundles this session last reported, catalog-name order. */
  skills: SessionSkillState[];
  /** Skills the session's OWNER has turned off on the web that this machine still holds —
   * names only, catalog order. Bytes already on a machine stay there until it reconciles, so
   * this is the honest gap between the decision and the disk, not an error. */
  declinedButApplied: string[];
}

export interface WorkspaceSessions {
  sessions: WorkspaceSession[];
  /** The workspace's staleness window (ms) — the ONE clock, never re-derived here. */
  stalenessWindowMs: number;
  /** The session-approval knob — 'on' means non-owner sessions are born pending. */
  sessionApproval: "off" | "on";
  /** The owner-set max session age (ms), or null when sessions do not expire. */
  sessionMaxAgeMs: number | null;
  /** Whether the actor sees ALL sessions (reviewer/owner) or only their own. */
  wholeWorkspace: boolean;
}

/** The stored jsonb block as the pages read it: a list, or null when the session reported none
 * (a file bundle). The column is the client's own word — shape-checked at the report door, so
 * this only re-establishes the type across the jsonb boundary. */
function harnessesOf(stored: unknown): ReportedHarnessState[] | null {
  return Array.isArray(stored) ? (stored as ReportedHarnessState[]) : null;
}

function freshnessOf(lastSeenAtMs: number | null, windowMs: number, now: number): SessionFreshness {
  if (lastSeenAtMs === null) {
    return "never";
  }
  return now - lastSeenAtMs <= windowMs ? "fresh" : "stale";
}

/**
 * The workspace's sessions for THIS actor — active AND pending (the page splits them). Role
 * scoping lives here: a plain member sees only their own sessions; a reviewer or owner sees
 * everyone's.
 */
export async function workspaceSessions(actor: MemberActor): Promise<WorkspaceSessions> {
  const ws = actor.workspaceId;
  const wholeWorkspace = actor.role !== "member";
  const now = Date.now();
  const db = getDb();

  const wsRows = await db
    .select({
      stalenessWindowMs: workspace.stalenessWindowMs,
      sessionApproval: workspace.sessionApproval,
      sessionMaxAgeMs: workspace.sessionMaxAgeMs,
    })
    .from(workspace)
    .where(eq(workspace.id, ws))
    .limit(1);
  const stalenessWindowMs = wsRows[0]?.stalenessWindowMs ?? 604800000;
  const sessionApproval = (wsRows[0]?.sessionApproval ?? "off") as "off" | "on";
  const sessionMaxAgeMs = wsRows[0]?.sessionMaxAgeMs ?? null;

  const sessionRows = await db.execute(sql`
    SELECT cs.id, cs.display_name, cs.user_id, cs.status,
           (extract(epoch from cs.created_at) * 1000)::bigint AS created_ms,
           (extract(epoch from cs.last_seen_at) * 1000)::bigint AS last_seen_ms,
           -- The display rule (app/lib/person-display.ts): a blank name falls back to the email.
           COALESCE(NULLIF(btrim(u.name), ''), u.email) AS owner_display, u.email AS owner_email
    FROM web.cli_session cs
    JOIN web."user" u ON u.id = cs.user_id
    WHERE cs.workspace_id = ${ws}
      AND (${wholeWorkspace} OR cs.user_id = ${actor.userId})
    ORDER BY u.email, cs.id
  `);
  const sessions = sessionRows.rows as {
    id: string;
    display_name: string;
    user_id: string;
    status: "active" | "pending";
    created_ms: string;
    last_seen_ms: string | null;
    owner_display: string;
    owner_email: string;
  }[];
  if (sessions.length === 0) {
    return { sessions: [], stalenessWindowMs, sessionApproval, sessionMaxAgeMs, wholeWorkspace };
  }

  const sessionIds = sessions.map((s) => s.id);
  const stateRows = await db
    .select({
      sessionId: sessionBundleState.sessionId,
      skillId: sessionBundleState.bundleId,
      appliedVersionId: sessionBundleState.appliedVersionId,
      harnessState: sessionBundleState.harnessState,
      reportedAtMs: sql<string>`(extract(epoch from ${sessionBundleState.reportedAt}) * 1000)::bigint`,
      skillName: bundle.name,
      skillStatus: bundle.status,
      currentVersionId: planeCurrentPointer.versionId,
    })
    .from(sessionBundleState)
    .innerJoin(bundle, and(eq(bundle.id, sessionBundleState.bundleId), eq(bundle.workspaceId, ws)))
    .leftJoin(
      planeCurrentPointer,
      and(
        eq(planeCurrentPointer.workspaceId, ws),
        eq(planeCurrentPointer.bundleId, sessionBundleState.bundleId),
      ),
    )
    .where(inArray(sessionBundleState.sessionId, sessionIds))
    .orderBy(asc(sessionBundleState.sessionId), asc(bundle.name));

  // The declined-but-still-applied gap: a bundle this session reports holding whose OWNER has
  // declined it. One query over the same session set, keyed by session so the page can say it
  // per machine.
  const declinedRows = await db.execute(sql`
    SELECT st.session_id, b.name
    FROM web.session_bundle_state st
    JOIN web.cli_session cs ON cs.id = st.session_id
    JOIN web.bundle b ON b.id = st.bundle_id AND b.workspace_id = ${ws}
    JOIN web.decline d ON d.workspace_id = ${ws} AND d.user_id = cs.user_id
                      AND d.bundle_id = st.bundle_id
    WHERE cs.workspace_id = ${ws} AND st.session_id IN (${sql.join(
      // Every id its own bind parameter: a bare JS array in a template renders as a
      // parenthesised list, which is not an array value and is refused by the server.
      sessionIds.map((id) => sql`${id}`),
      sql`, `,
    )})
    ORDER BY b.name
  `);
  const declinedBySession = new Map<string, string[]>();
  for (const raw of declinedRows.rows as { session_id: string; name: string }[]) {
    const list = declinedBySession.get(raw.session_id) ?? [];
    list.push(raw.name);
    declinedBySession.set(raw.session_id, list);
  }

  const statesBySession = new Map<string, SessionSkillState[]>();
  for (const row of stateRows) {
    const status: SessionSkillStatus =
      row.currentVersionId !== null && row.appliedVersionId === row.currentVersionId
        ? "current"
        : "behind";
    const state: SessionSkillState = {
      skillId: row.skillId,
      skillName: row.skillName,
      skillStatus: row.skillStatus as SessionSkillState["skillStatus"],
      appliedVersionId: row.appliedVersionId,
      currentVersionId: row.currentVersionId,
      status,
      harnesses: harnessesOf(row.harnessState),
      reportedAtMs: Number(row.reportedAtMs),
    };
    const list = statesBySession.get(row.sessionId);
    if (list === undefined) {
      statesBySession.set(row.sessionId, [state]);
    } else {
      list.push(state);
    }
  }

  return {
    sessions: sessions.map((s) => ({
      sessionId: s.id,
      displayName: s.display_name,
      ownerDisplay: s.owner_display,
      ownerEmail: s.owner_email,
      ownerUserId: s.user_id,
      status: s.status,
      createdAtMs: Number(s.created_ms),
      lastSeenAtMs: s.last_seen_ms === null ? null : Number(s.last_seen_ms),
      // The guard's own age predicate, mirrored: an over-age session's credential resolves to
      // nothing lane-side, so the page must never read it as a live machine.
      expired: sessionMaxAgeMs !== null && now - Number(s.created_ms) > sessionMaxAgeMs,
      freshness: freshnessOf(
        s.last_seen_ms === null ? null : Number(s.last_seen_ms),
        stalenessWindowMs,
        now,
      ),
      skills: statesBySession.get(s.id) ?? [],
      declinedButApplied: declinedBySession.get(s.id) ?? [],
    })),
    stalenessWindowMs,
    sessionApproval,
    sessionMaxAgeMs,
    wholeWorkspace,
  };
}

// ── The person's own machines (the skill page's applied state, the visibility page's proof) ──

/** One of the viewer's own sessions holding a named bundle. */
export interface AppliedOnSession {
  sessionId: string;
  /** The installation's own name — what the machine called itself when it logged in. */
  displayName: string;
  appliedVersionId: string;
  /** Whether that version is the workspace's `current`. */
  current: boolean;
  /** The per-harness applied states this machine reported for a config-placed ('mcp') bundle;
   * null for a file bundle. */
  harnesses: ReportedHarnessState[] | null;
  reportedAtMs: number;
}

/**
 * Which of the VIEWER'S OWN sessions hold one bundle, and at which version. Person-scoped by
 * construction (the actor's own user id) — a skill page tells you about your machines, never
 * anyone else's; the workspace-wide view is the Sessions page, whose role scoping is its own.
 */
export async function yourSessionsApplying(
  actor: MemberActor,
  bundleId: string,
): Promise<AppliedOnSession[]> {
  const rows = await getDb().execute(sql`
    SELECT cs.id, cs.display_name, st.applied_version_id, st.harness_state,
           cp.version_id AS current_version_id,
           (extract(epoch from st.reported_at) * 1000)::bigint AS reported_ms
    FROM web.session_bundle_state st
    JOIN web.cli_session cs ON cs.id = st.session_id
    LEFT JOIN plane.current_pointer cp
      ON cp.workspace_id = ${actor.workspaceId} AND cp.bundle_id = st.bundle_id
    WHERE cs.workspace_id = ${actor.workspaceId} AND cs.user_id = ${actor.userId}
      AND st.bundle_id = ${bundleId}
    ORDER BY cs.display_name, cs.id
  `);
  return (
    rows.rows as unknown as {
      id: string;
      display_name: string;
      applied_version_id: string;
      harness_state: unknown;
      current_version_id: string | null;
      reported_ms: string;
    }[]
  ).map((r) => ({
    sessionId: r.id,
    displayName: r.display_name,
    appliedVersionId: r.applied_version_id,
    current: r.current_version_id !== null && r.current_version_id === r.applied_version_id,
    harnesses: harnessesOf(r.harness_state),
    reportedAtMs: Number(r.reported_ms),
  }));
}

/** One of the viewer's sessions as the visibility page lists it — exactly the fields the
 * workspace can read about that machine, and nothing beside them. */
export interface VisibleSession {
  sessionId: string;
  displayName: string;
  /** The last report's time (epoch-ms), or null when the machine has never synced. */
  lastSeenAtMs: number | null;
  skills: { name: string; appliedVersionId: string }[];
}

/**
 * The viewer's OWN sessions and the per-bundle state they report — the visibility page's
 * evidence. It deliberately selects the same four things the page's prose promises (the
 * machine's name, the bundle's name, the version, the last report), so the list IS the
 * disclosure rather than an illustration of it.
 */
export async function visibleSessionsOf(actor: MemberActor): Promise<VisibleSession[]> {
  const sessionRows = await getDb().execute(sql`
    SELECT cs.id, cs.display_name,
           (extract(epoch from cs.last_seen_at) * 1000)::bigint AS last_seen_ms
    FROM web.cli_session cs
    WHERE cs.workspace_id = ${actor.workspaceId} AND cs.user_id = ${actor.userId}
    ORDER BY cs.display_name, cs.id
  `);
  const sessions = sessionRows.rows as unknown as {
    id: string;
    display_name: string;
    last_seen_ms: string | null;
  }[];
  if (sessions.length === 0) {
    return [];
  }
  const stateRows = await getDb()
    .select({
      sessionId: sessionBundleState.sessionId,
      name: bundle.name,
      appliedVersionId: sessionBundleState.appliedVersionId,
    })
    .from(sessionBundleState)
    .innerJoin(
      bundle,
      and(eq(bundle.id, sessionBundleState.bundleId), eq(bundle.workspaceId, actor.workspaceId)),
    )
    .where(
      inArray(
        sessionBundleState.sessionId,
        sessions.map((s) => s.id),
      ),
    )
    .orderBy(asc(sessionBundleState.sessionId), asc(bundle.name));
  const bySession = new Map<string, { name: string; appliedVersionId: string }[]>();
  for (const row of stateRows) {
    const list = bySession.get(row.sessionId) ?? [];
    list.push({ name: row.name, appliedVersionId: row.appliedVersionId });
    bySession.set(row.sessionId, list);
  }
  return sessions.map((s) => ({
    sessionId: s.id,
    displayName: s.display_name,
    lastSeenAtMs: s.last_seen_ms === null ? null : Number(s.last_seen_ms),
    skills: bySession.get(s.id) ?? [],
  }));
}

/** ACTIVE sessions in this workspace — the onboarding checklist's probe. */
export async function workspaceSessionCount(actor: MemberActor): Promise<number> {
  // Live means active AND within the owner-set expiry (the guard's predicate) — an expired
  // machine is not a working install, so the onboarding probe must not count it.
  const rows = await getDb().execute(sql`
    SELECT count(*)::int AS n FROM web.cli_session cs
    JOIN web.workspace w ON w.id = cs.workspace_id
    WHERE cs.workspace_id = ${actor.workspaceId} AND cs.status = 'active'
      AND ${sessionUnexpiredSql("cs", "w")}
  `);
  return Number((rows.rows[0] as { n: number | string } | undefined)?.n ?? 0);
}
