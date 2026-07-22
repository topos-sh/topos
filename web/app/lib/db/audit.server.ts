import { and, desc, eq, isNotNull, sql } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
import { auditEvent } from "@/lib/db/schema.app";

/**
 * The admin ceremonies' audit writer + the history pages' reader, over the ONE `audit_event`
 * ledger (which absorbed the old per-surface event tables). ONE row per attempt, whatever the
 * outcome (a refused attempt is as much a fact as a landed act; the trail must show both).
 * Kinds are an open vocabulary; subjects name the target (a user id, a bundle or channel name,
 * a device id). recordAdminEvent never throws into a ceremony: recording is best-effort by
 * design — an audit fault must not mask the act's own outcome. (Data-layer MUTATIONS emit
 * their rows in-transaction via auditInTx instead; this writer is for route-level ceremony
 * outcomes that have no surrounding transaction.)
 */
export type AdminOutcome = "ok" | "denied" | "error";

export async function recordAdminEvent(
  actor: MemberActor,
  event: { kind: string; subject: string; detail?: string; outcome: AdminOutcome },
): Promise<void> {
  try {
    await getDb()
      .insert(auditEvent)
      .values({
        workspaceId: actor.workspaceId,
        actorUserId: actor.userId,
        actorDisplay: actor.display,
        kind: event.kind,
        subject: event.subject,
        outcome: event.outcome,
        details: event.detail === undefined ? {} : { detail: event.detail },
      });
  } catch (error) {
    console.error("audit_event insert failed", error);
  }
}

export type AuditEventRow = typeof auditEvent.$inferSelect;

/** The audit window ceiling shared by the history readers. */
export const AUDIT_MAX_LIMIT = 100;

export interface AuditWindow {
  events: AuditEventRow[];
  /** True when older events exist beyond the window — retained, just not shown. */
  hasMore: boolean;
}

/**
 * The audit rows naming ONE subject (a channel id, a bundle id), newest first, bounded with a
 * +1 probe so a full window can honestly say older events exist. The append-only ledger
 * outlives the row it names — history is reachable only through a live resource by design.
 */
export async function auditEventsForSubject(
  actor: MemberActor,
  subject: string,
  opts: { limit?: number } = {},
): Promise<AuditWindow> {
  const limit = Math.min(Math.max(opts.limit ?? AUDIT_MAX_LIMIT, 1), AUDIT_MAX_LIMIT);
  const rows = await getDb()
    .select()
    .from(auditEvent)
    .where(and(eq(auditEvent.workspaceId, actor.workspaceId), eq(auditEvent.subject, subject)))
    .orderBy(desc(auditEvent.id))
    .limit(limit + 1);
  const hasMore = rows.length > limit;
  return { events: hasMore ? rows.slice(0, limit) : rows, hasMore };
}

/** The latest audit row of ONE kind — the policy panels' "last set by" line. */
export async function lastAuditEventOfKind(
  actor: MemberActor,
  kind: string,
): Promise<AuditEventRow | undefined> {
  const rows = await getDb()
    .select()
    .from(auditEvent)
    .where(
      and(
        eq(auditEvent.workspaceId, actor.workspaceId),
        eq(auditEvent.kind, kind),
        // The panels narrate acts, not refusals.
        sql`${auditEvent.outcome} = 'ok'`,
        isNotNull(auditEvent.createdAt),
      ),
    )
    .orderBy(desc(auditEvent.id))
    .limit(1);
  return rows[0];
}
