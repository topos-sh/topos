import type { MemberActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
import { adminEvent } from "@/lib/db/schema.app";

/**
 * The admin ceremonies' audit writer — ONE row per attempt, whatever the outcome (a refused
 * step-up is as much a fact as a landed act; the trail must show both). Kinds are an open
 * vocabulary; subjects name the target (an email, a skill or channel name, a device key id).
 * Never throws into a ceremony: recording is best-effort by design — an audit fault must not
 * mask the act's own outcome (the authoritative audit for directory writes is the directory's;
 * this trail is the web tier's own ledger of who drove which ceremony).
 */
export type AdminOutcome = "ok" | "denied" | "error";

export async function recordAdminEvent(
  actor: MemberActor,
  event: { kind: string; subject: string; detail?: string; outcome: AdminOutcome },
): Promise<void> {
  try {
    await getDb().insert(adminEvent).values({
      workspaceId: actor.workspaceId,
      kind: event.kind,
      subject: event.subject,
      detail: event.detail,
      setBy: actor.email,
      outcome: event.outcome,
    });
  } catch (error) {
    console.error("admin_event insert failed", error);
  }
}
