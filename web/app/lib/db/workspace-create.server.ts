import { sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import type { UserActor } from "@/lib/auth/guards.server";
import { isWorkspaceNameShape } from "@/lib/workspace-name";
import { isReservedWorkspaceName } from "@/topos-web/segments";
import { auditInTx, mintChannelId, mintWorkspaceId, workspaceByName } from "./identity.server";
import { getDb, isUniqueViolation } from "./index.server";
import { channel, seat, workspace } from "./schema.app";

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
  | { outcome: "taken" };

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
 * Create a workspace owned by `actor`. Reserved names refuse BEFORE the transaction with the
 * same typed `taken` a unique violation earns; the unique index on `workspace.name` is the
 * race arbiter, so a create-race loser maps to `taken` too, never a 500.
 */
export async function createWorkspace(
  actor: UserActor,
  input: { name: string; displayName: string },
): Promise<CreateWorkspaceResult> {
  const { name, displayName } = input;
  if (isReservedWorkspaceName(name, composition.reservedWorkspaceNames)) {
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
