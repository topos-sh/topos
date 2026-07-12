import { and, asc, eq, inArray } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import { getDb, getPool } from "@/lib/db/index.server";
import {
  planeCatalog,
  planeCurrent,
  planeDeviceRegistry,
  planeDeviceSkillState,
  planeWorkspaceMember,
} from "@/lib/db/schema.plane";

/**
 * The FLEET data access layer — the enrolled devices of a workspace and the version each one last
 * reported, read straight from the directory's own tables (SELECT-only by grant), plus the ONE
 * guarded revoke write. Every function is actor-first and derives its workspace scope FROM the
 * actor, the same rule the rest of the DAL follows.
 *
 * The fleet page is a visibility surface, not a cryptographic one: after an owner publishes a
 * scrubbed version they watch this page until every non-stale device is clean, and the copies that
 * are past the window, detached, or on a removed member's device are ENUMERATED for a human to
 * chase — never silently omitted. So this layer deliberately keeps its blind spots as data: a
 * detached copy carries its last-known applied version, and a device whose principal no longer
 * holds a confirmed seat is marked `removedUpstream` rather than dropped.
 */

/** How this device's copy of one skill sits against the workspace's current pointer. */
export type FleetSkillStatus = "current" | "behind" | "detached";

/** One (device × skill) applied-state row, joined to the catalog name and the current pointer. */
export interface FleetSkillState {
  /** The immutable custody key the report is written against. */
  skillId: string;
  /** The catalog name, or null when the skill_id is no longer cataloged (deleted identity). */
  skillName: string | null;
  skillStatus: "active" | "archived" | "deleted" | null;
  /** The version this device last applied (hex64), or null (it holds nothing for this skill). */
  appliedCommit: string | null;
  /** The workspace's current version for this skill (hex64), or null when nothing is published. */
  currentCommit: string | null;
  /**
   * `current` (applied == the current pointer), `behind` (applied != current), or `detached` (a
   * FINAL detach record — the copy is frozen as the person unfollowed or left, `detached=1`).
   */
  status: FleetSkillStatus;
  /** When this row was last reported (epoch-ms). */
  reportedAt: number;
  /** When the copy detached (epoch-ms), for a detached row — null otherwise. */
  detachedAt: number | null;
}

/** How fresh a device's last report is against the workspace staleness window. */
export type FleetFreshness = "fresh" | "stale" | "never";

/** One enrolled device: its principal, its liveness, and its per-skill applied state. */
export interface FleetDevice {
  deviceKeyId: string;
  principal: string;
  /** The credential is revoked — the device is signed out; re-enrolling is the recovery. */
  revoked: boolean;
  /** The device's last report time (epoch-ms), or null when it has never reported. */
  lastReportAt: number | null;
  /** `fresh` (reported within the window), `stale` (older), or `never` (no report yet). */
  freshness: FleetFreshness;
  /**
   * The device's principal holds NO confirmed workspace seat — a removed (or departed) member's
   * device. Removal deletes the seat but keeps the device + its applied state by design: these are
   * the copies still out there on a machine nobody administers anymore.
   */
  removedUpstream: boolean;
  /** Whether THIS actor may revoke this device — owner, or the device's own principal (self). */
  canRevoke: boolean;
  /** The skills this device last reported, catalog-name order. */
  skills: FleetSkillState[];
}

export interface Fleet {
  /** Devices in the actor's view — grouped/ordered by principal, then device id. */
  devices: FleetDevice[];
  /** The workspace's staleness window (epoch-ms) — the ONE clock, never re-derived here. */
  stalenessWindowMs: number;
  /** Whether the actor sees the WHOLE fleet (reviewer/owner) or only their own devices (member). */
  wholeFleet: boolean;
}

/** The confirmed-seat roster read stays the authority; a wrong-scope actor never gets here. */
function deriveStatus(
  detached: boolean,
  appliedCommit: string | null,
  currentCommit: string | null,
): FleetSkillStatus {
  if (detached) {
    return "detached";
  }
  if (appliedCommit !== null && appliedCommit === currentCommit) {
    return "current";
  }
  return "behind";
}

function freshnessOf(
  lastReportAt: number | null,
  stalenessWindowMs: number,
  now: number,
): FleetFreshness {
  if (lastReportAt === null) {
    return "never";
  }
  return now - lastReportAt <= stalenessWindowMs ? "fresh" : "stale";
}

/**
 * The workspace's fleet for THIS actor. Role scoping lives here: a plain member sees only their
 * own devices; a reviewer or owner sees the whole fleet (the role rides on the actor). The
 * staleness window is read through the ONE accessor (`topos_staleness_window`) so a missing policy
 * row cannot fork the default between this surface and the client hook. Devices whose principal
 * holds no confirmed seat are INCLUDED and marked `removedUpstream` — that IS the blind-spot data.
 */
