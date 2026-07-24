import { and, asc, eq, inArray, sql } from "drizzle-orm";
import type { MemberActor, UserActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
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
    })
    .from(cliSession)
    .innerJoin(workspace, eq(workspace.id, cliSession.workspaceId))
    .where(eq(cliSession.userId, actor.userId))
    .orderBy(sql`${cliSession.createdAt} DESC`);
  return rows.map((r) => ({
    sessionId: r.sessionId,
    displayName: r.displayName,
    workspaceId: r.workspaceId,
    workspaceName: r.workspaceName,
    workspaceDisplayName: r.workspaceDisplayName,
    status: r.status as AccountSession["status"],
    createdAtMs: Number(r.createdAtMs),
    lastSeenAtMs: r.lastSeenAtMs === null ? null : Number(r.lastSeenAtMs),
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
  freshness: SessionFreshness;
  /** The bundles this session last reported, catalog-name order. */
  skills: SessionSkillState[];
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
      freshness: freshnessOf(
        s.last_seen_ms === null ? null : Number(s.last_seen_ms),
        stalenessWindowMs,
        now,
      ),
      skills: statesBySession.get(s.id) ?? [],
    })),
    stalenessWindowMs,
    sessionApproval,
    sessionMaxAgeMs,
    wholeWorkspace,
  };
}

/** ACTIVE sessions in this workspace — the onboarding checklist's probe. */
export async function workspaceSessionCount(actor: MemberActor): Promise<number> {
  const rows = await getDb()
    .select({ n: sql<number>`count(*)::int` })
    .from(cliSession)
    .where(and(eq(cliSession.workspaceId, actor.workspaceId), eq(cliSession.status, "active")));
  return rows[0]?.n ?? 0;
}
