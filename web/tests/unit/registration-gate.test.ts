import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";
import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The registration gate's HOOK BODY (`assertRegistrationAllowed`) against a REAL scratch
 * Postgres, with the composition mocked so both policies × both tenancies are reachable:
 * the `open` composition's unconditional admit, the gated single-tenant reads (THE workspace's
 * knob + the invitation check scoped to its id), the gated multi-tenant rule (the per-workspace
 * knob is NEVER consulted; an invitation in ANY workspace + armed mail admits), and the
 * invited seat's own scoping (`bindInvitedSeats` binds only in the invitation's workspace).
 *
 * The file runs MAIL-ARMED (the five TOPOS_MAIL_SMTP_* set; nothing sends — the gate only
 * reads `canSend`); the unarmed refusal is covered by the pure decision table.
 */

let registrationPolicy: "gated" | "open" = "gated";
let tenancyMode: "single" | "multi" = "single";

vi.mock("@/composition.server", () => ({
  composition: {
    get registration() {
      return registrationPolicy;
    },
    get tenancy() {
      return tenancyMode;
    },
  },
}));

const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:rg@localhost:5453/postgres";
const SCRATCH = `registration_gate_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

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

async function q<Row extends Record<string, unknown> = Record<string, unknown>>(
  sql: string,
  params: unknown[] = [],
): Promise<Row[]> {
  const { getPool } = await import("@/lib/db/index.server");
  const result = await getPool().query(sql, params);
  return result.rows as Row[];
}

/** The boot-minted workspace (single tenancy's THE workspace) and, later, a second one. */
let wsA = "";
const WS_B = "w_gate_second";

async function invite(email: string, workspaceId: string): Promise<void> {
  await q(`INSERT INTO web.invitation (id, workspace_id, email, role) VALUES ($1, $2, $3, $4)`, [
    `inv_${email.split("@")[0]}`,
    workspaceId,
    email,
    "member",
  ]);
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  installTestEnv({
    DATABASE_URL: scratchUrl(),
    TOPOS_SETUP_CODE: "registration-gate-setup-code",
    // Arm mail so `canSend` is true — the invitation rung's other half. Nothing ever sends.
    TOPOS_MAIL_SMTP_HOST: "127.0.0.1",
    TOPOS_MAIL_SMTP_PORT: "2599",
    TOPOS_MAIL_SMTP_USER: "gate",
    TOPOS_MAIL_SMTP_PASS: "gate",
    TOPOS_MAIL_SMTP_FROM: "Gate <gate@example.test>",
  });
  await applyPlaneDdl(scratchUrl());
  const WEB_ROOT = resolve(__dirname, "..", "..");
  execFileSync("node", [join(WEB_ROOT, "scripts", "migrate.mjs")], {
    env: { ...process.env, DATABASE_URL: scratchUrl() },
    stdio: "pipe",
  });
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup("http://localhost:3000");
  wsA = (await identity.theWorkspace())?.id ?? "";
}, 60000);

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE IF EXISTS ${SCRATCH} WITH (FORCE)`);
});

describe("gated, single tenancy — the one boot workspace", () => {
  it("refuses an uninvited address even with mail armed", async () => {
    registrationPolicy = "gated";
    tenancyMode = "single";
    const { assertRegistrationAllowed, REGISTRATION_REFUSED } = await import(
      "@/lib/auth/registration.server"
    );
    await expect(assertRegistrationAllowed("uninvited@example.com")).rejects.toThrow(
      REGISTRATION_REFUSED,
    );
  });

  it("admits a pending invitation in THE workspace (mail armed)", async () => {
    registrationPolicy = "gated";
    tenancyMode = "single";
    const { assertRegistrationAllowed } = await import("@/lib/auth/registration.server");
    await invite("invited-here@example.com", wsA);
    await expect(assertRegistrationAllowed("invited-here@example.com")).resolves.toBeUndefined();
  });

  it("the workspace `open` knob admits an uninvited address; back to invite_only it refuses again", async () => {
    registrationPolicy = "gated";
    tenancyMode = "single";
    const { assertRegistrationAllowed, REGISTRATION_REFUSED } = await import(
      "@/lib/auth/registration.server"
    );
    await q(`UPDATE web.workspace SET registration = 'open' WHERE id = $1`, [wsA]);
    await expect(assertRegistrationAllowed("knob-open@example.com")).resolves.toBeUndefined();
    await q(`UPDATE web.workspace SET registration = 'invite_only' WHERE id = $1`, [wsA]);
    await expect(assertRegistrationAllowed("knob-open@example.com")).rejects.toThrow(
      REGISTRATION_REFUSED,
    );
  });
});

