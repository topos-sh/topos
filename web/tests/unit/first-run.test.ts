import { execFileSync } from "node:child_process";
import { join, resolve } from "node:path";
import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The FIRST-BOOT surface against a REAL scratch Postgres: `theWorkspace()` (the single-tenant
 * read every page resolves through) is null on a virgin database and a row the moment
 * `ensureSetup` runs; the boot-minted workspace stands UNCLAIMED (claimed_at null) with a live
 * claim-code hash; `claimableWorkspace` answers ONLY the right code (the uniform miss
 * otherwise); and a preset `TOPOS_SETUP_CODE` is exactly the code the stored hash matches — the
 * CI/IaC stability contract. The claim-consume race and the seats it mints are identity-core's
 * ground; this suite stays small on purpose.
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:identity2@localhost:5443/postgres";
const SCRATCH = `first_run_${Date.now()}_${Math.floor(Math.random() * 10000)}`;
const SETUP_CODE = "first-run-preset-code-000";

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
  installTestEnv({ DATABASE_URL: scratchUrl(), TOPOS_SETUP_CODE: SETUP_CODE });
  await applyPlaneDdl(scratchUrl());
  const WEB_ROOT = resolve(__dirname, "..", "..");
  execFileSync("node", [join(WEB_ROOT, "scripts", "migrate.mjs")], {
    env: { ...process.env, DATABASE_URL: scratchUrl() },
    stdio: "pipe",
  });
}, 60000);

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE IF EXISTS ${SCRATCH} WITH (FORCE)`);
});

describe("the first-boot setup ceremony", () => {
  it("multi tenancy skips the boot mint — no workspace, no claim code", async () => {
    // A virgin database: multi tenancy mints NOTHING (workspaces are born through the superset's
    // own creation surface, not the single-tenant genesis ceremony), so the single-tenant read
    // stays null and no claim link is printed. Runs first, while the DB is still virgin.
    const identity = await import("@/lib/db/identity.server");
    await identity.ensureSetup("http://localhost:3000", "multi");
    expect(await identity.theWorkspace()).toBeNull();
  });

  it("theWorkspace() is null on a virgin database, then the boot-minted, unclaimed row", async () => {
    const identity = await import("@/lib/db/identity.server");
    expect(await identity.theWorkspace()).toBeNull();

    await identity.ensureSetup("http://localhost:3000");

    const ws = await identity.theWorkspace();
    expect(ws).not.toBeNull();
    // Unclaimed: the claim moment is still open, and the code hash is live (the CHECK ties them).
    expect(ws?.claimedAt).toBeNull();
    // The default address slug (TOPOS_WORKSPACE_NAME unset ⇒ 'team') names the display too.
    expect(ws?.name).toBe("team");
    expect(ws?.displayName).toBe("team");
    // Born with its default channel — every workspace is.
    const channels = await q<{ name: string; is_default: boolean }>(
      `SELECT name, is_default FROM web.channel WHERE workspace_id = $1`,
      [ws?.id],
    );
    expect(channels).toEqual([{ name: "everyone", is_default: true }]);
  });

  it("claimableWorkspace hits only with the right code — every other probe is the uniform miss", async () => {
    const identity = await import("@/lib/db/identity.server");
    const ws = await identity.theWorkspace();
    expect(await identity.claimableWorkspace(SETUP_CODE)).toEqual({
      id: ws?.id,
      name: "team",
      displayName: "team",
    });
    expect(await identity.claimableWorkspace("not-the-setup-code")).toBeNull();
    expect(await identity.claimableWorkspace("")).toBeNull();
  });

  it("a preset TOPOS_SETUP_CODE is the stored hash's preimage — stable for CI/IaC", async () => {
    // Only the SHA-256 lands in the row; the equality is computed IN Postgres, like production.
    const rows = await q<{ preset: boolean }>(
      `SELECT claim_code_sha256 = sha256(convert_to($1, 'UTF8')) AS preset FROM web.workspace`,
      [SETUP_CODE],
    );
    expect(rows).toEqual([{ preset: true }]);
  });
});
