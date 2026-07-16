import { and, asc, count, desc, eq, sql } from "drizzle-orm";
import { alias } from "drizzle-orm/pg-core";
import type { MemberActor, OwnerActor, UserActor } from "@/lib/auth/guards.server";
import { auditInTx } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { personDisplayLeftSql } from "@/lib/db/person-display.server";
import { bundle, proposal, proposalComment, seat, workspace } from "@/lib/db/schema.app";
import { user } from "@/lib/db/schema.auth";
import { planeCurrentPointer, planeVersionDigest } from "@/lib/db/schema.custody";

/**
 * The DATA ACCESS LAYER — the one sanctioned door to the app's OWN `web` schema AND the
 * read-only `plane` custody mirror (scripts/check-boundary.mjs forbids the pool/schema/drizzle
 * imports everywhere else). Every function REQUIRES the actor whose authority it exercises:
 * actors are mintable only by the guards in app/lib/auth/guards.server.ts, so a caller that
 * skipped its guard cannot compile, and workspace-scoped reads derive their scope FROM the
 * actor (or assert it) so a wrong-workspace actor fails loudly instead of leaking.
 *
 * Since the identity unification, EVERY directory/policy/product row lives in this app's own
 * schema — there are no guarded SQL functions and no plane row-writes: policy logic is written
 * here, once, with role gates carried by the branded actor types and every mutating op
 * emitting its audit row in the SAME transaction (auditInTx).
 */

export type BundleRow = typeof bundle.$inferSelect;
export type WorkspaceRow = typeof workspace.$inferSelect;

/** A member actor must match the workspace it reads. A mismatch is a bug, never a leak. */
export function assertWorkspaceScope(actor: MemberActor, ws: string): void {
  if (actor.workspaceId !== ws) {
    throw new Error(`workspace-scope mismatch: actor for ${actor.workspaceId} used against ${ws}`);
  }
}

/** One dashboard row: a seat (the ONLY membership there is). */
export interface WorkspaceMembership {
  id: string;
  displayName: string;
  /** The workspace's address slug — what joining and sharing speak. */
  address: string;
  role: "owner" | "reviewer" | "member";
  /** A seat always admits — kept on the row so no consumer re-derives the rule. */
  navigable: boolean;
}

/**
 * The workspaces the actor holds a seat in — on this single-tenant install, zero or one row.
 * Seats ARE admission (invitations are claims on future users in their own table and list
 * nowhere here).
 */
export async function membershipsFor(actor: UserActor): Promise<WorkspaceMembership[]> {
  const rows = await getDb()
    .select({
      id: seat.workspaceId,
      displayName: workspace.displayName,
      address: workspace.name,
      role: seat.role,
    })
    .from(seat)
    .innerJoin(workspace, eq(workspace.id, seat.workspaceId))
    .where(eq(seat.userId, actor.userId))
    .orderBy(asc(seat.createdAt), asc(seat.workspaceId));
  return rows.map((s) => ({
    id: s.id,
    displayName: s.displayName,
    address: s.address,
    role: s.role as WorkspaceMembership["role"],
    navigable: true,
  }));
}

/** One workspace row for a member's render (display name + address + knobs). */
export async function workspaceById(
  actor: MemberActor,
  ws: string,
): Promise<WorkspaceRow | undefined> {
  assertWorkspaceScope(actor, ws);
  const rows = await getDb().select().from(workspace).where(eq(workspace.id, ws)).limit(1);
  return rows[0];
}

// ── The bundle catalog (the identity surface pages route on) ────────────────────────────────

/** One catalog entry — named identity + the custody pointer, when one exists. */
export interface SkillIndexRow {
  /** The immutable custody key — what every vault call keys on. */
  skillId: string;
  /** The catalog name — the user key pages route on. */
  name: string;
  /** The advisory display name (the author's folder name); render falls back to `name`. */
  displayName: string | null;
  status: "active" | "archived" | "deleted";
  /** The bundle kind — `"skill"` today; display metadata only, never branched on. */
  kind: string;
  /** The `current` version id, or null while nothing is published. */
  versionId: string | null;
  /** The pointer's CAS generation, or null while nothing is published. */
  generation: number | null;
  /** Epoch-milliseconds off the pointer row — `new Date(ms)` at the display edge only. */
  updatedAtMs: number | null;
  /** The consent digest of `current`, or null — render an em-dash. */
  bundleDigest: string | null;
  openProposals: number;
}

