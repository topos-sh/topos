import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { Client } from "pg";
import { sha256Hex } from "./crypto";
import type { Logger } from "./log";

/**
 * Boot migration — the web app's discipline, hand-rolled: an ordered plain-SQL lineage under
 * `gateway/migrations/`, applied before the first listener binds, under an advisory lock so
 * replicas racing at boot serialize instead of colliding. The ledger is the gateway's own
 * (`gateway.__migrations`), one row per applied file, sha256-pinned: an applied file that later
 * changes on disk is a fork of history and refuses loudly instead of silently diverging.
 */

const MIGRATION_LOCK_KEY = "topos:gateway:boot-migration";

export function migrationFiles(dir: string): string[] {
  return readdirSync(dir)
    .filter((name) => name.endsWith(".sql"))
    .sort();
}

export async function runMigrations(
  databaseUrl: string,
  migrationsDir: string,
  log: Logger,
): Promise<void> {
  // One dedicated connection holds the advisory lock for the whole sweep — session-level, so it
  // survives the per-file transactions and dies with the connection on any failure path.
  const client = new Client({ connectionString: databaseUrl });
  await client.connect();
  try {
    await client.query("SELECT pg_advisory_lock(hashtextextended($1, 0))", [MIGRATION_LOCK_KEY]);
    try {
      await client.query("CREATE SCHEMA IF NOT EXISTS gateway");
      await client.query(
        `CREATE TABLE IF NOT EXISTS gateway.__migrations (
           filename text PRIMARY KEY,
           sha256 text NOT NULL,
           applied_at timestamptz NOT NULL DEFAULT now()
         )`,
      );
      const applied = new Map<string, string>();
      const rows = await client.query<{ filename: string; sha256: string }>(
        "SELECT filename, sha256 FROM gateway.__migrations",
      );
      for (const row of rows.rows) {
        applied.set(row.filename, row.sha256);
      }
      for (const filename of migrationFiles(migrationsDir)) {
        const sql = readFileSync(join(migrationsDir, filename), "utf8");
        const digest = sha256Hex(sql);
        const seen = applied.get(filename);
        if (seen !== undefined) {
          if (seen !== digest) {
            throw new Error(`migration ${filename} changed after it was applied`);
          }
          continue;
        }
        // The file and its ledger row land in ONE transaction: a half-applied migration leaves
        // no row, so the next boot retries it whole.
        await client.query("BEGIN");
        try {
          await client.query(sql);
          await client.query(
            "INSERT INTO gateway.__migrations (filename, sha256) VALUES ($1, $2)",
            [filename, digest],
          );
          await client.query("COMMIT");
        } catch (error) {
          await client.query("ROLLBACK");
          throw error;
        }
        log("info", "migration applied", { filename });
      }
    } finally {
      await client.query("SELECT pg_advisory_unlock(hashtextextended($1, 0))", [
        MIGRATION_LOCK_KEY,
      ]);
    }
  } finally {
    await client.end();
  }
}
