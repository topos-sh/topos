import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The WORKSPACE-POLICY DAL (queries.policy.server.ts) against a REAL scratch Postgres carrying the
 * in-repo authority migrations — so the guarded reader/setter functions (`topos_invite_policy`,
 * `topos_staleness_window`, `topos_set_invite_policy`, `topos_set_staleness_window`) are the REAL
 * ones, defaults and owner gates included. The DAL touches ONLY schema `plane` (no web tables), so
 * the scratch DB needs the plane DDL alone.
 *
 * Actors are minted by CAST (the brand is module-private to guards.server.ts): the DAL relays the
 * actor's email + workspaceId to the SQL functions, whose OWN owner gate is what the tests exercise
 * — so a member-email actor cast as an OwnerActor is refused by the DATABASE, exactly as a real
 * owner-guard bypass would be.
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:postgres@localhost:5439/topos_test";
const SCRATCH = `web_policy_test_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

const DEFAULT_WINDOW_MS = 604_800_000; // 7 days — the SQL default.
const MAX_WINDOW_MS = 31_622_400_000; // 366 days — the SQL ceiling (inclusive).

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

async function scratchQuery<Row extends Record<string, unknown> = Record<string, unknown>>(
  sql: string,
  params: unknown[] = [],
): Promise<Row[]> {
  const { getPool } = await import("@/lib/db/index.server");
  const result = await getPool().query(sql, params);
  return result.rows as Row[];
}

/** Seed one plane workspace row (columns the OSS DDL requires; TEXT ISO-8601 created_at). */
async function seedWorkspace(ws: string, name: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
     VALUES ($1, $2, 'unverified', 'cloud', '2026-07-01T00:00:00Z', $3)`,
    [ws, name, name],
  );
}

/** Seed one CONFIRMED roster seat (principals must be canonical lowercase — the 0010 CHECK is live). */
async function seedSeat(
  ws: string,
  principal: string,
  role: "owner" | "reviewer" | "member",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, added_at)
     VALUES ($1, $2, $3, 'confirmed', '2026-07-01T00:00:01Z')`,
    [ws, principal, role],
  );
}

/** Read the raw policy row (the setters' effect, unmediated by the DAL). */
async function policyRow(
  ws: string,
): Promise<{ invite_policy: string; staleness_window_ms: string } | undefined> {
  const rows = await scratchQuery<{ invite_policy: string; staleness_window_ms: string }>(
    `SELECT invite_policy, staleness_window_ms FROM plane.workspace_policy WHERE workspace_id = $1`,
    [ws],
  );
  return rows[0];
}

async function q() {
  return import("@/lib/db/queries.policy.server");
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  // The DAL calls the guarded functions UNqualified; search_path resolves them in `plane`.
  await adminQuery(`ALTER DATABASE ${SCRATCH} SET search_path TO plane, public`);
  installTestEnv({ DATABASE_URL: scratchUrl() });
  await applyPlaneDdl(scratchUrl());
});

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE ${SCRATCH} WITH (FORCE)`);
});

describe("invitePolicyOf / stalenessWindowOf (the guarded default readers)", () => {
  it("returns the SQL defaults when NO workspace_policy row exists: members / 604800000", async () => {
    const queries = await q();
    await seedWorkspace("w_pol_def", "pol-def");
    await seedSeat("w_pol_def", "member@example.com", "member");
    // No workspace_policy row seeded — the readers COALESCE to the ONE default in SQL.
    expect(await policyRow("w_pol_def")).toBeUndefined();
    expect(await queries.invitePolicyOf(member("w_pol_def"))).toBe("members");
    expect(await queries.stalenessWindowOf(member("w_pol_def"))).toBe(DEFAULT_WINDOW_MS);
  });
});

