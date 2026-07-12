import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The FLEET DAL against a REAL scratch Postgres stood up from the in-repo authority migrations
 * (so `topos_staleness_window` and `topos_revoke_device` are the real guarded functions, and the
 * `device_registry`/`device_skill_state` DDL is exactly what ships). The scratch database is owned
 * by the connecting superuser, so seeding the authority tables directly needs no per-table grant.
 *
 * Actors are minted by CAST (the brand is module-private to guards.server.ts) — the helpers mirror
 * the guards' invariants: normalized emails and the role riding on the seat.
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:postgres@localhost:5439/topos_test";
const SCRATCH = `web_fleet_test_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

const WS = "w_fleet";
const WS_DEFAULT = "w_fleet_default";
const OWNER_EMAIL = "owner@fleet.example.com";
const MEMBER_EMAIL = "member@fleet.example.com";
const GONE_EMAIL = "gone@fleet.example.com";

// One-hour override on WS; WS_DEFAULT carries NO policy row (the 7-day default must show through).
const WINDOW_1H = 3_600_000;
const DEFAULT_WINDOW = 604_800_000;

const CUR_A = "a1".repeat(32);
const OLD_A = "a0".repeat(32);
const CUR_B = "b1".repeat(32);
const OLD_B = "b0".repeat(32);
const CUR_C = "c1".repeat(32);

const member = (ws: string, email = MEMBER_EMAIL): MemberActor =>
  ({ email: email.trim().toLowerCase(), workspaceId: ws, role: "member" }) as MemberActor;
const reviewer = (ws: string, email = MEMBER_EMAIL): MemberActor =>
  ({ email: email.trim().toLowerCase(), workspaceId: ws, role: "reviewer" }) as MemberActor;
const owner = (ws: string, email = OWNER_EMAIL): OwnerActor =>
  ({ email: email.trim().toLowerCase(), workspaceId: ws, role: "owner" }) as OwnerActor;

function scratchUrl(): string {
  const url = new URL(ADMIN_URL);
  url.pathname = `/${SCRATCH}`;
  return url.toString();
}

async function adminQuery(sql: string): Promise<void> {
  const client = new Client({ connectionString: ADMIN_URL });
  await client.connect();
  try {
    await client.query(sql);
  } finally {
    await client.end();
  }
}

async function scratchQuery(sql: string, params: unknown[] = []): Promise<void> {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().query(sql, params);
}

async function q() {
  return import("@/lib/db/queries.fleet.server");
}

const PUBKEY = Buffer.alloc(32, 7);

async function seedWorkspace(ws: string, name: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
     VALUES ($1, $2, 'unverified', 'cloud', '2026-07-01T00:00:00Z', $3)`,
    [ws, name, name],
  );
}

async function seedPolicy(ws: string, stalenessWindowMs: number): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace_policy (workspace_id, review_required, invite_policy, staleness_window_ms)
     VALUES ($1, 0, 'members', $2)`,
    [ws, stalenessWindowMs],
  );
}

async function seedSeat(
  ws: string,
  principal: string,
  role: "owner" | "reviewer" | "member",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, invited_by, added_at)
     VALUES ($1, $2, $3, 'confirmed', NULL, '2026-07-01T00:00:00Z')`,
    [ws, principal, role],
  );
}

async function seedCatalog(ws: string, skillId: string, name: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.catalog (workspace_id, skill_id, name, display_name, status, created_at)
     VALUES ($1, $2, $3, NULL, 'active', '2026-07-01T00:00:00Z')`,
    [ws, skillId, name],
  );
}

async function seedCurrent(ws: string, skillId: string, commitHex: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.skill_commit (workspace_id, commit_id, skill_id, bundle_digest)
     VALUES ($1, $2, $3, NULL)`,
    [ws, Buffer.from(commitHex, "hex"), skillId],
  );
  await scratchQuery(
    `INSERT INTO plane.current (workspace_id, skill_id, commit_id, epoch, seq, record, updated_at)
     VALUES ($1, $2, $3, 1, 1, NULL, $4)`,
    [ws, skillId, Buffer.from(commitHex, "hex"), Date.now()],
  );
}

