import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The first-run probe `hasAnyWorkspace()` (app/lib/db/resolve.server.ts) against a REAL scratch
 * Postgres stood up from the in-repo authority migrations. It is the single actor-less read the
 * data layer permits: it discloses ONE boolean about the deployment — is this a virgin plane? —
 * and the virgin landing / workspaces-index claim CTA both branch on it.
 *
 * The e2e suite cannot assert the virgin state (the shared topos_e2e DB is globally seeded with
 * workspaces before any spec runs), so the empty-plane boolean is proven HERE, on an isolated
 * scratch database created in beforeAll and dropped in afterAll — false on a fresh schema, true
 * the moment a single workspace row lands.
 */

const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:postgres@localhost:5439/topos_test";
const SCRATCH = `web_first_run_test_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

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
async function scratchQuery(sql: string, params: unknown[] = []): Promise<void> {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().query(sql, params);
}

async function probe() {
  const { hasAnyWorkspace } = await import("@/lib/db/resolve.server");
  return hasAnyWorkspace();
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  installTestEnv({ DATABASE_URL: scratchUrl() });
  // The REAL authority DDL — schema `plane` with the workspace table hasAnyWorkspace counts.
  await applyPlaneDdl(scratchUrl());
});

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE ${SCRATCH} WITH (FORCE)`);
});

describe("hasAnyWorkspace (the virgin-plane probe)", () => {
  it("is false on a fresh plane, true after the first workspace row lands", async () => {
    // A virgin plane: schema stood up, zero workspace rows.
    expect(await probe()).toBe(false);

    // Standing up the first workspace flips the boolean — the claim moment is over.
    await scratchQuery(
      `INSERT INTO plane.workspace
         (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
       VALUES ($1, $2, 'unverified', 'cloud', $3, $4)`,
      ["w_first", "First Workspace", "2026-07-01T00:00:00Z", "first"],
    );
    expect(await probe()).toBe(true);

    // A second workspace keeps it true — it is an existence probe, never a count.
    await scratchQuery(
      `INSERT INTO plane.workspace
         (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
       VALUES ($1, $2, 'unverified', 'cloud', $3, $4)`,
      ["w_second", "Second Workspace", "2026-07-02T00:00:00Z", "second"],
    );
    expect(await probe()).toBe(true);
  });
});
