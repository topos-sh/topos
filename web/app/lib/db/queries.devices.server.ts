import { and, asc, eq } from "drizzle-orm";
import type { UserActor } from "@/lib/auth/guards.server";
import type { AdminOutcome } from "@/lib/db/audit.server";
import { getDb, getPool } from "@/lib/db/index.server";
import { adminEvent } from "@/lib/db/schema.app";
import { planeDeviceRegistry, planeWorkspace, planeWorkspaceMember } from "@/lib/db/schema.plane";

/**
 * The ACCOUNT-level device DAL — the "your devices" page's reads and its self sign-out write.
 * Like the rest of the DAL (queries.server.ts) it lives under app/lib/db/, so raw drizzle/schema
 * imports are sanctioned here (scripts/check-boundary.mjs forbids them elsewhere); every function
 * takes the guard-minted actor whose authority it exercises as its first argument.
 *
 * This module is the ONE place a DAL function is scoped by a bare UserActor rather than a
 * workspace-admitted MemberActor — and that is deliberately safe: the reads and the write below
 * disclose or touch ONLY the person's own rows, keyed on their VERIFIED email (the same email the
 * roster gates on). There is no single-workspace admission because the page spans every workspace
 * the person belongs to; the disclosure stays the person's own devices, and the sign-out is
 * re-gated by the database's own owner-or-self matrix.
 */

/** One device_registry row as the account page renders it. */
export interface AccountDevice {
  deviceKeyId: string;
  revoked: boolean;
  /** BIGINT epoch-milliseconds off the registry row, or null — `new Date(ms)` at the render edge. */
  lastReportAtMs: number | null;
}

/** The person's devices in ONE workspace they hold a confirmed seat in — a render group. */
export interface WorkspaceDevices {
  workspaceId: string;
  displayName: string;
  /** The workspace's address slug — what enrolling a new device speaks. */
  address: string;
  devices: AccountDevice[];
}

/**
 * The person's OWN device_registry rows across EVERY workspace where their email holds a CONFIRMED
 * seat, grouped per workspace for render. The join pins device.principal to the confirmed seat's
 * principal AND to the actor's own email, so a row is disclosed only when it is BOTH the person's
 * own device AND in a workspace they are confirmed in — no other person's devices, no workspace the
 * person is only invited to (or absent from). Display name + address come from `plane.workspace`
 * with the workspace id as the honest fallback (a seat can outlive its workspace row). Workspaces
 * holding none of the person's devices contribute no group (the page shows the empty state instead).
 */
export async function devicesFor(actor: UserActor): Promise<WorkspaceDevices[]> {
  const rows = await getDb()
    .select({
      workspaceId: planeWorkspaceMember.workspaceId,
      displayName: planeWorkspace.displayName,
      address: planeWorkspace.name,
      deviceKeyId: planeDeviceRegistry.deviceKeyId,
      revoked: planeDeviceRegistry.revoked,
      lastReportAt: planeDeviceRegistry.lastReportAt,
    })
    .from(planeWorkspaceMember)
    .innerJoin(
      planeDeviceRegistry,
      and(
        eq(planeDeviceRegistry.workspaceId, planeWorkspaceMember.workspaceId),
        eq(planeDeviceRegistry.principal, planeWorkspaceMember.principal),
      ),
    )
    .leftJoin(planeWorkspace, eq(planeWorkspace.workspaceId, planeWorkspaceMember.workspaceId))
    .where(
      and(
        eq(planeWorkspaceMember.principal, actor.email),
        eq(planeWorkspaceMember.status, "confirmed"),
      ),
    )
    .orderBy(asc(planeWorkspaceMember.workspaceId), asc(planeDeviceRegistry.deviceKeyId));

  // Group per workspace, preserving the (workspace id, device key id) order the query established.
  const groups = new Map<string, WorkspaceDevices>();
  for (const row of rows) {
    let group = groups.get(row.workspaceId);
    if (group === undefined) {
      group = {
        workspaceId: row.workspaceId,
        displayName: row.displayName ?? row.workspaceId,
        address: row.address ?? row.workspaceId,
        devices: [],
      };
      groups.set(row.workspaceId, group);
    }
    group.devices.push({
      deviceKeyId: row.deviceKeyId,
      revoked: row.revoked === 1,
      lastReportAtMs: row.lastReportAt,
    });
  }
  return [...groups.values()];
}

/** The outcome codes `topos_revoke_device` speaks (the database's vocabulary, relayed verbatim). */
export type SignOutOutcome =
  | "revoked"
  | "unknown_device"
  | "owner_or_self_required"
  | "member_required";

/**
 * Sign one device out — ONE call to the guarded `topos_revoke_device`, which re-runs its own
 * role matrix (member gate, then owner-or-self on the target device). The web guard on the action
 * is a signed-in-actor check, never the lock: a self sign-out is legal in ANY workspace where the
 * actor holds a confirmed seat because the function's OWN matrix admits the device's own principal
 * regardless of role. The outcome code is the function's vocabulary, relayed to the caller as-is.
 */
export async function signOutDevice(
  actor: UserActor,
  workspaceId: string,
  deviceKeyId: string,
): Promise<SignOutOutcome> {
  const result = await getPool().query<{ outcome: SignOutOutcome }>(
    "select topos_revoke_device($1, $2, $3) as outcome",
    [workspaceId, actor.email, deviceKeyId],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_revoke_device returned no outcome");
  }
  return outcome;
}

/**
 * Record the self sign-out in the web tier's admin audit — ONE row per attempt, whatever the
 * outcome (mirroring recordAdminEvent in audit.server.ts). This is the ONE admin_event write that
 * lives OUTSIDE audit.server.ts, and by necessity: recordAdminEvent takes a MemberActor, whose
 * brand is module-private to guards.server.ts, and this ACCOUNT-level page never admits into a
 * single workspace — it holds only a UserActor. So the row is written here directly (raw drizzle
 * is sanctioned under app/lib/db/) with set_by = the actor's verified email and workspace_id = the
 * TARGET workspace, kind `device_revoke`, subject the device key id, detail "self". Best-effort by
 * design: an audit fault must never mask the sign-out's own outcome.
 */
export async function recordSelfDeviceRevoke(
  actor: UserActor,
  workspaceId: string,
  deviceKeyId: string,
  outcome: AdminOutcome,
): Promise<void> {
  try {
    await getDb().insert(adminEvent).values({
      workspaceId,
      kind: "device_revoke",
      subject: deviceKeyId,
      detail: "self",
      setBy: actor.email,
      outcome,
    });
  } catch (error) {
    console.error("admin_event insert failed", error);
  }
}
