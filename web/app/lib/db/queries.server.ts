import { and, asc, count, desc, eq, inArray, sql } from "drizzle-orm";
import type { MemberActor, OwnerActor, PlaneSeat, UserActor } from "@/lib/auth/guards.server";
import { getDb, getPool } from "@/lib/db/index.server";
import { policyEvent, proposalComment } from "@/lib/db/schema.app";
import {
  planeCatalog,
  planeCurrent,
  planeProposals,
  planeSkillCommit,
  planeWorkspace,
  planeWorkspaceMember,
  planeWorkspacePolicy,
} from "@/lib/db/schema.plane";

/**
 * The DATA ACCESS LAYER — the one sanctioned door to the web tier's OWN tables AND the
 * read-only `plane` models (scripts/check-boundary.mjs forbids the pool/schema/drizzle imports
 * everywhere else). Every function REQUIRES the actor whose authority it exercises: actors are
 * mintable only by the guards in app/lib/auth/guards.server.ts, so a caller that skipped its
 * guard cannot compile, and workspace-scoped reads derive their scope FROM the actor (or assert
 * it) so a wrong-workspace actor fails loudly instead of leaking.
 *
 * Membership, roles, workspace addresses, and the skill catalog come from the DIRECTORY's
 * tables (SELECT-only by grant on the vault tables); the ONE row-write door into the directory
 * is the guarded `topos_*` SQL functions — policy logic lives in the database, written once,
 * and this tier never re-implements a role gate.
 */

export type PolicyEventRow = typeof policyEvent.$inferSelect;
export type PlaneWorkspaceRow = typeof planeWorkspace.$inferSelect;
export type PlaneMemberRow = typeof planeWorkspaceMember.$inferSelect;

/** A member actor must match the workspace it reads. A mismatch is a bug, never a leak. */
function assertWorkspaceScope(actor: MemberActor, ws: string): void {
  if (actor.workspaceId !== ws) {
    throw new Error(`workspace-scope mismatch: actor for ${actor.workspaceId} used against ${ws}`);
  }
}

/** One dashboard row: a directory roster seat (the ONLY membership there is). */
export interface WorkspaceMembership {
  id: string;
  displayName: string;
  /** The workspace's address slug — what joining and sharing speak. */
  address: string;
  role: "owner" | "reviewer" | "member";
  status: "invited" | "confirmed";
  /**
   * Whether the row may render as a LINK into the workspace — the same rule the guard applies
   * (a CONFIRMED seat), carried ON the row so no consumer re-derives it. An invited-only row
   * is visible but not navigable.
   */
  navigable: boolean;
}

/**
 * The workspaces visible to the actor's OWN email: roster seats (invited + confirmed, seat
 * order). Display names come from `plane.workspace` with the id as the honest fallback (a
 * seat can outlive its workspace row).
 */
export async function planeMembershipsFor(actor: UserActor): Promise<WorkspaceMembership[]> {
  const seats = await getDb()
    .select({
      id: planeWorkspaceMember.workspaceId,
      displayName: planeWorkspace.displayName,
      address: planeWorkspace.name,
      role: planeWorkspaceMember.role,
      status: planeWorkspaceMember.status,
    })
    .from(planeWorkspaceMember)
    .leftJoin(planeWorkspace, eq(planeWorkspace.workspaceId, planeWorkspaceMember.workspaceId))
    .where(
      and(
        eq(planeWorkspaceMember.principal, actor.email),
        inArray(planeWorkspaceMember.status, ["invited", "confirmed"]),
      ),
    )
    // Seat order, TEXT ISO-8601 — never the LEFT-JOINed workspace.created_at (NULL for a
    // rowless workspace).
    .orderBy(asc(planeWorkspaceMember.addedAt), asc(planeWorkspaceMember.workspaceId));
  return seats.map((s) => ({
    id: s.id,
    displayName: s.displayName ?? s.id,
    address: s.address ?? s.id,
    role: s.role,
    status: s.status,
    navigable: s.status === "confirmed",
  }));
}

