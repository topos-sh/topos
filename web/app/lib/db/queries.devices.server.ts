import { and, asc, eq, sql } from "drizzle-orm";
import type { UserActor } from "@/lib/auth/guards.server";
import { revokeOwnDevice, selfUnlinkDevice } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { device, deviceLink, workspace } from "@/lib/db/schema.app";

/**
 * The ACCOUNT-level device DAL — the "your devices" page's reads, its self sign-out write, and
 * the per-link SELF unlink. A device is REGISTERED to ONE user (device ↔ server) and LINKED
 * per workspace, so this module is scoped by a bare UserActor and discloses ONLY the person's
 * own device rows — each carrying its linked-workspace list. Revocation is SELF-ONLY by
 * design — no owner arm reaches into someone else's pocket — and FINAL (the database trigger
 * refuses any un-revoke); re-enrolling through the device flow is the recovery.
 */

/** One device↔workspace link as the account page renders it. */
export interface AccountDeviceLink {
  workspaceId: string;
  workspaceName: string;
  workspaceDisplayName: string;
  status: "active" | "pending";
}

/** One device row as the account page renders it. */
export interface AccountDevice {
  deviceId: string;
  displayName: string;
  revoked: boolean;
  /** Epoch-milliseconds, or null when the device has never phoned home. */
  lastSeenAtMs: number | null;
  createdAtMs: number;
  /** The workspaces this device is linked to (a revoked device holds none — severed). */
  links: AccountDeviceLink[];
}

/** The person's OWN devices, oldest first, each with its linked-workspace list. */
export async function devicesFor(actor: UserActor): Promise<AccountDevice[]> {
  const db = getDb();
  const rows = await db
    .select({
      deviceId: device.id,
      displayName: device.displayName,
      revokedAt: device.revokedAt,
      lastSeenAtMs: sql<string | null>`(extract(epoch from ${device.lastSeenAt}) * 1000)::bigint`,
      createdAtMs: sql<string>`(extract(epoch from ${device.createdAt}) * 1000)::bigint`,
    })
    .from(device)
    .where(eq(device.userId, actor.userId))
    .orderBy(asc(device.createdAt), asc(device.id));
  const linkRows = await db
    .select({
      deviceId: deviceLink.deviceId,
      workspaceId: deviceLink.workspaceId,
      workspaceName: workspace.name,
      workspaceDisplayName: workspace.displayName,
      status: deviceLink.status,
    })
    .from(deviceLink)
    .innerJoin(device, eq(device.id, deviceLink.deviceId))
    .innerJoin(workspace, eq(workspace.id, deviceLink.workspaceId))
    .where(eq(device.userId, actor.userId))
    .orderBy(asc(workspace.name), asc(deviceLink.workspaceId));
  const linksByDevice = new Map<string, AccountDeviceLink[]>();
  for (const link of linkRows) {
    const entry: AccountDeviceLink = {
      workspaceId: link.workspaceId,
      workspaceName: link.workspaceName,
      workspaceDisplayName: link.workspaceDisplayName,
      status: link.status as AccountDeviceLink["status"],
    };
    const list = linksByDevice.get(link.deviceId);
    if (list === undefined) {
      linksByDevice.set(link.deviceId, [entry]);
    } else {
      list.push(entry);
    }
  }
  return rows.map((r) => ({
    deviceId: r.deviceId,
    displayName: r.displayName,
    revoked: r.revokedAt !== null,
    lastSeenAtMs: r.lastSeenAtMs === null ? null : Number(r.lastSeenAtMs),
    createdAtMs: Number(r.createdAtMs),
    links: linksByDevice.get(r.deviceId) ?? [],
  }));
}

export type UnlinkOutcome = "unlinked" | "unknown_link";

/**
 * SELF unlink — sever ONE of the actor's own device links (the page's per-link arm). Self-only
 * by the ceremony's WHERE clause (a foreign device id matches nothing — the same answer an
 * unknown one gets); the `device_unlinked` audit row (cause: self) rides the same transaction.
 */
export async function unlinkOwnDevice(
  actor: UserActor,
  deviceId: string,
  workspaceId: string,
): Promise<UnlinkOutcome> {
  return await selfUnlinkDevice(
    { userId: actor.userId, display: actor.display },
    deviceId,
    workspaceId,
  );
}

export type SignOutOutcome = "revoked" | "unknown_device";

/**
 * Sign one of the actor's OWN devices out — self-only by the WHERE clause itself (a foreign
 * device id simply matches nothing: the same answer as an unknown one, no oracle). Effective
 * immediately and final; the audit row rides the same transaction. IDEMPOTENT: re-revoking a
 * device the actor already signed out answers `revoked` again (a retried logout must not read
 * as a miss).
 */
export async function signOutDevice(actor: UserActor, deviceId: string): Promise<SignOutOutcome> {
  const revoked = await revokeOwnDevice({ userId: actor.userId, display: actor.display }, deviceId);
  if (revoked) {
    return "revoked";
  }
  const rows = await getDb()
    .select({ revokedAt: device.revokedAt })
    .from(device)
    .where(and(eq(device.id, deviceId), eq(device.userId, actor.userId)))
    .limit(1);
  return rows[0]?.revokedAt != null ? "revoked" : "unknown_device";
}
