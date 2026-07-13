/**
 * The e2e database bootstrap — playwright's `globalSetup`, also runnable standalone
 * (`node tests/e2e/db-setup.mjs`). It provisions a SEPARATE database `topos_e2e` on the same
 * Postgres server the unit lane's `topos_test` uses, so the two never collide.
 *
 * What it does, idempotently:
 *  1. CREATE DATABASE topos_e2e (once), with the connect/create grants + per-database search_path
 *     the two app roles need.
 *  2. Into `plane`: apply the in-repo plane SQL migrations AS `topos_plane` (ownership + the init's
 *     default-privileges chain match production boot), then mirror the grant shape the unit
 *     database already holds — broad SELECT + the guarded-function DML edges (topos_web writes the
 *     authority tables ONLY through the `topos_*` SECURITY-INVOKER functions).
 *  3. Into `web`: run the app's OWN drizzle migrator (scripts/migrate.mjs) — it creates schema web,
 *     the web-tier tables, and the `web.__drizzle_migrations` LEDGER. Recording the ledger is the
 *     point: the app's first-request migration then sees it and no-ops, so pre-seeding here never
 *     collides with the running app's `CREATE TABLE`s.
 *
 * The plane migrations run ONLY on a fresh `plane` schema (they are append-only); the grants + the
 * web migrator are both safe to re-run.
 */
import { execFileSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath, pathToFileURL } from "node:url";
import pg from "pg";

const { Client } = pg;

const HERE = resolve(fileURLToPath(import.meta.url), "..");
const WEB_ROOT = resolve(HERE, "..", "..");
const REPO_ROOT = resolve(WEB_ROOT, "..");
const MIGRATIONS_DIR = resolve(REPO_ROOT, "crates", "plane-store", "migrations");

// Kept in sync with tests/e2e/env.ts (that file is TypeScript; this one runs under plain node, so
// it cannot import it — the defaults are duplicated deliberately, overridable via env).
const MAINTENANCE_URL =
  process.env.E2E_MAINTENANCE_URL ?? "postgres://postgres:postgres@localhost:5439/postgres";
const ADMIN_URL =
  process.env.E2E_ADMIN_URL ?? "postgres://postgres:postgres@localhost:5439/topos_e2e";
const WEB_URL = process.env.DATABASE_URL ?? "postgres://topos_web:web@localhost:5439/topos_e2e";
const DB_NAME = "topos_e2e";

/* The topos_web grant shape (broad SELECT + the guarded-function DML edges, UPDATE at COLUMN
 * grain) now lives IN the plane migrations — 0019 onward carry the grants next to the schema
 * they bound, so applying the migrations AS topos_plane below grants the web role too. The one
 * ordering rule that leaves here: BOTH roles must exist BEFORE the migrations run (0019 skips
 * the web grants when the role is absent, and a deployment that creates the role afterward
 * without re-granting fails CLOSED — the web tier cannot read). `ensureDatabase` creates the
 * roles first for exactly that reason; compose initdb and production provisioning do the same. */

function planeMigrationFiles() {
  return readdirSync(MIGRATIONS_DIR)
    .filter((name) => name.endsWith(".sql"))
    .sort();
}

async function ensureDatabase() {
  const client = new Client({ connectionString: MAINTENANCE_URL });
  await client.connect();
  try {
    // A virgin server (CI's service container, a fresh laptop) holds neither app role — create
    // them idempotently so this script is the ONE bootstrap a contributor needs.
    for (const [role, password] of [
      ["topos_plane", "plane"],
      ["topos_web", "web"],
    ]) {
      const { rows } = await client.query("SELECT 1 FROM pg_roles WHERE rolname = $1", [role]);
      if (rows.length === 0) {
        await client.query(`CREATE ROLE ${role} LOGIN PASSWORD '${password}'`);
      }
    }
    const { rows } = await client.query("SELECT 1 FROM pg_database WHERE datname = $1", [DB_NAME]);
    if (rows.length === 0) {
      await client.query(`CREATE DATABASE ${DB_NAME}`);
    }
    // Idempotent: only the two app roles connect; topos_web may create (the drizzle migrator runs
    // CREATE SCHEMA IF NOT EXISTS web, and Postgres checks CREATE-on-database before honoring it).
    await client.query(`REVOKE ALL ON DATABASE ${DB_NAME} FROM PUBLIC`);
    await client.query(`GRANT CONNECT ON DATABASE ${DB_NAME} TO topos_plane`);
    await client.query(`GRANT CONNECT ON DATABASE ${DB_NAME} TO topos_web`);
    await client.query(`GRANT CREATE ON DATABASE ${DB_NAME} TO topos_web`);
    // topos_web keeps its own unqualified tables in `web` (first), but the DAL invokes the guarded
    // `topos_*` authority functions unqualified — so `plane` must be on the path too. The plane
    // model tables are addressed schema-qualified (Drizzle pgSchema("plane")), so nothing there is
    // ambiguous. Per-database only: the unit lane's `topos_test` search_path is untouched.
    await client.query(`ALTER ROLE topos_web IN DATABASE ${DB_NAME} SET search_path = web, plane`);
    await client.query(`ALTER ROLE topos_plane IN DATABASE ${DB_NAME} SET search_path = plane`);
  } finally {
    await client.end();
  }
}

