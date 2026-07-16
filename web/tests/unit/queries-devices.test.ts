import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asUser,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedDevice,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The ACCOUNT-level device DAL (queries.devices.server.ts) against a REAL scratch Postgres. A
 * device is a POSSESSION of one user (workspace-less), so the page reads disclose ONLY the
 * actor's own rows, and sign-out is SELF-ONLY by the WHERE clause itself — a foreign device id
 * answers the same as an unknown one (no oracle), while a RETRIED logout of an
 * already-signed-out own device stays `revoked` (idempotent, never a miss).
 */

let db: ScratchDb;
let wsId = "";

async function q() {
  return import("@/lib/db/queries.devices.server");
}

beforeAll(async () => {
  db = await createScratchDb("web_devices");
  wsId = await bootWorkspace();
  await seedUser(db, "u_ana", "Ana", "ana@example.com");
  await seedUser(db, "u_bo", "Bo", "bo@example.com");
  await seatUser(db, wsId, "u_ana", "member");
  await seatUser(db, wsId, "u_bo", "member");
  await seedDevice(db, "dk_ana_1", "u_ana", "laptop");
  await seedDevice(db, "dk_ana_2", "u_ana", "desktop");
  await seedDevice(db, "dk_bo_1", "u_bo", "bo-laptop");
  // Only the laptop has ever phoned home.
  await db.q(`UPDATE web.device SET last_seen_at = now() WHERE id = 'dk_ana_1'`);
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("devicesFor (the your-devices read)", () => {
  it("lists the actor's OWN devices only, oldest first, with liveness facts", async () => {
    const queries = await q();
    const rows = await queries.devicesFor(asUser("u_ana"));
    expect(rows.map((r) => [r.deviceId, r.displayName, r.revoked])).toEqual([
      ["dk_ana_1", "laptop", false],
      ["dk_ana_2", "desktop", false],
    ]);
    expect(typeof rows[0]?.lastSeenAtMs).toBe("number");
    expect(rows[1]?.lastSeenAtMs).toBeNull();
    expect(typeof rows[0]?.createdAtMs).toBe("number");
  });

  it("returns [] for a user with no devices — never another person's rows", async () => {
    const queries = await q();
    await seedUser(db, "u_empty", "Empty", "empty@example.com");
    expect(await queries.devicesFor(asUser("u_empty"))).toEqual([]);
  });
});

describe("signOutDevice (self-only, final, idempotent)", () => {
  it("revokes the actor's own device, audited; the listing shows it revoked", async () => {
    const queries = await q();
    expect(await queries.signOutDevice(asUser("u_ana", "Ana"), "dk_ana_2")).toBe("revoked");
    const rows = await queries.devicesFor(asUser("u_ana"));
    expect(rows.find((r) => r.deviceId === "dk_ana_2")?.revoked).toBe(true);
    const audit = await db.q(
      `SELECT 1 FROM web.audit_event WHERE workspace_id = $1 AND kind = 'device_revoked' AND subject = 'dk_ana_2'`,
      [wsId],
    );
    expect(audit).toHaveLength(1);
  });

  it("a RETRIED sign-out of an already-revoked own device stays revoked — never a miss", async () => {
    const queries = await q();
    expect(await queries.signOutDevice(asUser("u_ana", "Ana"), "dk_ana_2")).toBe("revoked");
  });

  it("a FOREIGN device id and an unknown one answer the same unknown_device (no oracle)", async () => {
    const queries = await q();
    expect(await queries.signOutDevice(asUser("u_ana", "Ana"), "dk_bo_1")).toBe("unknown_device");
    expect(await queries.signOutDevice(asUser("u_ana", "Ana"), "dk_never")).toBe("unknown_device");
    // Bo's device stands untouched.
    const rows = await db.q<{ revoked_at: string | null }>(
      `SELECT revoked_at FROM web.device WHERE id = 'dk_bo_1'`,
    );
    expect(rows[0]?.revoked_at).toBeNull();
  });

  it("the revocation audit lands in the OWNER's seat workspace, never the boot workspace", async () => {
    const queries = await q();
    // A SECOND workspace B (claimed), and a user seated ONLY in B — never in the boot ws A.
    await db.q(
      `INSERT INTO web.workspace (id, name, display_name, claimed_at)
       VALUES ('w_fixd_b', 'wsb-fixd', 'WS B', now())`,
    );
    await seedUser(db, "u_cy", "Cy", "cy@example.com");
    await seatUser(db, "w_fixd_b", "u_cy", "member");
    await seedDevice(db, "dk_cy", "u_cy", "cy-laptop");

    expect(await queries.signOutDevice(asUser("u_cy", "Cy"), "dk_cy")).toBe("revoked");

    // The device_revoked row lands in B (the owner's only seat) — NOT in the boot workspace A,
    // where the actor holds no seat. Zero rows there is the whole point of the fix.
    const inB = await db.q(
      `SELECT 1 FROM web.audit_event
       WHERE kind = 'device_revoked' AND subject = 'dk_cy' AND workspace_id = 'w_fixd_b'`,
    );
    expect(inB).toHaveLength(1);
    const inA = await db.q(
      `SELECT 1 FROM web.audit_event
       WHERE kind = 'device_revoked' AND subject = 'dk_cy' AND workspace_id = $1`,
      [wsId],
    );
    expect(inA).toHaveLength(0);
  });
});
