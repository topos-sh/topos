import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";
import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The step-up mail rung's server half, against a REAL scratch Postgres (the Drizzle migration
 * applied verbatim): the single-use confirmation token mint + atomic consume stored in Better
 * Auth's `verification` table, AND the registration gate every sign-up rung (magic link and
 * social included) rides — proving an uninvited address is refused whatever the rung.
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:ss@localhost:5451/postgres";
const SCRATCH = `stepup_token_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

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

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  installTestEnv({ DATABASE_URL: scratchUrl(), TOPOS_SETUP_CODE: "stepup-token-setup-code" });
  await applyPlaneDdl(scratchUrl());
  const WEB_ROOT = resolve(__dirname, "..", "..");
  execFileSync("node", [join(WEB_ROOT, "scripts", "migrate.mjs")], {
    env: { ...process.env, DATABASE_URL: scratchUrl() },
    stdio: "pipe",
  });
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup("http://localhost:3000");
}, 60000);

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE IF EXISTS ${SCRATCH} WITH (FORCE)`);
});

describe("the step-up confirmation token", () => {
  it("mints a token, stores ONLY its hash, and consumes it exactly once", async () => {
    const identity = await import("@/lib/db/identity.server");
    const token = await identity.mintStepUpConfirmation("u_once", "/members");
    expect(token.length).toBeGreaterThan(0);
    // The plaintext never lands in the row — only the hex digest under the namespaced identifier.
    const stored = await q<{ value: string }>(
      `SELECT value FROM web.verification WHERE identifier = 'step-up:u_once:/members'`,
    );
    expect(stored).toHaveLength(1);
    expect(stored[0]?.value).not.toContain(token);
    expect(stored[0]?.value).toMatch(/^[0-9a-f]{64}$/);
    // First consume wins; the row is gone, so a replay misses.
    expect(await identity.consumeStepUpConfirmation("u_once", "/members", token)).toBe(true);
    expect(await identity.consumeStepUpConfirmation("u_once", "/members", token)).toBe(false);
  });

  it("misses a wrong token, a foreign user, and a foreign CEREMONY; the real token still works in place", async () => {
    const identity = await import("@/lib/db/identity.server");
    const token = await identity.mintStepUpConfirmation("u_owner", "/members");
    expect(await identity.consumeStepUpConfirmation("u_owner", "/members", "not-the-token")).toBe(
      false,
    );
    expect(await identity.consumeStepUpConfirmation("u_intruder", "/members", token)).toBe(false);
    // A token minted on ONE ceremony page proves nothing on another (per-ceremony binding).
    expect(await identity.consumeStepUpConfirmation("u_owner", "/settings", token)).toBe(false);
    expect(await identity.consumeStepUpConfirmation("u_owner", "/members", token)).toBe(true);
  });

  it("misses an expired token", async () => {
    const identity = await import("@/lib/db/identity.server");
    const token = await identity.mintStepUpConfirmation("u_expired", "/members");
    await q(
      `UPDATE web.verification SET expires_at = now() - interval '1 minute' WHERE identifier = 'step-up:u_expired:/members'`,
    );
    expect(await identity.consumeStepUpConfirmation("u_expired", "/members", token)).toBe(false);
  });

  it("a fresh mint supersedes the prior token for the same user + ceremony", async () => {
    const identity = await import("@/lib/db/identity.server");
    const first = await identity.mintStepUpConfirmation("u_super", "/members");
    const second = await identity.mintStepUpConfirmation("u_super", "/members");
    expect(
      await q(`SELECT 1 FROM web.verification WHERE identifier = 'step-up:u_super:/members'`),
    ).toHaveLength(1);
    expect(await identity.consumeStepUpConfirmation("u_super", "/members", first)).toBe(false);
    expect(await identity.consumeStepUpConfirmation("u_super", "/members", second)).toBe(true);
  });

  it("an empty token never consumes anything", async () => {
    const identity = await import("@/lib/db/identity.server");
    await identity.mintStepUpConfirmation("u_empty", "/members");
    expect(await identity.consumeStepUpConfirmation("u_empty", "/members", "")).toBe(false);
  });
});

describe("the registration gate under every rung", () => {
  it("refuses an uninvited address — the gate the create hook runs for magic-link and social too", async () => {
    const { assertRegistrationAllowed, REGISTRATION_REFUSED } = await import(
      "@/lib/auth/registration.server"
    );
    // A fresh install is invite-only with no invitation and no armed mail: every rung is refused
    // by the SAME create.before hook, so magic-link/social cannot slip an uninvited sign-up in.
    await expect(assertRegistrationAllowed("nobody@example.com")).rejects.toThrow(
      REGISTRATION_REFUSED,
    );
  });
});
