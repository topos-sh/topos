import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Client, type ClientConfig } from "pg";

/**
 * The ONE plane-DDL applier the unit suite uses. It applies the directory's IN-REPO SQL
 * migrations (`crates/plane-store/migrations/*.sql` — the single source of truth; no vendoring),
 * so a test database gets the REAL authority DDL: the 0010 canonical-principal CHECK makes a
 * non-canonical seed a loud error, and the guarded `topos_*` policy functions (0015/0016) are
 * present so the DAL's function calls run against the real thing.
 *
 * Ordering facts the applier encodes:
 *  - CREATE SCHEMA runs BEFORE any SET ROLE (Postgres checks CREATE-on-database before honoring
 *    IF NOT EXISTS).
 *  - `SET search_path = plane` ALWAYS runs so the migrations' unqualified CREATE TABLEs land in
 *    schema `plane`.
 */

const HERE = fileURLToPath(new URL(".", import.meta.url));
const MIGRATIONS_DIR = resolve(HERE, "..", "..", "..", "crates", "plane-store", "migrations");

export type PlaneDdlTarget = Client | string | ClientConfig;

/** The migration filenames, in apply order. */
export function planeMigrationFiles(): string[] {
  return readdirSync(MIGRATIONS_DIR)
    .filter((name) => name.endsWith(".sql"))
    .sort();
}

async function withClient<T>(target: PlaneDdlTarget, fn: (db: Client) => Promise<T>): Promise<T> {
  if (target instanceof Client) {
    return fn(target);
  }
  const db = new Client(typeof target === "string" ? { connectionString: target } : target);
  await db.connect();
  try {
    return await fn(db);
  } finally {
    await db.end();
  }
}

/**
 * Ensure schema `plane` exists, then apply the migration files in filename order. The connection
 * must be an ADMIN one (a superuser, or a role that owns the database): a scratch database is
 * owned by whoever created it, so no per-table grants are needed to seed and read it.
 */
export async function applyPlaneDdl(target: PlaneDdlTarget): Promise<void> {
  await withClient(target, async (db) => {
    await db.query("CREATE SCHEMA IF NOT EXISTS plane");
    await db.query("SET search_path = plane");
    for (const file of planeMigrationFiles()) {
      await db.query(readFileSync(join(MIGRATIONS_DIR, file), "utf8"));
    }
  });
}