export async function fleetOf(actor: MemberActor): Promise<Fleet> {
  const ws = actor.workspaceId;
  const wholeFleet = actor.role !== "member";
  const now = Date.now();

  // The ONE clock home. `topos_staleness_window` COALESCEs a missing policy row to the default;
  // never re-derive that default here (BIGINT comes back as a string over the wire).
  const windowResult = await getPool().query<{ window: string }>(
    "select topos_staleness_window($1) as window",
    [ws],
  );
  const stalenessWindowMs = Number(windowResult.rows[0]?.window ?? 0);

  // The confirmed roster — the set that decides `removedUpstream` (a device off this set is a
  // removed member's, retained on purpose).
  const confirmedRows = await getDb()
    .select({ principal: planeWorkspaceMember.principal })
    .from(planeWorkspaceMember)
    .where(
      and(eq(planeWorkspaceMember.workspaceId, ws), eq(planeWorkspaceMember.status, "confirmed")),
    );
  const confirmed = new Set(confirmedRows.map((r) => r.principal));

  const deviceRows = await getDb()
    .select({
      deviceKeyId: planeDeviceRegistry.deviceKeyId,
      principal: planeDeviceRegistry.principal,
      revoked: planeDeviceRegistry.revoked,
      lastReportAt: planeDeviceRegistry.lastReportAt,
    })
    .from(planeDeviceRegistry)
    .where(
      and(
        eq(planeDeviceRegistry.workspaceId, ws),
        // Member scope: own devices only. Reviewer/owner: the whole fleet.
        ...(wholeFleet ? [] : [eq(planeDeviceRegistry.principal, actor.email)]),
      ),
    )
    .orderBy(asc(planeDeviceRegistry.principal), asc(planeDeviceRegistry.deviceKeyId));

  if (deviceRows.length === 0) {
    return { devices: [], stalenessWindowMs, wholeFleet };
  }

  const deviceKeyIds = deviceRows.map((d) => d.deviceKeyId);
  const stateRows = await getDb()
    .select({
      deviceKeyId: planeDeviceSkillState.deviceKeyId,
      skillId: planeDeviceSkillState.skillId,
      appliedCommit: planeDeviceSkillState.appliedCommit,
      reportedAt: planeDeviceSkillState.reportedAt,
      detached: planeDeviceSkillState.detached,
      detachedAt: planeDeviceSkillState.detachedAt,
      skillName: planeCatalog.name,
      skillStatus: planeCatalog.status,
      currentCommit: planeCurrent.commitId,
    })
    .from(planeDeviceSkillState)
    .leftJoin(
      planeCatalog,
      and(
        eq(planeCatalog.workspaceId, planeDeviceSkillState.workspaceId),
        eq(planeCatalog.skillId, planeDeviceSkillState.skillId),
      ),
    )
    .leftJoin(
      planeCurrent,
      and(
        eq(planeCurrent.workspaceId, planeDeviceSkillState.workspaceId),
        eq(planeCurrent.skillId, planeDeviceSkillState.skillId),
      ),
    )
    .where(
      and(
        eq(planeDeviceSkillState.workspaceId, ws),
        inArray(planeDeviceSkillState.deviceKeyId, deviceKeyIds),
      ),
    )
    .orderBy(asc(planeDeviceSkillState.deviceKeyId), asc(planeDeviceSkillState.skillId));

  const statesByDevice = new Map<string, FleetSkillState[]>();
  for (const row of stateRows) {
    const status = deriveStatus(row.detached === 1, row.appliedCommit, row.currentCommit);
    const state: FleetSkillState = {
      skillId: row.skillId,
      skillName: row.skillName,
      skillStatus: row.skillStatus,
      appliedCommit: row.appliedCommit,
      currentCommit: row.currentCommit,
      status,
      reportedAt: row.reportedAt,
      detachedAt: row.detachedAt,
    };
    const list = statesByDevice.get(row.deviceKeyId);
    if (list === undefined) {
      statesByDevice.set(row.deviceKeyId, [state]);
    } else {
      list.push(state);
    }
  }
  // Present each device's skills in catalog-NAME order (uncataloged rows last, by id).
  for (const list of statesByDevice.values()) {
    list.sort((a, b) => {
      const an = a.skillName ?? `￿${a.skillId}`;
      const bn = b.skillName ?? `￿${b.skillId}`;
      return an < bn ? -1 : an > bn ? 1 : 0;
    });
  }

  const devices: FleetDevice[] = deviceRows.map((d) => ({
    deviceKeyId: d.deviceKeyId,
    principal: d.principal,
    revoked: d.revoked === 1,
    lastReportAt: d.lastReportAt,
    freshness: freshnessOf(d.lastReportAt, stalenessWindowMs, now),
    removedUpstream: !confirmed.has(d.principal),
    // The fn's own matrix is owner-or-self; the control mirrors it, the fn stays the authority.
    canRevoke: actor.role === "owner" || d.principal === actor.email,
    skills: statesByDevice.get(d.deviceKeyId) ?? [],
  }));

  return { devices, stalenessWindowMs, wholeFleet };
}

/** The outcome codes `topos_revoke_device` speaks (relayed verbatim). */
export type RevokeDeviceOutcome =
  | "revoked"
  | "unknown_device"
  | "owner_or_self_required"
  | "member_required";

/**
 * Revoke ONE device's workspace credential — a guarded write: `topos_revoke_device` re-runs the
 * owner-or-self matrix itself (the web guard on the action is defense-in-depth, never the lock).
 * The flip is instant — the device's credential stops working immediately; the registry row and
 * its audit stay, and re-enrolling is the recovery. Idempotent: re-revoking answers `revoked`.
 */
export async function revokeDevice(
  actor: MemberActor,
  deviceKeyId: string,
): Promise<RevokeDeviceOutcome> {
  const result = await getPool().query<{ outcome: RevokeDeviceOutcome }>(
    "select topos_revoke_device($1, $2, $3) as outcome",
    [actor.workspaceId, actor.email, deviceKeyId],
  );
  const outcome = result.rows[0]?.outcome;
  if (outcome === undefined) {
    throw new Error("topos_revoke_device returned no outcome");
  }
  return outcome;
}
