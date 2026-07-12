import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The ROSTER write DAL (`queries.roster.server.ts`) against a REAL scratch Postgres carrying the
 * in-repo authority DDL — so `topos_set_member_role` and `topos_leave_workspace` (migration 0018)
 * are the REAL guarded functions, the last-owner lockout and the leave lapse-detach reconcile
 * included. Same scaffold as `queries.test.ts`: a scratch database created off the superuser session
 * URL (owned by that superuser, so no per-table grants are needed to seed the authority tables AND
 * to call the functions through the DAL), search_path `plane, public` so the DAL's unqualified
 * function calls resolve.
 *
 * Actors are minted by CAST — the one thing production must never do (the brand is module-private to
 * guards.server.ts); the DB re-runs every role gate itself, so a wrong-role cast is refused by the
 * function, not trusted.
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:postgres@localhost:5439/topos_test";
const SCRATCH = `web_roster_test_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

const member = (ws: string, email = "member@example.com"): MemberActor =>
  ({ email: email.trim().toLowerCase(), workspaceId: ws, role: "member" }) as MemberActor;
const owner = (ws: string, email = "owner@example.com"): OwnerActor =>
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

/** Raw SQL against the scratch DB via the app pool (same superuser; plane.* fully qualified). */
async function scratchQuery<Row extends Record<string, unknown> = Record<string, unknown>>(
  sql: string,
  params: unknown[] = [],
): Promise<Row[]> {
  const { getPool } = await import("@/lib/db/index.server");
  const result = await getPool().query(sql, params);
  return result.rows as Row[];
}

async function seedWorkspace(ws: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
     VALUES ($1, $1, 'unverified', 'cloud', '2026-07-01T00:00:00Z', $2)`,
    [ws, ws.replace(/_/g, "-")],
  );
}

/** Seed one roster seat. Principals must be canonical lowercase — the 0010 CHECK is live. */
async function seedSeat(
  ws: string,
  principal: string,
  role: "owner" | "reviewer" | "member",
  status: "invited" | "confirmed" = "confirmed",
  addedAt = "2026-07-01T00:00:01Z",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, invited_by, added_at)
     VALUES ($1, $2, $3, $4, NULL, $5)`,
    [ws, principal, role, status, addedAt],
  );
}

async function seedCatalog(ws: string, skillId: string, name: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.catalog (workspace_id, skill_id, name, display_name, status, created_at)
     VALUES ($1, $2, $3, NULL, 'active', '2026-07-01T00:00:00Z')`,
    [ws, skillId, name],
  );
}

/** Create the structural `everyone` channel and place a skill in it — one entitlement source. */
async function seedEveryonePlacement(ws: string, skillId: string): Promise<void> {
  await scratchQuery(`SELECT topos_ensure_everyone($1, '2026-07-01T00:00:00Z')`, [ws]);
  await scratchQuery(
    `INSERT INTO plane.channel_skills (workspace_id, channel_id, skill_id, added_by, added_at)
     VALUES ($1, 'everyone', $2, 'seed', '2026-07-01T00:00:00Z')`,
    [ws, skillId],
  );
}

async function seedDevice(ws: string, deviceKeyId: string, principal: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.device_registry (workspace_id, device_key_id, public_key, principal, revoked)
     VALUES ($1, $2, $3, $4, 0)`,
    [ws, deviceKeyId, Buffer.alloc(32), principal],
  );
}

async function seedDeviceSkillState(
  ws: string,
  deviceKeyId: string,
  skillId: string,
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.device_skill_state (workspace_id, device_key_id, skill_id, applied_commit, reported_at, detached)
     VALUES ($1, $2, $3, NULL, $4, 0)`,
    [ws, deviceKeyId, skillId, Date.parse("2026-07-01T12:00:00Z")],
  );
}

