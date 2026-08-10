import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { bootWorkspace, createScratchDb, type ScratchDb } from "./helpers/scratch-db";

/**
 * The bundle-kind CHECK arrives as an EAGER constraint, so it validates the rows already in the
 * table — and a database written before the vocabulary closed may hold a kind no release defines.
 * That must fail: rewriting a bundle's kind is not a migration's decision to make (kind is birth
 * metadata the whole system branches on). But failing with a bare
 * `check constraint "bundle_kind_check" is violated by some row` tells an operator nothing about
 * WHICH rows or what to do, and it happens at process start.
 *
 * So the shipped migration leads with a guard that names the offending kinds and the two ways out.
 * These tests run the COMMITTED SQL against a legacy-shaped table — the constraint dropped, an
 * out-of-vocabulary row inserted — rather than a transcription of it, so the file itself is what
 * is under test.
 */

const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

/** The committed migration, split the way drizzle splits it. */
function migrationStatements(): string[] {
  const sql = readFileSync(join(WEB_ROOT, "drizzle", "0013_bundle-kind-check.sql"), "utf8");
  return sql
    .split("--> statement-breakpoint")
    .map((s) => s.trim())
    .filter((s) => s.length > 0);
}

let db: ScratchDb;
let wsId: string;

beforeAll(async () => {
  db = await createScratchDb("web_kind_mig", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
}, 60000);

afterAll(async () => {
  await db.drop();
});

/** Put the table back the way a pre-this-release database held it. */
async function makeLegacy(): Promise<void> {
  await db.q(`ALTER TABLE web.bundle DROP CONSTRAINT IF EXISTS bundle_kind_check`);
}

async function runMigration(): Promise<unknown> {
  for (const statement of migrationStatements()) {
    const failure = await db
      .q(statement)
      .then(() => undefined)
      .catch((e: unknown) => e);
    if (failure !== undefined) {
      return failure;
    }
  }
  return undefined;
}

describe("migration 0013 refuses a legacy row rather than rewriting it", () => {
  it("names the offending kinds and the operator's two options", async () => {
    await makeLegacy();
    await db.q(`INSERT INTO web.bundle (id, workspace_id, name, kind) VALUES ($1, $2, $3, $4)`, [
      "s_legacy_alien",
      wsId,
      "legacy-alien",
      "knowledge",
    ]);

    const error = (await runMigration()) as { message?: string; hint?: string } | undefined;
    expect(error).toBeDefined();
    // The KINDS are named — an operator can find the rows from the message alone.
    expect(error?.message).toContain("'knowledge'");
    expect(error?.message).toContain("web.bundle");
    // And both ways out, plus the refusal to decide for them.
    expect(error?.hint).toContain("DELETE");
    expect(error?.hint).toContain("UPDATE");
    expect(error?.hint).toContain("'skill'");
    expect(error?.hint).toContain("'mcp'");

    // FAIL-CLOSED: the constraint did not land, and the row was not touched.
    const rows = await db.q<{ kind: string }>(
      `SELECT kind FROM web.bundle WHERE id = 's_legacy_alien'`,
    );
    expect(rows[0]?.kind).toBe("knowledge");
    const constraints = await db.q<{ conname: string }>(
      `SELECT conname FROM pg_constraint WHERE conname = 'bundle_kind_check'`,
    );
    expect(constraints).toHaveLength(0);
  });

  it("applies cleanly once the operator has settled those rows", async () => {
    await db.q(`DELETE FROM web.bundle WHERE id = 's_legacy_alien'`);
    expect(await runMigration()).toBeUndefined();
    const constraints = await db.q<{ conname: string }>(
      `SELECT conname FROM pg_constraint WHERE conname = 'bundle_kind_check'`,
    );
    expect(constraints).toHaveLength(1);
  });
});