describe("the open composition", () => {
  it("admits every sign-up — uninvited, no knob, either tenancy", async () => {
    const { assertRegistrationAllowed } = await import("@/lib/auth/registration.server");
    registrationPolicy = "open";
    tenancyMode = "single";
    await expect(assertRegistrationAllowed("anyone-single@example.com")).resolves.toBeUndefined();
    tenancyMode = "multi";
    await expect(assertRegistrationAllowed("anyone-multi@example.com")).resolves.toBeUndefined();
    registrationPolicy = "gated";
    tenancyMode = "single";
  });
});

describe("two workspaces — the scoping fences and the multi crossing", () => {
  beforeAll(async () => {
    await q(
      `INSERT INTO web.workspace (id, name, display_name, claimed_at) VALUES ($1, 'gate-second', 'Gate Second', now())`,
      [WS_B],
    );
  });

  it("single tenancy: an invitation in a workspace that is NOT the one workspace admits nothing", async () => {
    registrationPolicy = "gated";
    tenancyMode = "single";
    const identity = await import("@/lib/db/identity.server");
    const { assertRegistrationAllowed, REGISTRATION_REFUSED } = await import(
      "@/lib/auth/registration.server"
    );
    // With two rows on disk, pick whichever one theWorkspace() currently resolves as THE
    // workspace and invite into the OTHER — the scoped check must not see it.
    const theOne = (await identity.theWorkspace())?.id ?? "";
    const foreign = theOne === wsA ? WS_B : wsA;
    await invite("invited-elsewhere@example.com", foreign);
    await expect(assertRegistrationAllowed("invited-elsewhere@example.com")).rejects.toThrow(
      REGISTRATION_REFUSED,
    );
  });

  it("multi tenancy: the per-workspace knob is never consulted — `open` rows admit nothing", async () => {
    registrationPolicy = "gated";
    tenancyMode = "multi";
    const { assertRegistrationAllowed, REGISTRATION_REFUSED } = await import(
      "@/lib/auth/registration.server"
    );
    await q(`UPDATE web.workspace SET registration = 'open'`);
    await expect(assertRegistrationAllowed("uninvited-multi@example.com")).rejects.toThrow(
      REGISTRATION_REFUSED,
    );
    await q(`UPDATE web.workspace SET registration = 'invite_only'`);
  });

  it("multi tenancy: a pending invitation in ANY workspace + armed mail admits", async () => {
    registrationPolicy = "gated";
    tenancyMode = "multi";
    const { assertRegistrationAllowed } = await import("@/lib/auth/registration.server");
    await invite("invited-any@example.com", WS_B);
    await expect(assertRegistrationAllowed("invited-any@example.com")).resolves.toBeUndefined();
  });

  it("bindInvitedSeats binds ONLY in the invitation's own workspace, never a sibling", async () => {
    const identity = await import("@/lib/db/identity.server");
    await q(`INSERT INTO web."user" (id, name, email) VALUES ($1, $2, $3)`, [
      "u_bind_scope",
      "Bind Scope",
      "bind-scope@example.com",
    ]);
    await invite("bind-scope@example.com", wsA);
    const bound = await identity.bindInvitedSeats(
      "u_bind_scope",
      "bind-scope@example.com",
      "Bind Scope",
    );
    expect(bound).toBe(1);
    const seats = await q<{ workspace_id: string }>(
      `SELECT workspace_id FROM web.seat WHERE user_id = 'u_bind_scope'`,
    );
    expect(seats).toEqual([{ workspace_id: wsA }]);
  });
});
