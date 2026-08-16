import { sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import type { UserActor } from "@/lib/auth/guards.server";
import { isWorkspaceNameShape } from "@/lib/workspace-name";
import { isReservedWorkspaceName } from "@/topos-web/segments";
import { auditInTx, mintChannelId, mintWorkspaceId, workspaceByName } from "./identity.server";
import { getDb, isUniqueViolation } from "./index.server";
import { assignment, auditEvent, channel, seat, workspace } from "./schema.app";

/**
 * The ONE door that mints a self-serve workspace — the multi-tenant counterpart to the
 * single-tenant boot claim (`identity.server.ts`). A signed-in person with a seatless (or any)
 * identity creates a workspace they own; it is born CLAIMED (the creator IS the owner, so there
 * is no claim code), with its default `everyone` channel, an owner seat, and the baseline
 * assignment that aims that channel at everyone, all in ONE transaction with the audit row
 * emitted inside it — the same idioms the claim ceremony uses. The transaction BODY is the
 * exported `createWorkspaceTx`, so the login approval's create arm can run the IDENTICAL birth
 * inside its own fence; `createWorkspace` wraps it with the policy pre-checks.
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
  | { outcome: "rate-limited" }
  /** The per-person OWNED-workspaces cap tripped — the same honest, disclosed family. */
  | { outcome: "owned-limit" };

/**
 * The per-person creation floor: how many workspaces one account may mint in a rolling day.
 * The composition's `limit("workspace-create-per-day")` overrides it; ABSENT here means the
 * built-in floor, NOT unlimited — the one deliberate inversion of the limits convention,
 * because on an open-registration deployment this is an authenticated mint anyone can reach,
 * and slug-squatting is user-visible damage. Counted from the immutable audit trail.
 */
const CREATE_FLOOR_PER_DAY = 10;

/**
 * The per-person OWNED-workspaces floor: how many workspaces one account may own at once.
 * `limit("workspaces-owned")` (scope `forWorkspace(null)`, like `workspace-create`) overrides
 * it — a present row wins even when lower; ABSENT means this floor, the same inversion as the
 * daily floor and for the same reason. Counted from CURRENT owner seats (rides seat_user_idx),
 * so leaving a workspace really does free a slot.
 */
const OWNED_FLOOR = 3;

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
 * The policy pre-checks `createWorkspace` runs BEFORE its transaction — exported so the
 * /verify approval's create arm can run the SAME gate before entering its own fence
 * (byte-identical policy, one spelling). `ok` clears every gate. The two COUNTED floors
 * (daily + owned) deliberately do NOT live here: a pre-transaction count is a check-then-act
 * race two concurrent submits step through, so both run inside `createWorkspaceTx`, under the
 * per-person advisory lock.
 */
export async function createWorkspacePrecheck(): Promise<"ok" | "off"> {
  // Self-serve creation is a MULTI surface, whatever the entitlements say: single tenancy IS
  // its one boot-minted workspace, and a second row would break every LIMIT-1 resolution of
  // it. `/new` never mounts there, but a crafted POST reaches the shared ceremony — so the
  // surface reads OFF here, once, for every caller.
  if (composition.tenancy !== "multi") {
    return "off";
  }
  // The composition may switch self-serve creation off entirely (OSS default: allow-all).
  const entitlements = await composition.entitlements.forWorkspace(null);
  if (!entitlements.allows("workspace-create")) {
    return "off";
  }
  return "ok";
}

/**
 * The two per-person creation floors, counted INSIDE the birth transaction under the advisory
 * lock — so both doors (`/new` and the /verify create arm) meet the identical, race-free
 * check through the shared tx body. The daily floor counts the append-only audit trail (it
 * survives the person deleting workspaces to reset a row count); the owned floor counts
 * CURRENT owner seats.
 */
async function createFloorsRefusalTx(
  tx: Tx,
  userId: string,
): Promise<"rate-limited" | "owned-limit" | null> {
  const entitlements = await composition.entitlements.forWorkspace(null);
  const dailyCap = entitlements.limit("workspace-create-per-day") ?? CREATE_FLOOR_PER_DAY;
  const recent = await tx.execute(
    sql`SELECT count(*)::int AS n FROM ${auditEvent}
        WHERE kind = 'workspace_created' AND outcome = 'ok'
          AND actor_user_id = ${userId}
          AND created_at > now() - interval '24 hours'`,
  );
  if (((recent.rows[0] as { n: number } | undefined)?.n ?? 0) >= dailyCap) {
    return "rate-limited";
  }
  const ownedCap = entitlements.limit("workspaces-owned") ?? OWNED_FLOOR;
  const owned = await tx.execute(
    sql`SELECT count(*)::int AS n FROM ${seat}
        WHERE user_id = ${userId} AND role = 'owner'`,
  );
  if (((owned.rows[0] as { n: number } | undefined)?.n ?? 0) >= ownedCap) {
    return "owned-limit";
  }
  return null;
}

/** The drizzle transaction handle `createWorkspaceTx` runs under (any caller's tx). */
type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/**
 * The one-transaction workspace BIRTH, inside the CALLER's transaction: the per-person
 * advisory lock + the two counted floors (daily + owned — the TOCTOU-free spelling both
 * doors share), then the claimed workspace row + the default `everyone` channel + the
 * creator's owner seat + the baseline assignment + the `workspace_created` audit row. A
 * RESERVED name returns the typed `taken` before any write; a name RACE surfaces as the
 * unique violation on `workspace.name` — deliberately NOT caught here (a failed statement
 * aborts the enclosing transaction, so the CALLER maps it at its own boundary, exactly as
 * `createWorkspace` does below). `via` tags the audit row when the birth runs inside another
 * ceremony (the login approval's create arm).
 */
export async function createWorkspaceTx(
  tx: Tx,
  actor: { userId: string; display: string },
  input: { name: string; displayName: string },
  opts: { via?: "login" } = {},
): Promise<
  | { outcome: "created"; workspaceId: string }
  | { outcome: "taken" }
  | { outcome: "rate-limited" }
  | { outcome: "owned-limit" }
> {
  const { name, displayName } = input;
  // Self-defending door: callers validate for UX, but a malformed name must fail typed here,
  // not as a raw CHECK-constraint 500 from the insert below.
  if (!isWorkspaceNameShape(name)) {
    throw new Error("createWorkspaceTx: name is not a valid workspace address slug");
  }
  // The per-person lock FIRST, then the counted floors under it: two concurrent submits from
  // one account serialize here, so the second reads the first's rows and the caps hold
  // exactly (released with the transaction; the key hashes the acting user, so different
  // accounts never queue behind each other).
  await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${actor.userId}))`);
  const floored = await createFloorsRefusalTx(tx, actor.userId);
  if (floored !== null) {
    return { outcome: floored };
  }
  if (isReservedWorkspaceName(name, composition.reservedWorkspaceNames)) {
    return { outcome: "taken" };
  }
  const workspaceId = mintWorkspaceId();
  await tx.insert(workspace).values({
    id: workspaceId,
    name,
    displayName,
    // Born CLAIMED: the creator is the owner, so no claim code is ever minted (the workspace
    // claim-state CHECK ties claimed_at set ⇔ claim_code_sha256 null). now() is the DB clock.
    claimedAt: sql`now()` as never,
  });
  const defaultChannelId = mintChannelId();
  await tx.insert(channel).values({
    id: defaultChannelId,
    workspaceId,
    name: "everyone",
    isDefault: true,
  });
  await tx.insert(seat).values({ workspaceId, userId: actor.userId, role: "owner" });
  // The BASELINE, as a row: the default channel assigned to everyone. It is an ordinary
  // assignment, not a rule in the delivery query, so a workspace born without it would
  // deliver nothing. Written after the seat — the creator is its author.
  await tx.insert(assignment).values({
    workspaceId,
    userId: null,
    channelId: defaultChannelId,
    self: false,
    createdBy: actor.userId,
  });
  await auditInTx(tx, {
    workspaceId,
    actor: { userId: actor.userId, display: actor.display },
    kind: "workspace_created",
    subject: name,
    outcome: "ok",
    ...(opts.via === undefined ? {} : { details: { via: opts.via } }),
  });
  return { outcome: "created", workspaceId };
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
  const precheck = await createWorkspacePrecheck();
  if (precheck !== "ok") {
    return { outcome: precheck };
  }
  // The SAME one-read check the availability probe runs, unconditionally — reserved is
  // combined AFTER the read so both refusals cost one indexed lookup.
  const reserved = isReservedWorkspaceName(name, composition.reservedWorkspaceNames);
  const existing = await workspaceByName(name);
  if (reserved || existing !== null) {
    return { outcome: "taken" };
  }
  let workspaceId = "";
  let refused: "taken" | "rate-limited" | "owned-limit" | null = null;
  try {
    await getDb().transaction(async (tx) => {
      const born = await createWorkspaceTx(tx, actor, { name, displayName });
      if (born.outcome !== "created") {
        // A refusal wrote nothing, but roll back anyway — the tx body is shared, and its
        // typed answers must never half-commit whatever a future revision writes first.
        refused = born.outcome;
        throw REFUSED_IN_TX;
      }
      workspaceId = born.workspaceId;
    });
  } catch (error) {
    if (error === REFUSED_IN_TX && refused !== null) {
      return { outcome: refused };
    }
    if (isUniqueViolation(error)) {
      return { outcome: "taken" };
    }
    throw error;
  }
  return { outcome: "created", workspaceId, name };
}

/** Rollback sentinel for the shared tx body's typed refusals inside `createWorkspace`'s own tx. */
const REFUSED_IN_TX = Symbol("workspace-create-refused");
