import { and, asc, eq, sql } from "drizzle-orm";
import type { UserActor } from "@/lib/auth/guards.server";
import { revokeOwnDevice, theWorkspace } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { device } from "@/lib/db/schema.app";

/**
 * The ACCOUNT-level device DAL — the "your devices" page's reads and its self sign-out write.
 * A device is a POSSESSION of ONE user now (workspace-less), so this module is scoped by a
 * bare UserActor and discloses ONLY the person's own device rows. Revocation is SELF-ONLY by
 * design — no owner arm reaches into someone else's pocket — and FINAL (the database trigger
 * refuses any un-revoke); re-enrolling through the device flow is the recovery.
 */

/** One device row as the account page renders it. */
export interface AccountDevice {
  deviceId: string;
  displayName: string;
  revoked: boolean;
  /** Epoch-milliseconds, or null when the device has never phoned home. */
  lastSeenAtMs: number | null;
  createdAtMs: number;
}

/** The person's OWN devices, oldest first. */
export async function devicesFor(actor: UserActor): Promise<AccountDevice[]> {
  const rows = await getDb()
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
  return rows.map((r) => ({
    deviceId: r.deviceId,
    displayName: r.displayName,
    revoked: r.revokedAt !== null,
    lastSeenAtMs: r.lastSeenAtMs === null ? null : Number(r.lastSeenAtMs),
    createdAtMs: Number(r.createdAtMs),
  }));
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
  const ws = await theWorkspace();
  const revoked = await revokeOwnDevice(
    { userId: actor.userId, display: actor.display },
    deviceId,
    ws?.id ?? "",
  );
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