async function bootstrapPlane() {
  const db = new Client({ connectionString: ADMIN_URL });
  await db.connect();
  try {
    // CREATE SCHEMA runs BEFORE any SET ROLE (topos_plane holds no CREATE on the database, so a
    // SET-ROLE'd CREATE SCHEMA fails 42501 even with IF NOT EXISTS). AUTHORIZATION keeps a freshly
    // created schema owned like the production init's.
    await db.query("CREATE SCHEMA IF NOT EXISTS plane AUTHORIZATION topos_plane");

    // Apply the PENDING plane migrations AS topos_plane (SET ROLE does not re-read the role
    // search_path, so set it explicitly). A fresh schema applies them all; an existing dev
    // database picks up only what landed since (compared by leading migration number against
    // the sqlx ledger, so a re-run never re-executes an applied file). Each file is one
    // simple-protocol query (dollar-quoted bodies + all).
    await db.query("SET ROLE topos_plane");
    await db.query("SET search_path = plane");
    const applied = await appliedMigrationVersions(db);
    for (const file of planeMigrationFiles()) {
      if (!applied.has(migrationVersion(file))) {
        await db.query(readFileSync(join(MIGRATIONS_DIR, file), "utf8"));
      }
    }
    await db.query("RESET ROLE");
  } finally {
    await db.end();
  }
}

/** A migration file's version = its leading digits (sqlx's own convention). */
function migrationVersion(file) {
  return Number.parseInt(file, 10);
}

/**
 * The versions already applied, from wherever the ledger is. The plane binary's migrator writes
 * `_sqlx_migrations`; THIS bootstrap predates it on a fresh schema (no ledger, nothing applied).
 * Databases this file bootstrapped before the pending-migrations rework carry no ledger either —
 * for those, fall back to "which migration's tables/functions exist" via a probe of the LAST
 * fully-applied file's marker object. Simplest honest probe: the highest numbered marker below.
 */
async function appliedMigrationVersions(db) {
  const ledger = await db.query("SELECT to_regclass('_sqlx_migrations') AS t");
  if (ledger.rows[0]?.t !== null) {
    const { rows } = await db.query("SELECT version FROM _sqlx_migrations");
    return new Set(rows.map((r) => Number(r.version)));
  }
  const probe = await db.query("SELECT to_regclass('workspace') AS t");
  if (probe.rows[0]?.t === null) {
    return new Set(); // virgin schema — apply everything
  }
  // Pre-ledger bootstrap: probe the marker objects that tell the applied prefix apart. Each
  // entry is [version, EXISTS-probe]; extend when a new migration lands in this fallback era
  // (post-0019 databases always carry the marker function below or were ledger-migrated).
  const markers = [[19, "SELECT to_regproc('topos_delivery') IS NOT NULL AS ok"]];
  const applied = new Set();
  for (let v = 1; v <= 18; v += 1) {
    applied.add(v); // this fallback only exists for databases bootstrapped at 0018
  }
  for (const [version, sql] of markers) {
    const { rows } = await db.query(sql);
    if (rows[0]?.ok === true) {
      applied.add(version);
    }
  }
  return applied;
}

/** Run the app's OWN drizzle migrator against topos_e2e (creates schema web + tables + ledger). The
 * running app's first-request migration then finds the ledger and no-ops. */
function bootstrapWeb() {
  execFileSync("node", [join(WEB_ROOT, "scripts", "migrate.mjs")], {
    env: { ...process.env, DATABASE_URL: WEB_URL },
    stdio: "inherit",
  });
}

export async function setupE2eDatabase() {
  await ensureDatabase();
  await bootstrapPlane();
  bootstrapWeb();
}

// playwright globalSetup entry point.
export default setupE2eDatabase;

// Standalone: `node tests/e2e/db-setup.mjs`.
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  setupE2eDatabase()
    .then(() => {
      console.warn(`e2e database ${DB_NAME} bootstrapped.`);
      process.exit(0);
    })
    .catch((error) => {
      console.error(error);
      process.exit(1);
    });
}
