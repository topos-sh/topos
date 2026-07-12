import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { getPool } from "@/lib/db/index.server";

/**
 * The ROSTER write DAL — the two owner/self acts the members page drives that mutate the
 * DIRECTORY's own roster rows. Both are guarded `topos_*` SQL functions: the role gate + the
 * last-owner lockout live IN the database, written once, so the answer is authoritative for
 * every caller and this tier never re-implements a role gate. Actor-first like the rest of the
 * DAL (actors are mintable only by the guards), and the call pattern mirrors `inviteMembers`:
 * one `getPool().query` relaying the function's own outcome code VERBATIM to the caller, which
 * maps it to the page's honest copy.
 */

/** The role a workspace seat can hold — the select's domain and the guarded fn's parameter. */
export type SeatRole = "owner" | "reviewer" | "member";

/** The outcome codes `topos_set_member_role` speaks (the database's vocabulary, relayed verbatim). */
export type SetMemberRoleOutcome =
  | "set"
  | "member_required"
  | "owner_role_required"
  | "bad_role"
  | "unknown_member"
  | "sole_owner";

/**
 * Change a seat's role — an OWNER act on any seat (invited seats included; the role rides the
 * seat and survives confirmation). The database refuses demoting the sole confirmed owner
 * (`sole_owner`): a workspace must always have an owner. The web owner-guard on the calling
 * action is defense-in-depth; `topos_set_member_role` re-runs the owner gate itself.
 */
export async function setMemberRole(
  actor: OwnerActor,
  email: string,
  role: SeatRole,
): Promise<SetMemberRoleOutcome> {
  const result = await getPool().query<{ outcome: SetMemberRoleOutcome }>(
    "select topos_set_member_role($1, $2, $3, $4) as outcome",
    [actor.workspaceId, actor.email, email, role],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_set_member_role returned no outcome");
  }
  return outcome;
}

/** The outcome codes `topos_leave_workspace` speaks (the database's vocabulary, relayed verbatim). */
export type LeaveWorkspaceOutcome = "left" | "member_required" | "sole_owner";

/**
 * The signed-in person leaving their OWN seat. A sole confirmed owner cannot leave
 * (`sole_owner` — transfer ownership first). The database runs the lapse-detach reconcile BEFORE
 * the seat delete, so the person's devices freeze their copies with honest detach records, then
 * deletes the seat (`left`). The scope + principal both come from the actor — no wrong-scope leave
 * is representable.
 */
export async function leaveWorkspace(actor: MemberActor): Promise<LeaveWorkspaceOutcome> {
  // The server clock is epoch-MILLISECONDS (the detach freeze time); the audit created_at is TEXT
  // ISO-8601 — the same split every guarded roster write uses.
  const nowMs = Date.now();
  const createdAt = new Date(nowMs).toISOString();
  const result = await getPool().query<{ outcome: LeaveWorkspaceOutcome }>(
    "select topos_leave_workspace($1, $2, $3, $4) as outcome",
    [actor.workspaceId, actor.email, nowMs, createdAt],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_leave_workspace returned no outcome");
  }
  return outcome;
}
