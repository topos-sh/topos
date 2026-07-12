#!/usr/bin/env node
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
/**
 * migrate.mjs — apply the committed drizzle migrations to DATABASE_URL.
 *
 * A thin, dependency-light runner for the migrations the app also runs at first request
 * (app/lib/db/migrate.server.ts). Used to bootstrap a dev/test database and to prove the
 * migration set applies cleanly. The ledger is pinned to `web.__drizzle_migrations` — the same
 * (schema, table) the app and drizzle.config.ts declare — so one dump/restore carries the tables
 * and their ledger together, and the connecting role's `search_path=web` places every table in
 * the web schema.
 *
 *   DATABASE_URL=postgres://topos_web:web@localhost:5439/topos_dev node scripts/migrate.mjs
 */
import { drizzle } from "drizzle-orm/node-postgres";
import { migrate } from "drizzle-orm/node-postgres/migrator";
import { Pool } from "pg";

const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const url = process.env.DATABASE_URL;
if (!url) {
  console.error("migrate.mjs: DATABASE_URL is required");
  process.exit(1);
}

const pool = new Pool({ connectionString: url });
try {
  await migrate(drizzle(pool), {
    migrationsFolder: join(WEB_ROOT, "drizzle"),
    migrationsSchema: "web",
    migrationsTable: "__drizzle_migrations",
  });
  console.warn("migrations applied");
} finally {
  await pool.end();
}
