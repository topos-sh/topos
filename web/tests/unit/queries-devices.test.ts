import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { UserActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The ACCOUNT-level device DAL against a REAL scratch Postgres (created in beforeAll, dropped in
 * afterAll) on the session cluster. Schema `plane` is stood up from the in-repo authority
 * migrations (plane-ddl.ts) — so `topos_revoke_device` (migration 0018) is the REAL guarded
 * function, and its owner-or-self matrix decides every sign-out here. The web tier's own
 * `admin_event` table (mirroring schema.app.ts) is created in `public` for the audit-write test.
 *
 * Actors are minted here by CAST — the one thing production code must never do (the brand is
 * module-private to guards.server.ts). devicesFor + signOutDevice are scoped by a bare UserActor:
 * the safety is that both only ever touch the person's OWN rows, keyed on their verified email.
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:postgres@localhost:5439/topos_test";
const SCRATCH = `web_devices_test_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

const user = (email: string): UserActor => ({ email: email.trim().toLowerCase() }) as UserActor;

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

async function seedWorkspace(ws: string, displayName: string, name: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
     VALUES ($1, $2, 'unverified', 'cloud', '2026-07-01T00:00:00Z', $3)`,
    [ws, displayName, name],
  );
}

/** Seed one roster seat. Principals must be canonical lowercase — the 0010 CHECK is live. */
async function seedSeat(
  ws: string,
  principal: string,
  role: "owner" | "reviewer" | "member",
  status: "invited" | "confirmed",
  addedAt = "2026-07-01T00:00:01Z",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, invited_by, added_at)
     VALUES ($1, $2, $3, $4, NULL, $5)`,
    [ws, principal, role, status, addedAt],
  );
}

/** Seed one device_registry row (public_key is a NOT-NULL 32-byte BYTEA; revoked defaults 0). */
async function seedDevice(
  ws: string,
  deviceKeyId: string,
  principal: string,
  opts: { revoked?: 0 | 1; lastReportAtMs?: number | null } = {},
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.device_registry (workspace_id, device_key_id, public_key, principal, revoked, last_report_at)
     VALUES ($1, $2, $3, $4, $5, $6)`,
    [ws, deviceKeyId, Buffer.alloc(32), principal, opts.revoked ?? 0, opts.lastReportAtMs ?? null],
  );
}