describe("setInvitePolicy", () => {
  it("an owner sets the policy; the reader reflects it and the row lands", async () => {
    const queries = await q();
    await seedWorkspace("w_inv_ok", "inv-ok");
    await seedSeat("w_inv_ok", "boss@example.com", "owner");
    expect(await queries.setInvitePolicy(owner("w_inv_ok", "boss@example.com"), "owners")).toBe(
      "set",
    );
    expect(await queries.invitePolicyOf(member("w_inv_ok"))).toBe("owners");
    expect((await policyRow("w_inv_ok"))?.invite_policy).toBe("owners");
    // Flipping back to members works the same way.
    expect(await queries.setInvitePolicy(owner("w_inv_ok", "boss@example.com"), "members")).toBe(
      "set",
    );
    expect(await queries.invitePolicyOf(member("w_inv_ok"))).toBe("members");
  });

  it("a confirmed non-owner is owner_role_required; a non-member is member_required — no change", async () => {
    const queries = await q();
    await seedWorkspace("w_inv_gate", "inv-gate");
    await seedSeat("w_inv_gate", "plain@example.com", "member");
    // A member-email actor cast as OwnerActor — the DATABASE's own owner gate refuses it.
    expect(await queries.setInvitePolicy(owner("w_inv_gate", "plain@example.com"), "owners")).toBe(
      "owner_role_required",
    );
    // An email with no confirmed seat at all.
    expect(
      await queries.setInvitePolicy(owner("w_inv_gate", "stranger@example.com"), "owners"),
    ).toBe("member_required");
    // Neither attempt wrote a policy row.
    expect(await policyRow("w_inv_gate")).toBeUndefined();
  });

  it("an unexpected policy value is bad_policy — no change", async () => {
    const queries = await q();
    await seedWorkspace("w_inv_bad", "inv-bad");
    await seedSeat("w_inv_bad", "boss@example.com", "owner");
    expect(
      await queries.setInvitePolicy(
        owner("w_inv_bad", "boss@example.com"),
        "everyone" as "members",
      ),
    ).toBe("bad_policy");
    expect(await policyRow("w_inv_bad")).toBeUndefined();
  });
});

describe("setStalenessWindow", () => {
  it("an owner sets the window; the reader reflects the millisecond value exactly", async () => {
    const queries = await q();
    await seedWorkspace("w_win_ok", "win-ok");
    await seedSeat("w_win_ok", "boss@example.com", "owner");
    // 14 days in ms — a value above 2^31 that must round-trip as an exact Number.
    const fourteenDays = 14 * 86_400_000;
    expect(
      await queries.setStalenessWindow(owner("w_win_ok", "boss@example.com"), fourteenDays),
    ).toBe("set");
    expect(await queries.stalenessWindowOf(member("w_win_ok"))).toBe(fourteenDays);
    // The 366-day ceiling is inclusive.
    expect(
      await queries.setStalenessWindow(owner("w_win_ok", "boss@example.com"), MAX_WINDOW_MS),
    ).toBe("set");
    expect(await queries.stalenessWindowOf(member("w_win_ok"))).toBe(MAX_WINDOW_MS);
  });

  it("bad_window: zero, negative, and past 366 days are refused — no change", async () => {
    const queries = await q();
    await seedWorkspace("w_win_bad", "win-bad");
    await seedSeat("w_win_bad", "boss@example.com", "owner");
    // A known-good value first, so a later refusal proves it did NOT overwrite.
    expect(
      await queries.setStalenessWindow(owner("w_win_bad", "boss@example.com"), 3_600_000),
    ).toBe("set");
    for (const bad of [0, -1, MAX_WINDOW_MS + 1]) {
      expect(await queries.setStalenessWindow(owner("w_win_bad", "boss@example.com"), bad)).toBe(
        "bad_window",
      );
    }
    // Still the last good value.
    expect(await queries.stalenessWindowOf(member("w_win_bad"))).toBe(3_600_000);
  });

  it("a confirmed non-owner is owner_role_required; a non-member is member_required", async () => {
    const queries = await q();
    await seedWorkspace("w_win_gate", "win-gate");
    await seedSeat("w_win_gate", "plain@example.com", "member");
    expect(
      await queries.setStalenessWindow(owner("w_win_gate", "plain@example.com"), 86_400_000),
    ).toBe("owner_role_required");
    expect(
      await queries.setStalenessWindow(owner("w_win_gate", "stranger@example.com"), 86_400_000),
    ).toBe("member_required");
    expect(await policyRow("w_win_gate")).toBeUndefined();
  });
});
