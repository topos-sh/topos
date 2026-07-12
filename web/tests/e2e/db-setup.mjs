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

/** The grant shape the unit `topos_test` database already holds: broad SELECT + the exact
 * guarded-function DML edges (topos_web writes the authority tables ONLY through the topos_*
 * functions, which run SECURITY INVOKER, so it needs DML on precisely what they touch — UPDATE at
 * COLUMN grain, so the role cannot reach a column no guarded function writes). Production
 * provisioning must carry this same shape: this file is the in-repo record of it. */
const PLANE_GRANTS = `
GRANT USAGE ON SCHEMA plane TO topos_web;
GRANT SELECT ON ALL TABLES IN SCHEMA plane TO topos_web;
GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA plane TO topos_web;
GRANT INSERT ON plane.channel_events, plane.channel_members, plane.workspace_member, plane.workspace_policy TO topos_web;
GRANT INSERT ON plane.skill_detachments TO topos_web;
GRANT DELETE ON plane.channel_members, plane.channel_skills, plane.channels, plane.workspace_member TO topos_web;
-- UPDATE is granted at COLUMN grain — exactly the columns the guarded functions write. The
-- row/byte rule is enforced by grants, not convention: a table-wide UPDATE on device_registry
-- would let a compromised web tier rewrite a device's credential hash and then drive the DEVICE
-- lane as that device, which no guarded function can do.
GRANT UPDATE (review_required, invite_policy, staleness_window_ms) ON plane.workspace_policy TO topos_web;
GRANT UPDATE (role, invited_by) ON plane.workspace_member TO topos_web;
GRANT UPDATE (acked_at) ON plane.notices TO topos_web;
GRANT UPDATE (name) ON plane.channels TO topos_web;
GRANT UPDATE (detached, detached_at) ON plane.device_skill_state TO topos_web;
GRANT UPDATE (revoked) ON plane.device_registry TO topos_web;
ALTER DEFAULT PRIVILEGES FOR ROLE topos_plane IN SCHEMA plane GRANT SELECT ON TABLES TO topos_web;
`;

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

    const probe = await db.query("SELECT to_regclass('plane.workspace') AS t");
    if (probe.rows[0]?.t === null) {
      // Apply the plane migrations AS topos_plane (SET ROLE does not re-read the role search_path,
      // so set it explicitly). Each file is one simple-protocol query (dollar-quoted bodies + all).
      await db.query("SET ROLE topos_plane");
      await db.query("SET search_path = plane");
      for (const file of planeMigrationFiles()) {
        await db.query(readFileSync(join(MIGRATIONS_DIR, file), "utf8"));
      }
      await db.query("RESET ROLE");
    }

    // Grants (idempotent; superuser may grant on topos_plane's tables).
    await db.query(PLANE_GRANTS);
  } finally {
    await db.end();
  }
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
