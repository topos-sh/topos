import { sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import type { UserActor } from "@/lib/auth/guards.server";
import { isWorkspaceNameShape } from "@/lib/workspace-name";
import { isReservedWorkspaceName } from "@/topos-web/segments";
import { auditInTx, mintChannelId, mintWorkspaceId, workspaceByName } from "./identity.server";
import { getDb, isUniqueViolation } from "./index.server";
import { auditEvent, channel, seat, workspace } from "./schema.app";

/**
 * The ONE door that mints a self-serve workspace — the multi-tenant counterpart to the
 * single-tenant boot claim (`identity.server.ts`). A signed-in person with a seatless (or any)
 * identity creates a workspace they own; it is born CLAIMED (the creator IS the owner, so there
 * is no claim code), with its default `everyone` channel and an owner seat, all in ONE
 * transaction with the audit row emitted inside it — the same idioms the claim ceremony uses.
 *
 * A RESERVED name (a top-level route/operator segment) and a name-race unique violation return
 * the SAME typed `taken` refusal: the caller — and therefore the form — cannot tell a reserved
 * name from an already-taken one, so the reserved list is never enumerable through creation.
 */

export type CreateWorkspaceResult =
  | { outcome: "created"; workspaceId: string; name: string }
  | { outcome: "taken" }
  /** The composition switched self-serve creation off — the surface does not exist. */
  | { outcome: "off" }
  /** The per-person creation floor tripped — honest and disclosed, unlike `taken`. */
  | { outcome: "rate-limited" };

/**
 * The per-person creation floor: how many workspaces one account may mint in a rolling day.
 * The composition's `limit("workspace-create-per-day")` overrides it; ABSENT here means the
 * built-in floor, NOT unlimited — the one deliberate inversion of the limits convention,
 * because on an open-registration deployment this is an authenticated mint anyone can reach,
 * and slug-squatting is user-visible damage. Counted from the immutable audit trail.
 */
const CREATE_FLOOR_PER_DAY = 10;

/**
 * Whether a name is a free, valid workspace address slug: shape-valid AND not reserved AND not
 * already taken. Reserved and taken are BOTH `false` and indistinguishable — the reserved check
 * and the taken lookup are computed the same way (one indexed lookup when the shape is valid,
 * whether reserved or not), so the two answers cost the same. The live-availability probe and the
 * create action both read this, so their verdicts agree.
 */
export async function workspaceNameAvailable(name: string): Promise<boolean> {
  if (!isWorkspaceNameShape(name)) {
    return false;
  }
  const reserved = isReservedWorkspaceName(name, composition.reservedWorkspaceNames);
  const existing = await workspaceByName(name);
  return !reserved && existing === null;
}

/**
 * Create a workspace owned by `actor`. A reserved name and a taken name pay the SAME cost —
 * one indexed lookup, exactly what the availability probe pays — and return the SAME typed
 * `taken`, so neither the response nor its timing classifies "unavailable" into reserved vs
 * taken. The unique index on `workspace.name` stays the race arbiter, so a create-race loser
 * maps to `taken` too, never a 500.
 */
export async function createWorkspace(
  actor: UserActor,
  input: { name: string; displayName: string },
): Promise<CreateWorkspaceResult> {
  const { name, displayName } = input;
  // Self-defending door: callers validate for UX, but a malformed name must fail typed here,
  // not as a raw CHECK-constraint 500 from the insert below.
  if (!isWorkspaceNameShape(name)) {
    throw new Error("createWorkspace: name is not a valid workspace address slug");
  }
  // The composition may switch self-serve creation off entirely (OSS default: allow-all).
  const entitlements = await composition.entitlements.forWorkspace(null);
  if (!entitlements.allows("workspace-create")) {
    return { outcome: "off" };
  }
  // The rolling-day floor (see CREATE_FLOOR_PER_DAY) — counted from the audit trail, which is
  // append-only and survives the person leaving/deleting workspaces to reset a row count.
  const cap = entitlements.limit("workspace-create-per-day") ?? CREATE_FLOOR_PER_DAY;
  const recent = await getDb().execute(
    sql`SELECT count(*)::int AS n FROM ${auditEvent}
        WHERE kind = 'workspace_created' AND outcome = 'ok'
          AND actor_user_id = ${actor.userId}
          AND created_at > now() - interval '24 hours'`,
  );
  if (((recent.rows[0] as { n: number } | undefined)?.n ?? 0) >= cap) {
    return { outcome: "rate-limited" };
  }
  // The SAME one-read check the availability probe runs, unconditionally — reserved is
  // combined AFTER the read so both refusals cost one indexed lookup.
  const reserved = isReservedWorkspaceName(name, composition.reservedWorkspaceNames);
  const existing = await workspaceByName(name);
  if (reserved || existing !== null) {
    return { outcome: "taken" };
  }
  const workspaceId = mintWorkspaceId();
  try {
    await getDb().transaction(async (tx) => {
      await tx.insert(workspace).values({
        id: workspaceId,
        name,
        displayName,
        // Born CLAIMED: the creator is the owner, so no claim code is ever minted (the workspace
        // claim-state CHECK ties claimed_at set ⇔ claim_code_sha256 null). now() is the DB clock.
        claimedAt: sql`now()` as never,
      });
      await tx.insert(channel).values({
        id: mintChannelId(),
        workspaceId,
        name: "everyone",
        isDefault: true,
      });
      await tx.insert(seat).values({ workspaceId, userId: actor.userId, role: "owner" });
      await auditInTx(tx, {
        workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "workspace_created",
        subject: name,
        outcome: "ok",
      });
    });
  } catch (error) {
    if (isUniqueViolation(error)) {
      return { outcome: "taken" };
    }
    throw error;
  }
  return { outcome: "created", workspaceId, name };
}
