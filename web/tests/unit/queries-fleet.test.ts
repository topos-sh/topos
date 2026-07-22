import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
  asOwner,
  bootWorkspace,
  createScratchDb,
  linkDevice,
  placeInDefault,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedDevice,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * The FLEET DAL (queries.fleet.server.ts) against a REAL scratch Postgres — DEVICE-LINK-driven:
 * the fleet enumerates the workspace's link rows (active AND pending), the version each linked
 * device last reported, and derives the per-copy labels (detached / excluded / current /
 * behind) from the person's detach records, the device's exclusions, and the custody pointer.
 * Seat removal SEVERS links + reported state in the same fence, so a departed member's device
 * simply no longer appears — no ghost rows to enumerate. Role scoping lives here too: a member
 * sees only their own devices.
 */

let db: ScratchDb;
let wsId = "";

async function q() {
  return import("@/lib/db/queries.fleet.server");
}

async function report(deviceId: string, bundleId: string, appliedVersionId: string): Promise<void> {
  await db.q(
    `INSERT INTO web.device_bundle_state (device_id, bundle_id, applied_version_id)
     VALUES ($1, $2, $3)`,
    [deviceId, bundleId, appliedVersionId],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_fleet");
  wsId = await bootWorkspace();
  await seedUser(db, "u_own", "Owner", "own@example.com");
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seedUser(db, "u_gone", "Gone", "gone@example.com");
  await seatUser(db, wsId, "u_own", "owner");
  await seatUser(db, wsId, "u_mem", "member");
  await seatUser(db, wsId, "u_gone", "member");

  // Bundles: all pointered; s_cur rides the default channel (everyone's entitlement).
  await seedBundle(db, wsId, "s_cur", "current-skill");
  await placeInDefault(db, wsId, "s_cur");
  await seedBundle(db, wsId, "s_beh", "behind-skill");
  await seedBundle(db, wsId, "s_det", "detached-skill");
  await seedBundle(db, wsId, "s_exc", "excluded-skill");

  // Devices + links: the member's (fresh, active), the owner's (stale, active), a second
  // member device with a PENDING link, and the leaver's (severed below).
  await seedDevice(db, "dk_mem", "u_mem", "mem-laptop");
  await linkDevice(db, "dk_mem", wsId);
  await db.q(`UPDATE web.device SET last_seen_at = now() WHERE id = 'dk_mem'`);
  await seedDevice(db, "dk_own", "u_own", "own-laptop");
  await linkDevice(db, "dk_own", wsId);
  await db.q(`UPDATE web.device SET last_seen_at = now() - interval '8 days' WHERE id = 'dk_own'`);
  await seedDevice(db, "dk_mem_pend", "u_mem", "mem-second-box");
  await linkDevice(db, "dk_mem_pend", wsId, "pending");
  await seedDevice(db, "dk_gone", "u_gone", "gone-laptop");
  await linkDevice(db, "dk_gone", wsId);

  // Applied state: current / behind / detached / excluded on the member's device.
  await report("dk_mem", "s_cur", versionIdFor("s_cur"));
  await report("dk_mem", "s_beh", "e".repeat(64));
  await report("dk_mem", "s_det", versionIdFor("s_det"));
  await report("dk_mem", "s_exc", versionIdFor("s_exc"));
  await db.q(
    `INSERT INTO web.bundle_detachment (user_id, workspace_id, bundle_id, cause)
     VALUES ('u_mem', $1, 's_det', 'unfollow')`,
    [wsId],
  );
  await db.q(`INSERT INTO web.device_exclusion (device_id, bundle_id) VALUES ('dk_mem', 's_exc')`);

  // The owner's device is merely behind on the current-channel skill.
  await report("dk_own", "s_cur", "f".repeat(64));

  // The leaver reported a CURRENT copy — then their seat was removed through the REAL
  // ceremony, which now also SEVERS their links + reported state in the same fence.
  await report("dk_gone", "s_cur", versionIdFor("s_cur"));
  const identity = await import("@/lib/db/identity.server");
  const removed = await identity.removeSeat(
    { userId: "u_own", display: "Owner" },
    wsId,
    "u_gone",
    "membership_removed",
  );
  if (removed !== "ok") {
    throw new Error(`seat removal seed failed: ${removed}`);
  }
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("fleetOf — role scoping over the link rows", () => {
  it("a plain member sees only their OWN devices (pending links included)", async () => {
    const queries = await q();
    const fleet = await queries.fleetOf(asMember(wsId, "u_mem"));
    expect(fleet.wholeFleet).toBe(false);
    expect(fleet.devices.map((d) => [d.deviceId, d.linkStatus])).toEqual([
      ["dk_mem", "active"],
      ["dk_mem_pend", "pending"],
    ]);
  });

  it("an owner sees the WHOLE fleet — and the severed leaver's device is GONE, not a ghost", async () => {
    const queries = await q();
    const fleet = await queries.fleetOf(asOwner(wsId, "u_own"));
    expect(fleet.wholeFleet).toBe(true);
    // Owner-email order: mem@ < own@; the removed member's link (and device) no longer lists.
    expect(fleet.devices.map((d) => d.deviceId)).toEqual(["dk_mem", "dk_mem_pend", "dk_own"]);
    expect(fleet.stalenessWindowMs).toBe(604800000);
    expect(fleet.deviceApproval).toBe("off");
  });

  it("seat removal severed the leaver's reported state with the link", async () => {
    const rows = await db.q(`SELECT 1 FROM web.device_bundle_state WHERE device_id = 'dk_gone'`);
    expect(rows).toHaveLength(0);
    const links = await db.q(`SELECT 1 FROM web.device_link WHERE device_id = 'dk_gone'`);
    expect(links).toHaveLength(0);
    const audits = await db.q(
      `SELECT details ->> 'cause' AS cause FROM web.audit_event
       WHERE kind = 'device_unlinked' AND subject = 'dk_gone'`,
    );
    expect(audits).toEqual([{ cause: "seat_removed" }]);
  });
});

describe("fleetOf — freshness against the ONE staleness clock", () => {
  it("fresh within the window, stale past it, never without a report", async () => {
    const queries = await q();
    const fleet = await queries.fleetOf(asOwner(wsId, "u_own"));
    const byId = new Map(fleet.devices.map((d) => [d.deviceId, d]));
    expect(byId.get("dk_mem")?.freshness).toBe("fresh");
    expect(byId.get("dk_own")?.freshness).toBe("stale");
    expect(byId.get("dk_mem_pend")?.freshness).toBe("never");
  });

  it("the workspace window is live: shrinking it flips fresh to stale", async () => {
    const queries = await q();
    await db.q(`UPDATE web.workspace SET staleness_window_ms = 1 WHERE id = $1`, [wsId]);
    try {
      // The member's device last phoned home a beat ago — a 1ms window calls that stale.
      const fleet = await queries.fleetOf(asMember(wsId, "u_mem"));
      expect(fleet.stalenessWindowMs).toBe(1);
      expect(fleet.devices[0]?.freshness).toBe("stale");
    } finally {
      await db.q(`UPDATE web.workspace SET staleness_window_ms = 604800000 WHERE id = $1`, [wsId]);
    }
  });
});

describe("fleetOf — the per-copy statuses", () => {
  it("current vs behind vs detached (cause-tagged) vs excluded, catalog-name order", async () => {
    const queries = await q();
    const fleet = await queries.fleetOf(asMember(wsId, "u_mem"));
    const skills = fleet.devices[0]?.skills ?? [];
    expect(skills.map((s) => [s.skillName, s.status, s.detachCause])).toEqual([
      ["behind-skill", "behind", null],
      ["current-skill", "current", null],
      ["detached-skill", "detached", "unfollow"],
      ["excluded-skill", "excluded", null],
    ]);
    expect(skills.find((s) => s.skillId === "s_cur")?.currentVersionId).toBe(versionIdFor("s_cur"));
    expect(skills.find((s) => s.skillId === "s_beh")?.appliedVersionId).toBe("e".repeat(64));
  });

  it("the device-approval knob rides the fleet read", async () => {
    const queries = await q();
    await db.q(`UPDATE web.workspace SET device_approval = 'on' WHERE id = $1`, [wsId]);
    try {
      const fleet = await queries.fleetOf(asOwner(wsId, "u_own"));
      expect(fleet.deviceApproval).toBe("on");
    } finally {
      await db.q(`UPDATE web.workspace SET device_approval = 'off' WHERE id = $1`, [wsId]);
    }
  });
});

describe("workspaceDeviceCount (the onboarding probe)", () => {
  it("counts live devices with an ACTIVE link — pending and revoked never count", async () => {
    const queries = await q();
    // Active links: dk_mem, dk_own (dk_mem_pend is pending; dk_gone was severed).
    expect(await queries.workspaceDeviceCount(asOwner(wsId, "u_own"))).toBe(2);
  });
});