/** The grouped OPEN-proposal count per bundle (one spelled copy, index + single-row probe). */
function openProposalCounts(ws: string, bundleId?: string) {
  return getDb()
    .select({ bundleId: proposal.bundleId, n: count() })
    .from(proposal)
    .where(
      and(
        eq(proposal.workspaceId, ws),
        eq(proposal.status, "open"),
        ...(bundleId === undefined ? [] : [eq(proposal.bundleId, bundleId)]),
      ),
    )
    .groupBy(proposal.bundleId);
}

/**
 * The catalog SELECT shared by the index and the single-row probe: bundle ⟕ current_pointer ⟕
 * version_digest. The BUNDLE row is the identity surface (a bundle exists the moment its name
 * is minted); the pointer joins in when a publish has landed one.
 */
function skillIndexSelect() {
  return getDb()
    .select({
      skillId: bundle.id,
      name: bundle.name,
      displayName: bundle.displayName,
      status: bundle.status,
      kind: bundle.kind,
      versionId: planeCurrentPointer.versionId,
      generation: planeCurrentPointer.generation,
      updatedAtMs: sql<
        number | null
      >`(extract(epoch from ${planeCurrentPointer.movedAt}) * 1000)::bigint`.mapWith((v) =>
        v === null ? null : Number(v),
      ),
      bundleDigest: planeVersionDigest.bundleDigest,
    })
    .from(bundle)
    .leftJoin(
      planeCurrentPointer,
      and(
        eq(planeCurrentPointer.workspaceId, bundle.workspaceId),
        eq(planeCurrentPointer.bundleId, bundle.id),
      ),
    )
    .leftJoin(
      planeVersionDigest,
      and(
        eq(planeVersionDigest.workspaceId, bundle.workspaceId),
        eq(planeVersionDigest.bundleId, bundle.id),
        eq(planeVersionDigest.versionId, planeCurrentPointer.versionId),
      ),
    );
}

/**
 * The workspace's catalog (a CLI publish lands its rows and the next page load shows them —
 * no extra state). Active entries only: archived/deleted identities are lifecycle surfaces.
 */
export async function skillIndexOf(actor: MemberActor, ws: string): Promise<SkillIndexRow[]> {
  assertWorkspaceScope(actor, ws);
  const [rows, counts] = await Promise.all([
    skillIndexSelect()
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.status, "active")))
      .orderBy(asc(bundle.name)),
    openProposalCounts(ws),
  ]);
  const open = new Map(counts.map((c) => [c.bundleId, c.n]));
  return rows.map((row) => ({
    ...row,
    status: row.status as SkillIndexRow["status"],
    openProposals: open.get(row.skillId) ?? 0,
  }));
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
    .where(and(eq(bundle.workspaceId, ws), eq(bundle.name, name), eq(bundle.status, "active")))
    .limit(1);
  const row = rows[0];
  if (row === undefined) {
    return undefined;
  }
  const counts = await openProposalCounts(ws, row.skillId);
  return {
    ...row,
    status: row.status as SkillIndexRow["status"],
    openProposals: counts[0]?.n ?? 0,
  };
}

/** One bundle row by its immutable id (any status) — the ceremonies' re-read anchor. */
export async function bundleById(
  actor: MemberActor,
  bundleId: string,
): Promise<BundleRow | undefined> {
  const rows = await getDb()
    .select()
    .from(bundle)
    .where(and(eq(bundle.workspaceId, actor.workspaceId), eq(bundle.id, bundleId)))
    .limit(1);
  return rows[0];
}

// ── Proposals (web rows; candidates live vault-side by digest) ──────────────────────────────

export type ProposalRow = typeof proposal.$inferSelect;

/**
 * A proposal row with the two people joined in for DISPLAY: the proposer's and resolver's
 * current names (null when the user id is null — a deleted account). Display attributes only;
 * every authority decision keys on the ids.
 */
export type ProposalDisplayRow = ProposalRow & {
  proposedByDisplay: string | null;
  resolvedByDisplay: string | null;
};

const proposerUser = alias(user, "proposer_user");
const resolverUser = alias(user, "resolver_user");

function proposalDisplaySelect() {
  return getDb()
    .select({
      row: proposal,
      proposedByDisplay: personDisplayLeftSql(proposerUser),
      resolvedByDisplay: personDisplayLeftSql(resolverUser),
    })
    .from(proposal)
    .leftJoin(proposerUser, eq(proposerUser.id, proposal.proposedBy))
    .leftJoin(resolverUser, eq(resolverUser.id, proposal.resolvedBy));
}