/** The actor's own roster seat in ONE workspace — the guard's roster probe (single row). */
export async function planeMembership(
  actor: UserActor,
  ws: string,
): Promise<PlaneSeat | undefined> {
  const rows = await getDb()
    .select({ role: planeWorkspaceMember.role, status: planeWorkspaceMember.status })
    .from(planeWorkspaceMember)
    .where(
      and(
        eq(planeWorkspaceMember.workspaceId, ws),
        eq(planeWorkspaceMember.principal, actor.email),
      ),
    )
    .limit(1);
  return rows[0];
}

/** One workspace row (display name + address + created_at TEXT ISO-8601 — parse at render). */
export async function planeWorkspaceById(
  actor: MemberActor,
  ws: string,
): Promise<PlaneWorkspaceRow | undefined> {
  assertWorkspaceScope(actor, ws);
  const rows = await getDb()
    .select()
    .from(planeWorkspace)
    .where(eq(planeWorkspace.workspaceId, ws))
    .limit(1);
  return rows[0];
}

/** The workspace's full roster, seat order — the members panel's read (plain rows). */
export async function rosterOf(actor: MemberActor): Promise<PlaneMemberRow[]> {
  return getDb()
    .select()
    .from(planeWorkspaceMember)
    .where(eq(planeWorkspaceMember.workspaceId, actor.workspaceId))
    .orderBy(asc(planeWorkspaceMember.addedAt), asc(planeWorkspaceMember.principal));
}

/** The workspace policy row — knob display (the write path is elsewhere, never a table write). */
export async function workspacePolicyOf(
  actor: MemberActor,
): Promise<typeof planeWorkspacePolicy.$inferSelect | undefined> {
  const rows = await getDb()
    .select()
    .from(planeWorkspacePolicy)
    .where(eq(planeWorkspacePolicy.workspaceId, actor.workspaceId))
    .limit(1);
  return rows[0];
}

/** One catalog entry in the workspace — named identity + the current pointer, when one exists. */
export interface SkillIndexRow {
  /** The immutable custody key — what every vault call keys on. */
  skillId: string;
  /** The catalog name — the user key pages route on. */
  name: string;
  /** The advisory display name (the author's folder name); render falls back to `name`. */
  displayName: string | null;
  status: "active" | "archived" | "deleted";
  /** The `current` version id (lowercase hex64), or null while nothing is published. */
  versionId: string | null;
  epoch: number | null;
  seq: number | null;
  /** BIGINT epoch-milliseconds off the pointer row — `new Date(ms)` at the display edge only. */
  updatedAtMs: number | null;
  /** hex64, or null (schema-honest: `skill_commit.bundle_digest` is NULLABLE) — render an em-dash. */
  bundleDigest: string | null;
  openProposals: number;
}

/**
 * The grouped open-proposal count per skill. The `epoch = base_epoch AND seq = base_seq` join
 * is a DISPLAY-ONLY mirror of the vault's staleness rule (`open AND base == current`) — the
 * vault is the authority; if its predicate ever moves, this count is the thing that silently
 * drifts, which is why the publish-stales-a-proposal seed pins it in the e2e. ONE spelled
 * copy, shared by the index and the single-row probe.
 */
function openProposalCounts(ws: string, skillId?: string) {
  return getDb()
    .select({ skillId: planeProposals.skillId, n: count() })
    .from(planeProposals)
    .innerJoin(
      planeCurrent,
      and(
        eq(planeCurrent.workspaceId, planeProposals.workspaceId),
        eq(planeCurrent.skillId, planeProposals.skillId),
        eq(planeCurrent.epoch, planeProposals.baseEpoch),
        eq(planeCurrent.seq, planeProposals.baseSeq),
      ),
    )
    .where(
      and(
        eq(planeProposals.workspaceId, ws),
        eq(planeProposals.status, "open"),
        ...(skillId === undefined ? [] : [eq(planeProposals.skillId, skillId)]),
      ),
    )
    .groupBy(planeProposals.skillId);
}

/**
 * The catalog SELECT shared by the index and the single-row probe: catalog ⟕ current ⟕
 * skill_commit. The CATALOG is the identity surface (a skill exists the moment its name is
 * minted); the pointer joins in when a publish has landed one.
 */
