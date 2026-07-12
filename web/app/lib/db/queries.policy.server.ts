import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { getPool } from "@/lib/db/index.server";

/**
 * The WORKSPACE-POLICY data access — the invite policy and the staleness window, the two knobs
 * that join the review-required default on the settings page. Every read goes through the
 * directory's guarded accessor functions (`topos_invite_policy` / `topos_staleness_window`), which
 * COALESCE a missing `workspace_policy` row to the ONE canonical default — so this tier never
 * re-derives "members" or 604800000 in TypeScript, and a workspace with no policy row shows the
 * true default rather than a blank. Every write goes through the guarded setter functions
 * (`topos_set_invite_policy` / `topos_set_staleness_window`), which re-run the owner gate INSIDE
 * the database — the OwnerActor on the call is the web tier's matching lock, never the only one.
 * The outcome codes are the functions' own vocabulary, relayed verbatim.
 *
 * Actor-first like the rest of the DAL: workspace-scoped reads take their scope FROM the actor, so
 * a wrong-scope actor cannot leak another workspace's policy.
 */

/** The invite policy a missing row falls back to is 'members' — decided in SQL, read here. */
export async function invitePolicyOf(actor: MemberActor): Promise<"members" | "owners"> {
  const result = await getPool().query<{ policy: "members" | "owners" }>(
    "select topos_invite_policy($1) as policy",
    [actor.workspaceId],
  );
  const policy = result.rows[0]?.policy;
  if (policy === undefined) {
    throw new Error("topos_invite_policy returned no row");
  }
  return policy;
}

/**
 * The fleet's staleness window in milliseconds (a bigint). `pg` hands a bigint back as a string,
 * so parse it at the edge — the default 604800000 (7 days) and the 366-day ceiling both sit well
 * inside `Number.MAX_SAFE_INTEGER`, so a plain `Number` is exact.
 */
export async function stalenessWindowOf(actor: MemberActor): Promise<number> {
  const result = await getPool().query<{ window_ms: string }>(
    "select topos_staleness_window($1) as window_ms",
    [actor.workspaceId],
  );
  const raw = result.rows[0]?.window_ms;
  if (raw === undefined || raw === null) {
    throw new Error("topos_staleness_window returned no row");
  }
  return Number(raw);
}

/** The outcome codes `topos_set_invite_policy` speaks (the database's vocabulary, relayed verbatim). */
export type InvitePolicyOutcome = "set" | "member_required" | "owner_role_required" | "bad_policy";

/**
 * Set who may invite: 'members' (any confirmed member) or 'owners' (owners only). The guarded
 * function re-runs the owner gate itself; this DAL is a thin relay — it does NOT pre-validate the
 * policy string, so an unexpected value comes back as the function's own `bad_policy`.
 */
export async function setInvitePolicy(
  actor: OwnerActor,
  policy: "members" | "owners",
): Promise<InvitePolicyOutcome> {
  const result = await getPool().query<{ outcome: InvitePolicyOutcome }>(
    "select topos_set_invite_policy($1, $2, $3) as outcome",
    [actor.workspaceId, actor.email, policy],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_set_invite_policy returned no outcome");
  }
  return outcome;
}

/** The outcome codes `topos_set_staleness_window` speaks. */
export type StalenessWindowOutcome =
  | "set"
  | "member_required"
  | "owner_role_required"
  | "bad_window";

/**
 * Set the fleet's staleness window in milliseconds. The guarded function bounds it (1ms .. 366d)
 * and refuses anything outside as `bad_window`; the OwnerActor is the web tier's lock, the
 * database's own owner gate the authoritative one.
 */
export async function setStalenessWindow(
  actor: OwnerActor,
  windowMs: number,
): Promise<StalenessWindowOutcome> {
  const result = await getPool().query<{ outcome: StalenessWindowOutcome }>(
    "select topos_set_staleness_window($1, $2, $3) as outcome",
    [actor.workspaceId, actor.email, windowMs],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_set_staleness_window returned no outcome");
  }
  return outcome;
}