async function seatOf(
  ws: string,
  principal: string,
): Promise<{ role: string; status: string } | undefined> {
  const rows = await scratchQuery<{ role: string; status: string }>(
    `SELECT role, status FROM plane.workspace_member WHERE workspace_id = $1 AND principal = $2`,
    [ws, principal],
  );
  return rows[0];
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  await adminQuery(`ALTER DATABASE ${SCRATCH} SET search_path TO plane, public`);
  installTestEnv({ DATABASE_URL: scratchUrl() });
  await applyPlaneDdl(scratchUrl());
});

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE ${SCRATCH} WITH (FORCE)`);
});

async function q() {
  return import("@/lib/db/queries.roster.server");
}

describe("setMemberRole (the guarded topos_set_member_role write)", () => {
  it("a confirmed owner sets a member's role — outcome 'set' and the row flips", async () => {
    const roster = await q();
    await seedWorkspace("w_role_ok");
    await seedSeat("w_role_ok", "boss@example.com", "owner");
    await seedSeat("w_role_ok", "hand@example.com", "member");

    const outcome = await roster.setMemberRole(
      owner("w_role_ok", "boss@example.com"),
      "hand@example.com",
      "reviewer",
    );
    expect(outcome).toBe("set");
    expect(await seatOf("w_role_ok", "hand@example.com")).toEqual({
      role: "reviewer",
      status: "confirmed",
    });
  });

  it("a plain member acting is 'owner_role_required'; a non-member is 'member_required' — no change", async () => {
    const roster = await q();
    await seedWorkspace("w_role_gate");
    await seedSeat("w_role_gate", "boss@example.com", "owner");
    await seedSeat("w_role_gate", "plain@example.com", "member");

    // The DB re-runs the owner gate on the ACTING principal, whatever the cast claims.
    expect(
      await roster.setMemberRole(
        owner("w_role_gate", "plain@example.com"),
        "boss@example.com",
        "member",
      ),
    ).toBe("owner_role_required");
    expect(
      await roster.setMemberRole(
        owner("w_role_gate", "stranger@example.com"),
        "boss@example.com",
        "member",
      ),
    ).toBe("member_required");
    expect(await seatOf("w_role_gate", "boss@example.com")).toEqual({
      role: "owner",
      status: "confirmed",
    });
  });

  it("demoting the SOLE confirmed owner is refused — 'sole_owner', the role unchanged", async () => {
    const roster = await q();
    await seedWorkspace("w_role_sole");
    await seedSeat("w_role_sole", "solo@example.com", "owner");
    await seedSeat("w_role_sole", "hand@example.com", "member");

    expect(
      await roster.setMemberRole(
        owner("w_role_sole", "solo@example.com"),
        "solo@example.com",
        "member",
      ),
    ).toBe("sole_owner");
    expect(await seatOf("w_role_sole", "solo@example.com")).toEqual({
      role: "owner",
      status: "confirmed",
    });

    // With a SECOND confirmed owner present, the same demotion lands (the workspace keeps an owner).
    await roster.setMemberRole(
      owner("w_role_sole", "solo@example.com"),
      "hand@example.com",
      "owner",
    );
    expect(
      await roster.setMemberRole(
        owner("w_role_sole", "solo@example.com"),
        "solo@example.com",
        "member",
      ),
    ).toBe("set");
    expect(await seatOf("w_role_sole", "solo@example.com")).toEqual({
      role: "member",
      status: "confirmed",
    });
  });

  it("an unknown target seat is 'unknown_member'", async () => {
    const roster = await q();
    await seedWorkspace("w_role_unk");
    await seedSeat("w_role_unk", "boss@example.com", "owner");
    expect(
      await roster.setMemberRole(
        owner("w_role_unk", "boss@example.com"),
        "ghost@example.com",
        "reviewer",
      ),
    ).toBe("unknown_member");
  });
});

describe("leaveWorkspace (the guarded topos_leave_workspace write)", () => {
  it("a member leaves: the seat is deleted AND the lapse-detach freezes their entitled copies", async () => {
    const roster = await q();
    const ws = "w_leave_detach";
    await seedWorkspace(ws);
    await seedSeat(ws, "boss@example.com", "owner");
    await seedSeat(ws, "leaver@example.com", "member");
    // The leaver is entitled to one skill through `everyone`, and holds a reported device copy.
    await seedCatalog(ws, "s_deploy", "deploy");
    await seedEveryonePlacement(ws, "s_deploy");
    await seedDevice(ws, "dk_leaver01", "leaver@example.com");
    await seedDeviceSkillState(ws, "dk_leaver01", "s_deploy");

    const outcome = await roster.leaveWorkspace(member(ws, "leaver@example.com"));
    expect(outcome).toBe("left");

    // The seat is gone from the directory roster.
    expect(await seatOf(ws, "leaver@example.com")).toBeUndefined();
    // The reconcile ran BEFORE the delete: a person-scoped detach record + a frozen fleet row.
    const detach = await scratchQuery<{ cause: string }>(
      `SELECT cause FROM plane.skill_detachments
       WHERE workspace_id = $1 AND principal = 'leaver@example.com' AND skill_id = 's_deploy'`,
      [ws],
    );
    expect(detach).toEqual([{ cause: "membership_removed" }]);
    const frozen = await scratchQuery<{ detached: string }>(
      `SELECT detached::text AS detached FROM plane.device_skill_state
       WHERE workspace_id = $1 AND device_key_id = 'dk_leaver01' AND skill_id = 's_deploy'`,
      [ws],
    );
    expect(frozen).toEqual([{ detached: "1" }]);
  });

  it("the sole confirmed owner cannot leave — 'sole_owner', the seat stands", async () => {
    const roster = await q();
    const ws = "w_leave_sole";
    await seedWorkspace(ws);
    await seedSeat(ws, "solo@example.com", "owner");

    // Cast the owner as a member actor — the DB reads the seat's real role, not the cast.
    expect(await roster.leaveWorkspace(member(ws, "solo@example.com"))).toBe("sole_owner");
    expect(await seatOf(ws, "solo@example.com")).toEqual({ role: "owner", status: "confirmed" });
  });

  it("a non-member (no seat) is 'member_required'", async () => {
    const roster = await q();
    const ws = "w_leave_none";
    await seedWorkspace(ws);
    expect(await roster.leaveWorkspace(member(ws, "nobody@example.com"))).toBe("member_required");
  });
});