function skillIndexSelect() {
  return getDb()
    .select({
      skillId: planeCatalog.skillId,
      name: planeCatalog.name,
      displayName: planeCatalog.displayName,
      status: planeCatalog.status,
      versionId: planeCurrent.commitId,
      epoch: planeCurrent.epoch,
      seq: planeCurrent.seq,
      updatedAtMs: planeCurrent.updatedAt,
      bundleDigest: planeSkillCommit.bundleDigest,
    })
    .from(planeCatalog)
    .leftJoin(
      planeCurrent,
      and(
        eq(planeCurrent.workspaceId, planeCatalog.workspaceId),
        eq(planeCurrent.skillId, planeCatalog.skillId),
      ),
    )
    .leftJoin(
      planeSkillCommit,
      and(
        eq(planeSkillCommit.workspaceId, planeCurrent.workspaceId),
        eq(planeSkillCommit.commitId, planeCurrent.commitId),
      ),
    );
}

/**
 * The workspace's skill catalog, straight from the directory's own tables (a CLI publish
 * lands its rows and the next page load shows them — no web-tier state). Active entries only:
 * archived/deleted identities are lifecycle surfaces, not catalog rows.
 */
export async function skillIndexOf(actor: MemberActor, ws: string): Promise<SkillIndexRow[]> {
  assertWorkspaceScope(actor, ws);
  const [rows, counts] = await Promise.all([
    skillIndexSelect()
      .where(and(eq(planeCatalog.workspaceId, ws), eq(planeCatalog.status, "active")))
      .orderBy(asc(planeCatalog.name)),
    openProposalCounts(ws),
  ]);
  const open = new Map(counts.map((c) => [c.skillId, c.n]));
  return rows.map((row) => ({ ...row, openProposals: open.get(row.skillId) ?? 0 }));
}

/**
 * One catalog row in the actor's OWN workspace, by NAME (the user key pages route on) — the
 * skill pages' existence probe + header. Vault calls key on the returned `skillId`.
 */
export async function skillIndexRow(
  actor: MemberActor,
  name: string,
): Promise<SkillIndexRow | undefined> {
  const ws = actor.workspaceId;
  const rows = await skillIndexSelect()
    .where(
      and(
        eq(planeCatalog.workspaceId, ws),
        eq(planeCatalog.name, name),
        eq(planeCatalog.status, "active"),
      ),
    )
    .limit(1);
  const row = rows[0];
  if (row === undefined) {
    return undefined;
  }
  const counts = await openProposalCounts(ws, row.skillId);
  return { ...row, openProposals: counts[0]?.n ?? 0 };
}

/** The outcome codes `topos_invite` speaks (the database's vocabulary, relayed verbatim). */
export type InviteOutcome =
  | "invited"
  | "member_required"
  | "owner_role_required"
  | "unknown_channel";

/**
 * Invitation IS a guarded roster write: ONE call to the database's `topos_invite`, which
 * re-runs the membership + invite-policy gates itself, records who invited whom, and
 * pre-places invitees into named channels — this tier adds nothing to the decision. The
 * outcome code is the function's own vocabulary.
 */
export async function inviteMembers(
  actor: MemberActor,
  emails: string[],
  channels: string[] = [],
): Promise<InviteOutcome> {
  const createdAt = new Date().toISOString();
  // The email + channel lists bind as single `text[]` params: the driver serializes a JS array
  // into one Postgres array literal, so an empty list is `'{}'` (never a spread of scalars, which
  // a drizzle `sql` template would produce — malformed for a `::text[]` cast).
  const result = await getPool().query<{ outcome: InviteOutcome }>(
    "select topos_invite($1, $2, $3::text[], $4::text[], $5) as outcome",
    [actor.workspaceId, actor.email, emails, channels, createdAt],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_invite returned no outcome");
  }
  return outcome;
}

export type PolicyOutcome = "ok" | "denied" | "error";

export async function recordPolicyEvent(
  actor: OwnerActor,
  reviewRequired: boolean,
  outcome: PolicyOutcome,
): Promise<void> {
  await getDb().insert(policyEvent).values({
    workspaceId: actor.workspaceId,
    reviewRequired,
    setBy: actor.email,
    outcome,
  });
}

