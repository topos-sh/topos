import { and, asc, eq, inArray, sql } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
import { personDisplayLeftSql } from "@/lib/db/person-display.server";
import {
  bundle,
  bundleDetachment,
  device,
  deviceBundleState,
  deviceExclusion,
  seat,
  workspace,
} from "@/lib/db/schema.app";
import { user } from "@/lib/db/schema.auth";
import { planeCurrentPointer } from "@/lib/db/schema.custody";

/**
 * The FLEET data access layer — the enrolled devices of the workspace and the version each one
 * last reported, joined against the custody pointer for staleness. Actor-first like the rest
 * of the DAL.
 *
 * The fleet page is a visibility surface, not a cryptographic one: after an owner publishes a
 * scrubbed version they watch this page until every non-stale device is clean, and the copies
 * that are past the window, detached, excluded, or on a removed member's device are ENUMERATED
 * for a human to chase — never silently omitted. The reconcile only UPSERTS state rows, so a
 * row whose bundle left the device's install set is the frozen "last known" record; this layer
 * DERIVES the blind-spot label by joining the person's detach records and the device's
 * exclusions over it. Revocation is SELF-ONLY now (a device is a possession) — this page
 * carries NO revoke arm; your-devices is the one place a device signs out.
 */

/** How this device's copy of one bundle sits against the workspace's current pointer. */
export type FleetSkillStatus = "current" | "behind" | "detached" | "excluded" | "removed_upstream";

/** One (device × bundle) applied-state row, joined to the catalog and the current pointer. */
export interface FleetSkillState {
  skillId: string;
  /** The catalog name, or null when the id is no longer cataloged (a purged tombstone). */
  skillName: string | null;
  skillStatus: "active" | "archived" | "deleted" | null;
  /** The version this device last applied. */
  appliedVersionId: string;
  /** The workspace's current version, or null when nothing is published (or withdrawn). */
  currentVersionId: string | null;
  status: FleetSkillStatus;
  /** When this row was last reported (epoch-ms). */
  reportedAtMs: number;
  /** The person's detach record's cause, when one names this copy. */
  detachCause: string | null;
}

/** How fresh a device's last report is against the workspace staleness window. */
export type FleetFreshness = "fresh" | "stale" | "never";

/** One enrolled device: its owner, its liveness, and its per-bundle applied state. */
export interface FleetDevice {
  deviceId: string;
  displayName: string;
  /** The owning person (display + login address — attribution, never an authority key). */
  ownerDisplay: string;
  ownerEmail: string;
  ownerUserId: string;
  revoked: boolean;
  /** The device's last-seen time (epoch-ms), or null when it has never phoned home. */
  lastSeenAtMs: number | null;
  freshness: FleetFreshness;
  /**
   * The device's owner holds NO seat — a removed (or departed) member's device. Removal
   * deletes the seat but keeps the device + its applied state by design: these are the copies
   * still out there on a machine nobody administers anymore.
   */
  removedUpstream: boolean;
  /** The bundles this device last reported, catalog-name order. */
  skills: FleetSkillState[];
}

export interface Fleet {
  devices: FleetDevice[];
  /** The workspace's staleness window (ms) — the ONE clock, never re-derived here. */
  stalenessWindowMs: number;
  /** Whether the actor sees the WHOLE fleet (reviewer/owner) or only their own devices. */
  wholeFleet: boolean;
}

function freshnessOf(lastSeenAtMs: number | null, windowMs: number, now: number): FleetFreshness {
  if (lastSeenAtMs === null) {
    return "never";
  }
  return now - lastSeenAtMs <= windowMs ? "fresh" : "stale";
}

/**
 * The workspace's fleet for THIS actor. Role scoping lives here: a plain member sees only
 * their own devices; a reviewer or owner sees the whole fleet. Devices whose owner holds no
 * seat are INCLUDED and marked `removedUpstream` — that IS the blind-spot data.
 */
