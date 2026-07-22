import { eq } from "drizzle-orm";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { auditInTx } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { workspace } from "@/lib/db/schema.app";

/**
 * The WORKSPACE-POLICY data access — the settings page's knobs, now plain columns on the app's
 * OWN `web.workspace` row (the old guarded setter functions and the separate policy table are
 * gone; there is exactly one row per install and its DEFAULTs are the canonical fallbacks, so
 * no reader re-derives 604800000 anywhere). Reads take a MemberActor; writes take
 * the OwnerActor brand as the gate (the route re-guards as owner) and land their audit row in the
 * SAME transaction.
 */

export interface WorkspacePolicy {
  stalenessWindowMs: number;
  /** The protection DEFAULT an unpinned bundle inherits (`reviewed` = review-required). */
  protectionDefault: "open" | "reviewed";
  registration: "invite_only" | "open";
  /** The device-approval knob: 'on' → a non-owner's new device link is born pending. */
  deviceApproval: "off" | "on";
}

/** The workspace's policy knobs, one read. */
export async function workspacePolicyOf(actor: MemberActor): Promise<WorkspacePolicy> {
  const rows = await getDb()
    .select({
      stalenessWindowMs: workspace.stalenessWindowMs,
      protectionDefault: workspace.protectionDefault,
      registration: workspace.registration,
      deviceApproval: workspace.deviceApproval,
    })
    .from(workspace)
    .where(eq(workspace.id, actor.workspaceId))
    .limit(1);
  const row = rows[0];
  if (row === undefined) {
    throw new Error("workspace row missing for a member actor");
  }
  return {
    stalenessWindowMs: row.stalenessWindowMs,
    protectionDefault: row.protectionDefault as WorkspacePolicy["protectionDefault"],
    registration: row.registration as WorkspacePolicy["registration"],
    deviceApproval: row.deviceApproval as WorkspacePolicy["deviceApproval"],
  };
}

export async function stalenessWindowOf(actor: MemberActor): Promise<number> {
  return (await workspacePolicyOf(actor)).stalenessWindowMs;
}

/** One owner-gated knob write + its same-transaction audit row. */
async function setKnob(
  actor: OwnerActor,
  values: Partial<typeof workspace.$inferInsert>,
  kind: string,
  subject: string,
): Promise<void> {
  await getDb().transaction(async (tx) => {
    await tx.update(workspace).set(values).where(eq(workspace.id, actor.workspaceId));
    await auditInTx(tx, {
      workspaceId: actor.workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind,
      subject,
      outcome: "ok",
    });
  });
}

export type StalenessWindowOutcome = "set" | "bad_window";

/** The old bound, kept: 1ms .. 366 days. */
const STALENESS_WINDOW_MAX_MS = 31_622_400_000;

/** Set the fleet's staleness window in milliseconds (bounded; refuses anything outside). */
export async function setStalenessWindow(
  actor: OwnerActor,
  windowMs: number,
): Promise<StalenessWindowOutcome> {
  if (!Number.isSafeInteger(windowMs) || windowMs <= 0 || windowMs > STALENESS_WINDOW_MAX_MS) {
    return "bad_window";
  }
  await setKnob(actor, { stalenessWindowMs: windowMs }, "policy_staleness", String(windowMs));
  return "set";
}

export type RegistrationOutcome = "set" | "bad_value";

/**
 * The registration knob — `invite_only` (the default) or `open`. `open` disables the
 * invitation proof: any address may sign itself up. Owner-only; the settings page carries
 * the honest copy.
 */
export async function setRegistration(
  actor: OwnerActor,
  value: string,
): Promise<RegistrationOutcome> {
  if (value !== "invite_only" && value !== "open") {
    return "bad_value";
  }
  await setKnob(actor, { registration: value }, "policy_registration", value);
  return "set";
}

export type DeviceApprovalOutcome = "set" | "bad_value";

/**
 * The device-approval knob — `off` (the default: a member's new device link is born active)
 * or `on` (born pending until an owner approves it on the fleet page). An owner's own act is
 * always its own approval, whatever this says. Owner-only.
 */
export async function setDeviceApproval(
  actor: OwnerActor,
  value: string,
): Promise<DeviceApprovalOutcome> {
  if (value !== "off" && value !== "on") {
    return "bad_value";
  }
  await setKnob(actor, { deviceApproval: value }, "policy_device_approval", value);
  return "set";
}