async function seedDevice(
  ws: string,
  deviceKeyId: string,
  principal: string,
  lastReportAt: number | null,
  revoked = 0,
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.device_registry (workspace_id, device_key_id, public_key, principal, revoked, last_report_at)
     VALUES ($1, $2, $3, $4, $5, $6)`,
    [ws, deviceKeyId, PUBKEY, principal, revoked, lastReportAt],
  );
}

async function seedState(
  ws: string,
  deviceKeyId: string,
  skillId: string,
  appliedCommitHex: string | null,
  reportedAt: number,
  detached = 0,
  detachedAt: number | null = null,
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.device_skill_state
       (workspace_id, device_key_id, skill_id, applied_commit, reported_at, detached, detached_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7)`,
    [
      ws,
      deviceKeyId,
      skillId,
      appliedCommitHex === null ? null : Buffer.from(appliedCommitHex, "hex"),
      reportedAt,
      detached,
      detachedAt,
    ],
  );
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  await adminQuery(`ALTER DATABASE ${SCRATCH} SET search_path TO plane, public`);
  installTestEnv({ DATABASE_URL: scratchUrl() });
  await applyPlaneDdl(scratchUrl());

  const now = Date.now();

  // WS: the 1-hour override, an owner + a member seat, a NON-seated "gone" principal.
  await seedWorkspace(WS, "fleet");
  await seedPolicy(WS, WINDOW_1H);
  await seedSeat(WS, OWNER_EMAIL, "owner");
  await seedSeat(WS, MEMBER_EMAIL, "member");

  await seedCatalog(WS, "s_a", "alpha");
  await seedCurrent(WS, "s_a", CUR_A);
  await seedCatalog(WS, "s_b", "beta");
  await seedCurrent(WS, "s_b", CUR_B);
  await seedCatalog(WS, "s_c", "gamma");
  await seedCurrent(WS, "s_c", CUR_C);

  // The member's three devices: fresh (current + behind + detached), stale, and never-reported.
  await seedDevice(WS, "dm-fresh", MEMBER_EMAIL, now - 60_000);
  await seedState(WS, "dm-fresh", "s_a", CUR_A, now - 60_000); // current
  await seedState(WS, "dm-fresh", "s_b", OLD_B, now - 60_000); // behind
  await seedState(WS, "dm-fresh", "s_c", OLD_A, now - 90_000, 1, now - 90_000); // detached

  await seedDevice(WS, "dm-stale", MEMBER_EMAIL, now - 7_200_000); // 2h ago → stale under 1h
  await seedState(WS, "dm-stale", "s_a", CUR_A, now - 7_200_000);

  await seedDevice(WS, "dm-never", MEMBER_EMAIL, null); // never reported
  await seedState(WS, "dm-never", "s_a", CUR_A, now - 10_000);

  // The owner's device, and a removed member's orphaned device (no seat → removedUpstream).
  await seedDevice(WS, "do-one", OWNER_EMAIL, now - 60_000);
  await seedState(WS, "do-one", "s_a", CUR_A, now - 60_000);

  await seedDevice(WS, "dg-gone", GONE_EMAIL, now - 1_800_000);
  await seedState(WS, "dg-gone", "s_a", OLD_A, now - 1_800_000); // behind, still out there

  // WS_DEFAULT: no policy row (the 7-day default), one member device reported two days ago.
  await seedWorkspace(WS_DEFAULT, "fleet-default");
  await seedSeat(WS_DEFAULT, MEMBER_EMAIL, "member");
  await seedCatalog(WS_DEFAULT, "s_a", "alpha");
  await seedCurrent(WS_DEFAULT, "s_a", CUR_A);
  await seedDevice(WS_DEFAULT, "dd-one", MEMBER_EMAIL, now - 172_800_000); // 2 days ago
  await seedState(WS_DEFAULT, "dd-one", "s_a", CUR_A, now - 172_800_000);
});

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE ${SCRATCH} WITH (FORCE)`);
});

describe("fleetOf — role scoping", () => {
  it("a plain member sees ONLY their own devices", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(member(WS));
    expect(fleet.wholeFleet).toBe(false);
    expect(fleet.devices.map((d) => d.deviceKeyId).sort()).toEqual([
      "dm-fresh",
      "dm-never",
      "dm-stale",
    ]);
    // Never another principal's device, never a removed member's.
    expect(fleet.devices.every((d) => d.principal === MEMBER_EMAIL)).toBe(true);
  });

  it("a reviewer sees the whole fleet", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(reviewer(WS));
    expect(fleet.wholeFleet).toBe(true);
    expect(fleet.devices.map((d) => d.deviceKeyId).sort()).toEqual([
      "dg-gone",
      "dm-fresh",
      "dm-never",
      "dm-stale",
      "do-one",
    ]);
  });

  it("an owner sees the whole fleet, and gets a revoke affordance on every device", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(owner(WS));
    expect(fleet.wholeFleet).toBe(true);
    expect(fleet.devices).toHaveLength(5);
    expect(fleet.devices.every((d) => d.canRevoke)).toBe(true);
  });

  it("a member's canRevoke is self-only (owner-or-self mirrored)", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(member(WS));
    // The member sees only their own devices, so every one is self-revocable.
    expect(fleet.devices.every((d) => d.canRevoke)).toBe(true);
    // A reviewer (whole fleet) may revoke only their own — NOT the owner's or the gone device's.
    const asReviewer = await fleetOf(reviewer(WS));
    const byId = new Map(asReviewer.devices.map((d) => [d.deviceKeyId, d]));
    expect(byId.get("dm-fresh")?.canRevoke).toBe(true); // own
    expect(byId.get("do-one")?.canRevoke).toBe(false); // owner's
    expect(byId.get("dg-gone")?.canRevoke).toBe(false); // a stranger's
  });
});

describe("fleetOf — status derivation", () => {
  it("applied == current is `current`, applied != current is `behind`, detached is `detached`", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(member(WS));
    const fresh = fleet.devices.find((d) => d.deviceKeyId === "dm-fresh");
    expect(fresh).toBeDefined();
    const byName = new Map(fresh?.skills.map((s) => [s.skillName, s]));
    expect(byName.get("alpha")?.status).toBe("current");
    expect(byName.get("alpha")?.appliedCommit).toBe(CUR_A);
    expect(byName.get("alpha")?.currentCommit).toBe(CUR_A);
    expect(byName.get("beta")?.status).toBe("behind");
    expect(byName.get("beta")?.appliedCommit).toBe(OLD_B);
    expect(byName.get("beta")?.currentCommit).toBe(CUR_B);

    const detached = byName.get("gamma");
    expect(detached?.status).toBe("detached");
    expect(detached?.detachedAt).not.toBeNull();
    expect(detached?.appliedCommit).toBe(OLD_A); // its LAST applied commit, frozen
  });

  it("skills come back in catalog-name order", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(member(WS));
    const fresh = fleet.devices.find((d) => d.deviceKeyId === "dm-fresh");
    expect(fresh?.skills.map((s) => s.skillName)).toEqual(["alpha", "beta", "gamma"]);
  });
});

describe("fleetOf — freshness against the staleness window", () => {
  it("classifies fresh / stale / never against the 1-hour override", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(owner(WS));
    expect(fleet.stalenessWindowMs).toBe(WINDOW_1H);
    const byId = new Map(fleet.devices.map((d) => [d.deviceKeyId, d]));
    expect(byId.get("dm-fresh")?.freshness).toBe("fresh"); // 1 min ago
    expect(byId.get("dm-stale")?.freshness).toBe("stale"); // 2 h ago, past 1 h
    expect(byId.get("dm-never")?.freshness).toBe("never"); // NULL last_report_at
    expect(byId.get("dm-never")?.lastReportAt).toBeNull();
  });

  it("honors the 7-day DEFAULT when no policy row exists", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(member(WS_DEFAULT));
    expect(fleet.stalenessWindowMs).toBe(DEFAULT_WINDOW);
    // Two days ago is FRESH under the 7-day default (it would be stale under the 1-hour override).
    expect(fleet.devices[0]?.freshness).toBe("fresh");
  });
});

describe("fleetOf — removedUpstream marking", () => {
  it("marks a device whose principal holds no confirmed seat, and no one else", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(owner(WS));
    const byId = new Map(fleet.devices.map((d) => [d.deviceKeyId, d]));
    expect(byId.get("dg-gone")?.removedUpstream).toBe(true);
    expect(byId.get("dm-fresh")?.removedUpstream).toBe(false);
    expect(byId.get("do-one")?.removedUpstream).toBe(false);
  });
});

// Revoke MUTATES device_registry.revoked — kept LAST so the read assertions above see pristine rows.
describe("revokeDevice — the owner-or-self matrix", () => {
  it("a member may revoke their OWN device (self)", async () => {
    const { revokeDevice } = await q();
    expect(await revokeDevice(member(WS), "dm-never")).toBe("revoked");
  });

  it("a member may NOT revoke another member's device", async () => {
    const { revokeDevice } = await q();
    expect(await revokeDevice(member(WS), "do-one")).toBe("owner_or_self_required");
  });

  it("an owner may revoke any device", async () => {
    const { revokeDevice } = await q();
    expect(await revokeDevice(owner(WS), "dm-fresh")).toBe("revoked");
  });

  it("an unknown device is `unknown_device`", async () => {
    const { revokeDevice } = await q();
    expect(await revokeDevice(owner(WS), "no-such-device")).toBe("unknown_device");
  });

  it("a revoked device reads back as revoked on the fleet", async () => {
    const { fleetOf } = await q();
    const fleet = await fleetOf(owner(WS));
    const byId = new Map(fleet.devices.map((d) => [d.deviceKeyId, d]));
    expect(byId.get("dm-never")?.revoked).toBe(true);
    expect(byId.get("dm-fresh")?.revoked).toBe(true);
    expect(byId.get("do-one")?.revoked).toBe(false);
  });
});