export async function fleetOf(actor: MemberActor): Promise<Fleet> {
  const ws = actor.workspaceId;
  const wholeFleet = actor.role !== "member";
  const now = Date.now();
  const db = getDb();

  const wsRows = await db
    .select({ stalenessWindowMs: workspace.stalenessWindowMs })
    .from(workspace)
    .where(eq(workspace.id, ws))
    .limit(1);
  const stalenessWindowMs = wsRows[0]?.stalenessWindowMs ?? 604800000;

  // Every device that TOUCHES this workspace: its owner holds a seat, OR it has reported
  // state against one of this workspace's bundles (the removed-member blind spot).
  const deviceRows = await db.execute(sql`
    SELECT DISTINCT d.id, d.display_name, d.user_id, d.revoked_at,
           (extract(epoch from d.last_seen_at) * 1000)::bigint AS last_seen_ms,
           -- The display rule (app/lib/person-display.ts): a blank name falls back to the email.
           COALESCE(NULLIF(btrim(u.name), ''), u.email) AS owner_display, u.email AS owner_email,
           (s.user_id IS NOT NULL) AS seated
    FROM web.device d
    JOIN web."user" u ON u.id = d.user_id
    LEFT JOIN web.seat s ON s.workspace_id = ${ws} AND s.user_id = d.user_id
    WHERE (s.user_id IS NOT NULL
           OR EXISTS (SELECT 1 FROM web.device_bundle_state st
                      JOIN web.bundle b ON b.id = st.bundle_id
                      WHERE st.device_id = d.id AND b.workspace_id = ${ws}))
      AND (${wholeFleet} OR d.user_id = ${actor.userId})
    ORDER BY u.email, d.id
  `);
  const devices = deviceRows.rows as {
    id: string;
    display_name: string;
    user_id: string;
    revoked_at: string | null;
    last_seen_ms: string | null;
    owner_display: string;
    owner_email: string;
    seated: boolean;
  }[];
  if (devices.length === 0) {
    return { devices: [], stalenessWindowMs, wholeFleet };
  }

  const deviceIds = devices.map((d) => d.id);
  const ownerByDevice = new Map(devices.map((d) => [d.id, d.user_id]));
  const stateRows = await db
    .select({
      deviceId: deviceBundleState.deviceId,
      skillId: deviceBundleState.bundleId,
      appliedVersionId: deviceBundleState.appliedVersionId,
      reportedAtMs: sql<string>`(extract(epoch from ${deviceBundleState.reportedAt}) * 1000)::bigint`,
      skillName: bundle.name,
      skillStatus: bundle.status,
      currentVersionId: planeCurrentPointer.versionId,
      excluded: sql<boolean>`EXISTS (
        SELECT 1 FROM ${deviceExclusion} dx
        WHERE dx.device_id = ${deviceBundleState.deviceId} AND dx.bundle_id = ${deviceBundleState.bundleId}
      )`,
      detachCause: bundleDetachment.cause,
    })
    .from(deviceBundleState)
    .innerJoin(bundle, and(eq(bundle.id, deviceBundleState.bundleId), eq(bundle.workspaceId, ws)))
    // The device join must PRECEDE the detachment join: the latter's ON clause correlates on
    // the owning user, and SQL join order is the FROM clause's order.
    .innerJoin(device, eq(device.id, deviceBundleState.deviceId))
    .leftJoin(
      planeCurrentPointer,
      and(
        eq(planeCurrentPointer.workspaceId, ws),
        eq(planeCurrentPointer.bundleId, deviceBundleState.bundleId),
      ),
    )
    .leftJoin(
      bundleDetachment,
      and(
        eq(bundleDetachment.workspaceId, ws),
        eq(bundleDetachment.bundleId, deviceBundleState.bundleId),
        eq(bundleDetachment.userId, device.userId),
      ),
    )
    .where(inArray(deviceBundleState.deviceId, deviceIds))
    .orderBy(asc(deviceBundleState.deviceId), asc(bundle.name));

  const seatedOwners = new Set(devices.filter((d) => d.seated).map((d) => d.user_id));
  const statesByDevice = new Map<string, FleetSkillState[]>();
  for (const row of stateRows) {
    const ownerSeated = seatedOwners.has(ownerByDevice.get(row.deviceId) ?? "");
    let status: FleetSkillStatus;
    if (!ownerSeated) {
      status = "removed_upstream";
    } else if (row.detachCause !== null) {
      status = "detached";
    } else if (row.excluded) {
      status = "excluded";
    } else if (row.currentVersionId !== null && row.appliedVersionId === row.currentVersionId) {
      status = "current";
    } else {
      status = "behind";
    }
    const state: FleetSkillState = {
      skillId: row.skillId,
      skillName: row.skillName,
      skillStatus: row.skillStatus as FleetSkillState["skillStatus"],
      appliedVersionId: row.appliedVersionId,
      currentVersionId: row.currentVersionId,
      status,
      reportedAtMs: Number(row.reportedAtMs),
      detachCause: row.detachCause,
    };
    const list = statesByDevice.get(row.deviceId);
    if (list === undefined) {
      statesByDevice.set(row.deviceId, [state]);
    } else {
      list.push(state);
    }
  }

  return {
    devices: devices.map((d) => ({
      deviceId: d.id,
      displayName: d.display_name,
      ownerDisplay: d.owner_display,
      ownerEmail: d.owner_email,
      ownerUserId: d.user_id,
      revoked: d.revoked_at !== null,
      lastSeenAtMs: d.last_seen_ms === null ? null : Number(d.last_seen_ms),
      freshness: freshnessOf(
        d.last_seen_ms === null ? null : Number(d.last_seen_ms),
        stalenessWindowMs,
        now,
      ),
      removedUpstream: !d.seated,
      skills: statesByDevice.get(d.id) ?? [],
    })),
    stalenessWindowMs,
    wholeFleet,
  };
}

/** The people whose seats are gone but whose detach records still name copies — a named blind
 * spot even when the device rows themselves are revoked or quiet. */
export interface DetachedCopyRow {
  userId: string;
  display: string;
  bundleId: string;
  bundleName: string | null;
  cause: string;
  createdAt: Date;
}

/** Every standing detach record in the workspace, person-joined — the fleet's chase list. */
export async function detachedCopiesOf(actor: MemberActor): Promise<DetachedCopyRow[]> {
  const rows = await getDb()
    .select({
      userId: bundleDetachment.userId,
      display: personDisplayLeftSql(user),
      bundleId: bundleDetachment.bundleId,
      bundleName: bundle.name,
      cause: bundleDetachment.cause,
      createdAt: bundleDetachment.createdAt,
    })
    .from(bundleDetachment)
    .leftJoin(user, eq(user.id, bundleDetachment.userId))
    .leftJoin(
      bundle,
      and(
        eq(bundle.workspaceId, bundleDetachment.workspaceId),
        eq(bundle.id, bundleDetachment.bundleId),
      ),
    )
    .where(eq(bundleDetachment.workspaceId, actor.workspaceId))
    .orderBy(asc(bundleDetachment.createdAt));
  return rows.map((r) => ({ ...r, display: r.display ?? "former member" }));
}

/** Whether the actor holds any seat rows at all — the fleet page's empty-state probe. */
export async function workspaceHasDevices(actor: MemberActor): Promise<boolean> {
  const rows = await getDb()
    .select({ id: device.id })
    .from(device)
    .innerJoin(seat, and(eq(seat.workspaceId, actor.workspaceId), eq(seat.userId, device.userId)))
    .limit(1);
  return rows.length > 0;
}
