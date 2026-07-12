import { drizzle, type NodePgDatabase } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import { serverEnv } from "@/env.server";
import * as schema from "./schema";

export type Db = NodePgDatabase<typeof schema>;

// Lazy singletons: env is parsed at first use, not at import (CI builds run without secrets).
let pool: Pool | undefined;
let db: Db | undefined;

export function getPool(): Pool {
  pool ??= new Pool({ connectionString: serverEnv().DATABASE_URL });
  return pool;
}

export function getDb(): Db {
  db ??= drizzle(getPool(), { schema });
  return db;
}
