/**
 * The e2e database bootstrap — playwright's `globalSetup`, also runnable standalone
 * (`node tests/e2e/db-setup.mjs`). It provisions a SEPARATE database `topos_e2e` on the same
 * Postgres server the unit lane's `topos_test` uses, so the two never collide.
 *
 * What it does, idempotently, mirroring scripts/compose-init-db.sh (the production first-boot
 * provisioning — ONE ROLE PER APPLICATION, each owning its schema and running its own
 * migration lineage):
 *  1. CREATE the two app roles + DATABASE topos_e2e (once), the connect/create grants, the
 *     per-database role search_paths (web, plane / plane), the two schemas each owned by its
 *     role, and the ALTER DEFAULT PRIVILEGES chain that keeps the app's read-only view of
 *     custody state current across future plane migrations.
 *  2. Into `plane`: apply the vault's in-repo SQL migrations AS `topos_plane` (ownership + the
 *     default-privileges chain match production boot). Fresh lineage: the applied set reads
 *     from the `plane._sqlx_migrations` ledger alone.
 *  3. Into `web`: run the app's OWN drizzle migrator (scripts/migrate.mjs) — it creates the
 *     web-tier tables and the `web.__drizzle_migrations` LEDGER. Recording the ledger is the
 *     point: the app's first-request migration then sees it and no-ops.
 */
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
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
    // Idempotent, exactly like compose-init-db.sh: only the two app roles connect; topos_web may
    // create (the drizzle migrator runs CREATE SCHEMA IF NOT EXISTS web, and Postgres checks
    // CREATE-on-database before honoring it).
    await client.query(`REVOKE ALL ON DATABASE ${DB_NAME} FROM PUBLIC`);
    await client.query(`GRANT CONNECT ON DATABASE ${DB_NAME} TO topos_plane`);
    await client.query(`GRANT CONNECT ON DATABASE ${DB_NAME} TO topos_web`);
    await client.query(`GRANT CREATE ON DATABASE ${DB_NAME} TO topos_web`);
    // Role-level search_path, per database (probed by LOGGING IN as the role — SET ROLE does not
    // adopt it): the app's own tables lead, the custody mirror follows.
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
    // Schemas are born superuser-side with AUTHORIZATION (the owning role holds no CREATE on the
    // database), exactly like the compose init.
    await db.query("CREATE SCHEMA IF NOT EXISTS plane AUTHORIZATION topos_plane");
    await db.query("CREATE SCHEMA IF NOT EXISTS web AUTHORIZATION topos_web");
    // The app's read-only view of custody state: every table a plane migration adds arrives
    // already SELECT-granted to the web role.
    await db.query("GRANT USAGE ON SCHEMA plane TO topos_web");
    await db.query(
      "ALTER DEFAULT PRIVILEGES FOR ROLE topos_plane IN SCHEMA plane GRANT SELECT ON TABLES TO topos_web",
    );

    // Apply the PENDING plane migrations AS topos_plane (SET ROLE does not re-read the role
    // search_path, so set it explicitly). Fresh lineage: the applied set reads from the sqlx
    // ledger alone — no marker probes, no pre-ledger fallback. This bootstrap RECORDS what it
    // applies in that same ledger (sqlx's own shape, real SHA-384 checksums), so it is
    // idempotent across the CI double invocation (standalone, then again as playwright's
    // globalSetup) and the vault's own migrator would see the set as applied.
    await db.query("SET ROLE topos_plane");
    await db.query("SET search_path = plane");
    await db.query(`CREATE TABLE IF NOT EXISTS _sqlx_migrations (
      version BIGINT PRIMARY KEY,
      description TEXT NOT NULL,
      installed_on TIMESTAMPTZ NOT NULL DEFAULT now(),
      success BOOLEAN NOT NULL,
      checksum BYTEA NOT NULL,
      execution_time BIGINT NOT NULL
    )`);
    const applied = await appliedMigrationVersions(db);
    for (const file of planeMigrationFiles()) {
      if (!applied.has(migrationVersion(file))) {
        const source = readFileSync(join(MIGRATIONS_DIR, file), "utf8");
        await db.query(source);
        await db.query(
          `INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
           VALUES ($1, $2, true, $3, 0)`,
          [
            migrationVersion(file),
            file.replace(/^\d+_/, "").replace(/\.sql$/, ""),
            createHash("sha384").update(source).digest(),
          ],
        );
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
 * The versions already applied, from the `plane._sqlx_migrations` ledger the vault's own
 * migrator writes. No ledger = a fresh schema = apply everything (this bootstrap may predate
 * the vault's first boot on a shared dev database — the vault's migrator then no-ops).
 */
async function appliedMigrationVersions(db) {
  const ledger = await db.query("SELECT to_regclass('_sqlx_migrations') AS t");
  if (ledger.rows[0]?.t !== null) {
    const { rows } = await db.query("SELECT version FROM _sqlx_migrations");
    return new Set(rows.map((r) => Number(r.version)));
  }
  return new Set();
}

/** Run the app's OWN drizzle migrator against topos_e2e (creates the tables + the
 * `web.__drizzle_migrations` ledger). The running app's first-request migration then finds the
 * ledger and no-ops. */
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