function toDisplayRow(joined: {
  row: ProposalRow;
  proposedByDisplay: string | null;
  resolvedByDisplay: string | null;
}): ProposalDisplayRow {
  return {
    ...joined.row,
    proposedByDisplay: joined.proposedByDisplay,
    resolvedByDisplay: joined.resolvedByDisplay,
  };
}

/** The bundle's proposal rows, open first then newest-resolved — the proposals page's read. */
export async function proposalsOf(
  actor: MemberActor,
  bundleId: string,
): Promise<ProposalDisplayRow[]> {
  const rows = await proposalDisplaySelect()
    .where(and(eq(proposal.workspaceId, actor.workspaceId), eq(proposal.bundleId, bundleId)))
    .orderBy(
      sql`case when ${proposal.status} = 'open' then 0 else 1 end`,
      desc(proposal.createdAt),
    );
  return rows.map(toDisplayRow);
}

/** One proposal row by its candidate version id — the review page's read. */
export async function proposalByCandidate(
  actor: MemberActor,
  bundleId: string,
  versionId: string,
): Promise<ProposalDisplayRow | undefined> {
  const rows = await proposalDisplaySelect()
    .where(
      and(
        eq(proposal.workspaceId, actor.workspaceId),
        eq(proposal.bundleId, bundleId),
        eq(proposal.candidateVersionId, versionId),
      ),
    )
    // An open row outranks a resolved one for the same candidate (a re-propose after reject).
    .orderBy(sql`case when ${proposal.status} = 'open' then 0 else 1 end`, desc(proposal.createdAt))
    .limit(1);
  const row = rows[0];
  return row === undefined ? undefined : toDisplayRow(row);
}

/** Whether a proposal row exists for this candidate — the comment lane's existence probe. */
export async function proposalExists(
  actor: MemberActor,
  bundleId: string,
  versionId: string,
): Promise<boolean> {
  return (await proposalByCandidate(actor, bundleId, versionId)) !== undefined;
}

// ── Proposal comments (re-keyed to the ONE user identity) ───────────────────────────────────

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
  bundleId: string,
  versionId: string,
): Promise<ProposalCommentThread> {
  const rows = await getDb()
    .select()
    .from(proposalComment)
    .where(
      and(
        eq(proposalComment.workspaceId, actor.workspaceId),
        eq(proposalComment.bundleId, bundleId),
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
 * idempotent by PK — ON CONFLICT DO NOTHING, never a duplicate row, never an error. Authorship
 * is the actor's user id + a display snapshot (readable after renames/deletes); the thread is
 * append-only (no update/delete function exists in this DAL by design).
 *
 * The INSERT … SELECT gates atomically on the thread's row count staying under
 * `COMMENT_THREAD_CAP` — comments are otherwise an unbounded write lane. A zero row count is
 * then disambiguated: the id already landed (a replay — success) or the thread is full.
 */
export async function insertProposalComment(
  actor: MemberActor,
  input: { id: string; bundleId: string; versionId: string; body: string },
): Promise<CommentInsertOutcome> {
  const db = getDb();
  const result = await db.execute(sql`
    insert into ${proposalComment} (id, workspace_id, bundle_id, version_id, author_user_id, author_display, body)
    select ${input.id}, ${actor.workspaceId}, ${input.bundleId}, ${input.versionId}, ${actor.userId}, ${actor.display}, ${input.body}
    where (
      select count(*) from ${proposalComment}
      where ${proposalComment.workspaceId} = ${actor.workspaceId}
        and ${proposalComment.bundleId} = ${input.bundleId}
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
    .where(
      and(
        eq(proposalComment.id, input.id),
        eq(proposalComment.workspaceId, actor.workspaceId),
        eq(proposalComment.bundleId, input.bundleId),
        eq(proposalComment.versionId, input.versionId),
      ),
    )
    .limit(1);
  return replay.length > 0 ? "replayed" : "thread_full";
}

// ── The review-default knob (a plain workspace column now) ──────────────────────────────────

export type ReviewDefaultOutcome = "set";

/**
 * Set the workspace's protection DEFAULT (`open`/`reviewed` — what an unpinned bundle
 * inherits). The OwnerActor brand IS the gate; the audit row lands in the same transaction.
 */
export async function setReviewDefault(
  actor: OwnerActor,
  required: boolean,
): Promise<ReviewDefaultOutcome> {
  const value = required ? "reviewed" : "open";
  await getDb().transaction(async (tx) => {
    await tx
      .update(workspace)
      .set({ protectionDefault: value })
      .where(eq(workspace.id, actor.workspaceId));
    await auditInTx(tx, {
      workspaceId: actor.workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "policy_review_default",
      subject: value,
      outcome: "ok",
    });
  });
  return "set";
}
