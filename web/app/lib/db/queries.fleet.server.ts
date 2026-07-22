import { and, asc, eq, inArray, sql } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import { getDb } from "@/lib/db/index.server";
import {
  bundle,
  bundleDetachment,
  device,
  deviceBundleState,
  deviceExclusion,
  deviceLink,
  workspace,
} from "@/lib/db/schema.app";
import { planeCurrentPointer } from "@/lib/db/schema.custody";

/**
 * The FLEET data access layer — DEVICE-LINK-driven since the link model landed: the page
 * enumerates the workspace's link rows (a device is registered once, linked per workspace),
 * the version each linked device last reported, and the PENDING links awaiting an owner when
 * the device-approval knob holds them. Actor-first like the rest of the DAL.
 *
 * The fleet page is a visibility surface with OWNER ARMS now: approve/reject a pending link,
 * remove any link (the ceremonies live in identity.server.ts — severing deletes the link and
 * the device's reported state here; bytes already on the machine stay there). Seat removal
 * severs the person's links in the same fence, so a departed member's device simply no longer
 * appears — the old removed-upstream and detached-copies enumerations died with the ghost rows
 * they chased.
 */

/** How this device's copy of one bundle sits against the workspace's current pointer. */
export type FleetSkillStatus = "current" | "behind" | "detached" | "excluded";

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

/** One linked device: its owner, its link, its liveness, and its per-bundle applied state. */
export interface FleetDevice {
  deviceId: string;
  displayName: string;
  /** The owning person (display + login address — attribution, never an authority key). */
  ownerDisplay: string;
  ownerEmail: string;
  ownerUserId: string;
  /** The device↔workspace link's status — 'pending' awaits an owner's approval. */
  linkStatus: "active" | "pending";
  /** When the link was created (epoch-ms). */
  linkedAtMs: number;
  /** The device's last-seen time (epoch-ms), or null when it has never phoned home. */
  lastSeenAtMs: number | null;
  freshness: FleetFreshness;
  /** The bundles this device last reported, catalog-name order. */
  skills: FleetSkillState[];
}

export interface Fleet {
  devices: FleetDevice[];
  /** The workspace's staleness window (ms) — the ONE clock, never re-derived here. */
  stalenessWindowMs: number;
  /** The device-approval knob — 'on' means non-owner links are born pending. */
  deviceApproval: "off" | "on";
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
 * The workspace's fleet for THIS actor — every LINK row (active AND pending; the page splits
 * them). Role scoping lives here: a plain member sees only their own devices; a reviewer or
 * owner sees the whole fleet.
 */
export async function fleetOf(actor: MemberActor): Promise<Fleet> {
  const ws = actor.workspaceId;
  const wholeFleet = actor.role !== "member";
  const now = Date.now();
  const db = getDb();

  const wsRows = await db
    .select({
      stalenessWindowMs: workspace.stalenessWindowMs,
      deviceApproval: workspace.deviceApproval,
    })
    .from(workspace)
    .where(eq(workspace.id, ws))
    .limit(1);
  const stalenessWindowMs = wsRows[0]?.stalenessWindowMs ?? 604800000;
  const deviceApproval = (wsRows[0]?.deviceApproval ?? "off") as "off" | "on";

  const deviceRows = await db.execute(sql`
    SELECT d.id, d.display_name, d.user_id, dl.status AS link_status,
           (extract(epoch from dl.created_at) * 1000)::bigint AS linked_ms,
           (extract(epoch from d.last_seen_at) * 1000)::bigint AS last_seen_ms,
           -- The display rule (app/lib/person-display.ts): a blank name falls back to the email.
           COALESCE(NULLIF(btrim(u.name), ''), u.email) AS owner_display, u.email AS owner_email
    FROM web.device_link dl
    JOIN web.device d ON d.id = dl.device_id
    JOIN web."user" u ON u.id = d.user_id
    WHERE dl.workspace_id = ${ws}
      AND (${wholeFleet} OR d.user_id = ${actor.userId})
    ORDER BY u.email, d.id
  `);
  const devices = deviceRows.rows as {
    id: string;
    display_name: string;
    user_id: string;
    link_status: "active" | "pending";
    linked_ms: string;
    last_seen_ms: string | null;
    owner_display: string;
    owner_email: string;
  }[];
  if (devices.length === 0) {
    return { devices: [], stalenessWindowMs, deviceApproval, wholeFleet };
  }

  const deviceIds = devices.map((d) => d.id);
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

  const statesByDevice = new Map<string, FleetSkillState[]>();
  for (const row of stateRows) {
    let status: FleetSkillStatus;
    if (row.detachCause !== null) {
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
      linkStatus: d.link_status,
      linkedAtMs: Number(d.linked_ms),
      lastSeenAtMs: d.last_seen_ms === null ? null : Number(d.last_seen_ms),
      freshness: freshnessOf(
        d.last_seen_ms === null ? null : Number(d.last_seen_ms),
        stalenessWindowMs,
        now,
      ),
      skills: statesByDevice.get(d.id) ?? [],
    })),
    stalenessWindowMs,
    deviceApproval,
    wholeFleet,
  };
}

/** Live (non-revoked) devices ACTIVELY linked to this workspace — the onboarding probe. */
export async function workspaceDeviceCount(actor: MemberActor): Promise<number> {
  const rows = await getDb()
    .select({ n: sql<number>`count(*)::int` })
    .from(deviceLink)
    .innerJoin(device, eq(device.id, deviceLink.deviceId))
    .where(
      and(
        eq(deviceLink.workspaceId, actor.workspaceId),
        eq(deviceLink.status, "active"),
        sql`${device.revokedAt} IS NULL`,
      ),
    );
  return rows[0]?.n ?? 0;
}