export async function lastPolicyEvent(
  actor: MemberActor,
  ws: string,
): Promise<PolicyEventRow | undefined> {
  assertWorkspaceScope(actor, ws);
  const rows = await getDb()
    .select()
    .from(policyEvent)
    .where(eq(policyEvent.workspaceId, ws))
    .orderBy(desc(policyEvent.setAt))
    .limit(1);
  return rows[0];
}

export type ProposalCommentRow = typeof proposalComment.$inferSelect;

/** The thread render's window: the NEWEST comments a page load will display. */
export const COMMENT_DISPLAY_LIMIT = 200;

/** The hard per-thread ceiling the atomic insert enforces (the unbounded-write belt). */
export const COMMENT_THREAD_CAP = 500;

export interface ProposalCommentThread {
  /** Oldest-first among the LATEST `COMMENT_DISPLAY_LIMIT` comments. */
  comments: ProposalCommentRow[];
  /** True when older comments exist beyond the window — retained, just not displayed. */
  truncated: boolean;
}

/**
 * One proposal's comment thread, scoped to the actor's OWN workspace: the newest
 * `COMMENT_DISPLAY_LIMIT` comments (fetched newest-first with a one-row truncation probe,
 * re-sorted oldest-first for display) — the render stays bounded however long a thread grows.
 */
export async function proposalCommentsFor(
  actor: MemberActor,
  skillId: string,
  versionId: string,
): Promise<ProposalCommentThread> {
  const rows = await getDb()
    .select()
    .from(proposalComment)
    .where(
      and(
        eq(proposalComment.workspaceId, actor.workspaceId),
        eq(proposalComment.skillId, skillId),
        eq(proposalComment.versionId, versionId),
      ),
    )
    .orderBy(desc(proposalComment.createdAt), desc(proposalComment.id))
    .limit(COMMENT_DISPLAY_LIMIT + 1);
  const truncated = rows.length > COMMENT_DISPLAY_LIMIT;
  const window = truncated ? rows.slice(0, COMMENT_DISPLAY_LIMIT) : rows;
  return { comments: window.reverse(), truncated };
}

/** The insert's typed outcome — the ambiguity between a replay and a full thread is resolved. */
export type CommentInsertOutcome = "inserted" | "replayed" | "thread_full";

/**
 * Append ONE comment. The id is the render-minted UUID from the form, so a retried submit is
 * idempotent by PK — ON CONFLICT DO NOTHING, never a duplicate row, never an error. The author
 * is the guard-minted actor's verified email; the thread is append-only (no update/delete
 * function exists in this DAL by design).
 *
 * The INSERT … SELECT gates atomically on the thread's row count staying under
 * `COMMENT_THREAD_CAP` — comments are otherwise an unbounded write lane (route actions bypass
 * every route-level limiter). A zero row count is then disambiguated: the id already landed (a
 * replay — success) or the thread is genuinely full.
 */
export async function insertProposalComment(
  actor: MemberActor,
  input: { id: string; skillId: string; versionId: string; body: string },
): Promise<CommentInsertOutcome> {
  const db = getDb();
  const result = await db.execute(sql`
    insert into ${proposalComment} (id, workspace_id, skill_id, version_id, author_email, body)
    select ${input.id}, ${actor.workspaceId}, ${input.skillId}, ${input.versionId}, ${actor.email}, ${input.body}
    where (
      select count(*) from ${proposalComment}
      where ${proposalComment.workspaceId} = ${actor.workspaceId}
        and ${proposalComment.skillId} = ${input.skillId}
        and ${proposalComment.versionId} = ${input.versionId}
    ) < ${COMMENT_THREAD_CAP}
    on conflict (id) do nothing
  `);
  if ((result.rowCount ?? 0) > 0) {
    return "inserted";
  }
  const replay = await db
    .select({ id: proposalComment.id })
    .from(proposalComment)
    .where(eq(proposalComment.id, input.id))
    .limit(1);
  return replay.length > 0 ? "replayed" : "thread_full";
}