async function revokedFlag(ws: string, deviceKeyId: string): Promise<number | undefined> {
  const rows = await scratchQuery<{ revoked: string }>(
    `SELECT revoked FROM plane.device_registry WHERE workspace_id = $1 AND device_key_id = $2`,
    [ws, deviceKeyId],
  );
  return rows[0] === undefined ? undefined : Number(rows[0].revoked);
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  await adminQuery(`ALTER DATABASE ${SCRATCH} SET search_path TO plane, public`);
  installTestEnv({ DATABASE_URL: scratchUrl() });
  await applyPlaneDdl(scratchUrl());
  const web = new Client({ connectionString: scratchUrl() });
  await web.connect();
  try {
    await web.query(`
      CREATE TABLE public.admin_event (
        id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
        workspace_id text NOT NULL,
        kind text NOT NULL,
        subject text NOT NULL,
        detail text,
        set_by text NOT NULL,
        set_at timestamptz NOT NULL DEFAULT now(),
        outcome text NOT NULL,
        CONSTRAINT admin_event_outcome_check CHECK (outcome IN ('ok', 'denied', 'error'))
      );
    `);
  } finally {
    await web.end();
  }
});

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE ${SCRATCH} WITH (FORCE)`);
});

async function q() {
  return import("@/lib/db/queries.devices.server");
}

describe("devicesFor (the account device list)", () => {
  it("returns ONLY the person's own devices, across every confirmed-seat workspace, grouped", async () => {
    const queries = await q();
    const me = "maya@devices.example.com";

    // Two workspaces where I hold a CONFIRMED seat, each with my own devices.
    await seedWorkspace("w_dev_a", "Alpha WS", "alpha-ws");
    await seedSeat("w_dev_a", me, "owner", "confirmed");
    await seedDevice("w_dev_a", "dk_a1", me, { lastReportAtMs: 1_700_000_000_000 });
    await seedDevice("w_dev_a", "dk_a2", me, { revoked: 1 });
    // Another person's device in the SAME workspace never appears.
    await seedSeat("w_dev_a", "other@devices.example.com", "member", "confirmed");
    await seedDevice("w_dev_a", "dk_other", "other@devices.example.com");

    await seedWorkspace("w_dev_b", "Beta WS", "beta-ws");
    await seedSeat("w_dev_b", me, "member", "confirmed");
    await seedDevice("w_dev_b", "dk_b1", me);

    // A workspace where my seat is INVITED-only — my device there must NOT appear (no admission).
    await seedWorkspace("w_dev_inv", "Invited WS", "invited-ws");
    await seedSeat("w_dev_inv", me, "member", "invited");
    await seedDevice("w_dev_inv", "dk_inv", me);

    // A workspace where I hold NO seat at all — a stray device row there is not mine to surface.
    await seedWorkspace("w_dev_absent", "Absent WS", "absent-ws");
    await seedDevice("w_dev_absent", "dk_absent", me);

    const groups = await queries.devicesFor(user("Maya@Devices.Example.com"));
    expect(groups).toEqual([
      {
        workspaceId: "w_dev_a",
        displayName: "Alpha WS",
        address: "alpha-ws",
        devices: [
          { deviceKeyId: "dk_a1", revoked: false, lastReportAtMs: 1_700_000_000_000 },
          { deviceKeyId: "dk_a2", revoked: true, lastReportAtMs: null },
        ],
      },
      {
        workspaceId: "w_dev_b",
        displayName: "Beta WS",
        address: "beta-ws",
        devices: [{ deviceKeyId: "dk_b1", revoked: false, lastReportAtMs: null }],
      },
    ]);
  });

  it("falls back to the workspace id for display name AND address when the workspace row is missing", async () => {
    const queries = await q();
    const me = "ghost@devices.example.com";
    // A seat + device can outlive the workspace row.
    await seedSeat("w_dev_ghost", me, "member", "confirmed");
    await seedDevice("w_dev_ghost", "dk_ghost", me);
    const groups = await queries.devicesFor(user(me));
    expect(groups).toEqual([
      {
        workspaceId: "w_dev_ghost",
        displayName: "w_dev_ghost",
        address: "w_dev_ghost",
        devices: [{ deviceKeyId: "dk_ghost", revoked: false, lastReportAtMs: null }],
      },
    ]);
  });

  it("returns [] for a person with a confirmed seat but no devices", async () => {
    const queries = await q();
    await seedWorkspace("w_dev_empty", "Empty WS", "empty-ws");
    await seedSeat("w_dev_empty", "lonely@devices.example.com", "owner", "confirmed");
    expect(await queries.devicesFor(user("lonely@devices.example.com"))).toEqual([]);
  });
});

describe("signOutDevice (the guarded topos_revoke_device self sign-out)", () => {
  it("a self sign-out succeeds and flips revoked; re-signing-out is idempotent", async () => {
    const queries = await q();
    const me = "self@revoke.example.com";
    await seedWorkspace("w_rev_self", "Self WS", "self-ws");
    await seedSeat("w_rev_self", me, "member", "confirmed");
    await seedDevice("w_rev_self", "dk_self", me);

    expect(await queries.signOutDevice(user(me), "w_rev_self", "dk_self")).toBe("revoked");
    expect(await revokedFlag("w_rev_self", "dk_self")).toBe(1);
    // Idempotent: re-revoking a revoked device still answers 'revoked'.
    expect(await queries.signOutDevice(user(me), "w_rev_self", "dk_self")).toBe("revoked");
  });

  it("a plain member cannot sign out ANOTHER person's device — owner_or_self_required", async () => {
    const queries = await q();
    const me = "plain@revoke.example.com";
    const other = "colleague@revoke.example.com";
    await seedWorkspace("w_rev_foreign", "Foreign WS", "foreign-ws");
    await seedSeat("w_rev_foreign", me, "member", "confirmed");
    await seedSeat("w_rev_foreign", other, "member", "confirmed");
    await seedDevice("w_rev_foreign", "dk_theirs", other);

    expect(await queries.signOutDevice(user(me), "w_rev_foreign", "dk_theirs")).toBe(
      "owner_or_self_required",
    );
    // The device is untouched.
    expect(await revokedFlag("w_rev_foreign", "dk_theirs")).toBe(0);
  });

  it("an unknown device is 'unknown_device'; a non-member workspace is 'member_required'", async () => {
    const queries = await q();
    const me = "edge@revoke.example.com";
    await seedWorkspace("w_rev_edge", "Edge WS", "edge-ws");
    await seedSeat("w_rev_edge", me, "member", "confirmed");
    expect(await queries.signOutDevice(user(me), "w_rev_edge", "dk_nope")).toBe("unknown_device");
    // No seat in this workspace: the gate stops at membership before it ever looks at the device.
    expect(await queries.signOutDevice(user(me), "w_rev_stranger", "dk_whatever")).toBe(
      "member_required",
    );
  });
});

describe("recordSelfDeviceRevoke (the one audit write outside audit.server.ts)", () => {
  it("lands one admin_event row (kind device_revoke, detail 'self') keyed on the target workspace", async () => {
    const queries = await q();
    await queries.recordSelfDeviceRevoke(
      user("Auditor@Revoke.Example.com"),
      "w_audit",
      "dk_audited",
      "ok",
    );
    const rows = await scratchQuery<{
      workspace_id: string;
      kind: string;
      subject: string;
      detail: string;
      set_by: string;
      outcome: string;
    }>(`SELECT workspace_id, kind, subject, detail, set_by, outcome FROM public.admin_event`);
    expect(rows).toEqual([
      {
        workspace_id: "w_audit",
        kind: "device_revoke",
        subject: "dk_audited",
        detail: "self",
        set_by: "auditor@revoke.example.com",
        outcome: "ok",
      },
    ]);
  });
});
