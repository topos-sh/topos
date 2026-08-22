import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { Client, type ClientConfig } from "pg";

/**
 * The ONE gateway-DDL applier the unit suite uses. It applies the gateway's IN-REPO SQL lineage
 * (`gateway/migrations/*.sql` — the single source of truth; no vendoring), so a test database
 * gets the REAL DDL the app's read-only mirror (schema.gateway.ts) runs against — the same
 * discipline as plane-ddl.ts.
 *
 * The lineage GRANTs its web-readable tables to role `topos_web`, so that role is created here
 * (NOLOGIN — nothing in the unit suite logs in as it; the grants-by-login proof lives in
 * `scripts/check-db-grants.sh` and the gateway package's own suite) before the files run.
 */

const HERE = fileURLToPath(new URL(".", import.meta.url));
const MIGRATIONS_DIR = resolve(HERE, "..", "..", "..", "gateway", "migrations");

export type GatewayDdlTarget = Client | string | ClientConfig;

/** The migration filenames, in apply order. */
function gatewayMigrationFiles(): string[] {
  return readdirSync(MIGRATIONS_DIR)
    .filter((name) => name.endsWith(".sql"))
    .sort();
}

async function withClient<T>(target: GatewayDdlTarget, fn: (db: Client) => Promise<T>): Promise<T> {
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
 * Ensure the roles the lineage grants to exist, then apply the migration files in filename
 * order. The connection must be an ADMIN one (a superuser, or a role that owns the database).
 */
export async function applyGatewayDdl(target: GatewayDdlTarget): Promise<void> {
  await withClient(target, async (db) => {
    // Cluster-wide and racy across parallel suites; the catch is the IF-NOT-EXISTS.
    for (const role of ["topos_web", "topos_gateway"]) {
      await db.query(`CREATE ROLE ${role} NOLOGIN`).catch((error: unknown) => {
        if ((error as { code?: string }).code !== "42710") {
          throw error;
        }
      });
    }
    await db.query("CREATE SCHEMA IF NOT EXISTS gateway");
    await db.query("SET search_path = gateway");
    for (const file of gatewayMigrationFiles()) {
      await db.query(readFileSync(join(MIGRATIONS_DIR, file), "utf8"));
    }
  });
}
