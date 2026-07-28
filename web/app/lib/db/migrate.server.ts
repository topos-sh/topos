import fs from "node:fs";
import path from "node:path";
import { migrate } from "drizzle-orm/node-postgres/migrator";
import { getDb, getPool } from "./index.server";

/**
 * One constant advisory-lock key for the web tier's boot migration, shared by every process that
 * migrates this schema. Arbitrary but immutable: changing it would let an old and a new build
 * migrate concurrently across a deploy.
 */
const MIGRATION_LOCK_KEY = "topos:web:boot-migration";

/**
 * The committed migrations folder lives at the package root (`drizzle/`). In dev that is the
 * cwd; the built server (`react-router-serve ./build/server/index.js`) is launched from the
 * same package dir, so cwd-relative resolution covers both. A SUPERSET build's cwd carries its
 * OWN drizzle folder (a different schema's journal), so `WEB_MIGRATIONS_DIR` overrides the
 * search outright — the deployment points the boot migrator at THIS app's journal explicitly.
 * Missing folder = a broken build; fail loudly rather than serve unmigrated.
 */
function resolveMigrationsFolder(): string {
  const override = process.env.WEB_MIGRATIONS_DIR;
  const candidates = override
    ? [override]
    : [
        path.join(process.cwd(), "drizzle"),
        // Tooling launched from the repo root rather than the package dir.
        path.join(process.cwd(), "web", "drizzle"),
      ];
  for (const candidate of candidates) {
    if (fs.existsSync(path.join(candidate, "meta", "_journal.json"))) {
      return candidate;
    }
  }
  throw new Error(`drizzle migrations folder not found (looked in: ${candidates.join(", ")})`);
}

/**
 * Drizzle's migrator takes NO lock of its own: two processes booting against a database with
 * pending migrations both read the same ledger tail and both replay the same DDL. Single-process
 * deploys never hit that, but any overlap does — a rolling replacement, `--scale`, or a crash-loop
 * restart racing a deploy. A session-scoped Postgres advisory lock serializes the whole run: the
 * second boot blocks until the first finishes, then finds the ledger current and applies nothing.
 * The lock must be TAKEN and RELEASED on one dedicated connection (advisory locks are
 * session-scoped; the migrator itself runs on other pool connections while we hold it), and a
 * dropped connection releases it implicitly — no wedged-lock failure mode.
 */
export async function runMigrations(): Promise<void> {
  const client = await getPool().connect();
  try {
    await client.query("select pg_advisory_lock(hashtextextended($1, 0))", [MIGRATION_LOCK_KEY]);
    await runMigrationsLocked();
  } finally {
    try {
      await client.query("select pg_advisory_unlock(hashtextextended($1, 0))", [
        MIGRATION_LOCK_KEY,
      ]);
    } finally {
      client.release();
    }
  }
}

async function runMigrationsLocked(): Promise<void> {
  await migrate(getDb(), {
    migrationsFolder: resolveMigrationsFolder(),
    // The ledger is PINNED into the web tier's own schema (web.__drizzle_migrations), never
    // drizzle's default `drizzle` schema: the web tier owns exactly ONE schema in the shared
    // database, so one dump/restore carries the tables and their ledger together. NB the
    // migrator unconditionally runs `CREATE SCHEMA IF NOT EXISTS web`, and Postgres checks
    // CREATE-on-database BEFORE honoring the IF NOT EXISTS — the deployment grants topos_web
    // exactly that. Must match drizzle.config.ts `migrations:`.
    migrationsSchema: "web",
    migrationsTable: "__drizzle_migrations",
  });
}
