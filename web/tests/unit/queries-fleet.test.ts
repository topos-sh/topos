import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
  asOwner,
  bootWorkspace,
  createScratchDb,
  placeInDefault,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedDevice,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * The FLEET DAL (queries.fleet.server.ts) against a REAL scratch Postgres. The fleet page is a
 * VISIBILITY surface: the reconcile only upserts state rows, and this layer derives every
 * blind-spot label from the person's detach records, the device's exclusions, and the seat's
 * absence — detached / excluded / removed_upstream are ENUMERATED for a human to chase, never
 * silently omitted. Role scoping lives here too: a member sees only their own devices.
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

  // Devices: the member's (fresh), the owner's (stale), the leaver's (never seen).
  await seedDevice(db, "dk_mem", "u_mem", "mem-laptop");
  await db.q(`UPDATE web.device SET last_seen_at = now() WHERE id = 'dk_mem'`);
  await seedDevice(db, "dk_own", "u_own", "own-laptop");
  await db.q(`UPDATE web.device SET last_seen_at = now() - interval '8 days' WHERE id = 'dk_own'`);
  await seedDevice(db, "dk_gone", "u_gone", "gone-laptop");

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

  // The leaver reported a CURRENT copy — then their seat was removed through the REAL ceremony,
  // which writes the membership_removed detach records for what they were being delivered.
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

/**
 * (These cases once pinned a join-order bug — the detachment join's ON clause correlated on
 * the device table before it was joined; fixed by joining device first.)
 */
describe("fleetOf — role scoping", () => {
  it("a plain member sees only their OWN devices", async () => {
    const queries = await q();
    const fleet = await queries.fleetOf(asMember(wsId, "u_mem"));
    expect(fleet.wholeFleet).toBe(false);
    expect(fleet.devices.map((d) => d.deviceId)).toEqual(["dk_mem"]);
  });

  it("an owner (or reviewer) sees the WHOLE fleet — the removed member's device included", async () => {
    const queries = await q();
    const fleet = await queries.fleetOf(asOwner(wsId, "u_own"));
    expect(fleet.wholeFleet).toBe(true);
    // Owner-email order: gone@ < mem@ < own@.
    expect(fleet.devices.map((d) => d.deviceId)).toEqual(["dk_gone", "dk_mem", "dk_own"]);
    expect(fleet.stalenessWindowMs).toBe(604800000);
  });
});

describe("fleetOf — freshness against the ONE staleness clock", () => {
  it("fresh within the window, stale past it, never without a report", async () => {
    const queries = await q();
    const fleet = await queries.fleetOf(asOwner(wsId, "u_own"));
    const byId = new Map(fleet.devices.map((d) => [d.deviceId, d]));
    expect(byId.get("dk_mem")?.freshness).toBe("fresh");
    expect(byId.get("dk_own")?.freshness).toBe("stale");
    expect(byId.get("dk_gone")?.freshness).toBe("never");
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

describe("fleetOf — the per-copy statuses and the NAMED blind spots", () => {
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

  it("a seatless owner's device rides as removed_upstream — every copy it holds so labelled", async () => {
    const queries = await q();
    const fleet = await queries.fleetOf(asOwner(wsId, "u_own"));
    const gone = fleet.devices.find((d) => d.deviceId === "dk_gone");
    expect(gone?.removedUpstream).toBe(true);
    expect(gone?.ownerEmail).toBe("gone@example.com");
    // removed_upstream OUTRANKS current: the copy matches the pointer, but nobody administers it.
    expect(gone?.skills.map((s) => [s.skillId, s.status])).toEqual([["s_cur", "removed_upstream"]]);
    // Seated owners' devices are never so labelled.
    expect(fleet.devices.find((d) => d.deviceId === "dk_own")?.removedUpstream).toBe(false);
  });
});

describe("detachedCopiesOf (the chase list)", () => {
  it("lists every standing detach record person-joined — the removal ceremony's included", async () => {
    const queries = await q();
    const rows = await queries.detachedCopiesOf(asOwner(wsId, "u_own"));
    expect(rows.map((r) => [r.userId, r.display, r.bundleId, r.bundleName, r.cause])).toEqual([
      ["u_mem", "Member", "s_det", "detached-skill", "unfollow"],
      ["u_gone", "Gone", "s_cur", "current-skill", "membership_removed"],
    ]);
  });
});
