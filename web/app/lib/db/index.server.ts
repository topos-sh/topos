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

/**
 * Whether an error is a Postgres unique violation (23505). Drizzle wraps driver errors in a
 * DrizzleQueryError carrying the pg error on `.cause`, so the code must be read through BOTH
 * layers — a bare `.code` check silently never fires.
 */
export function isUniqueViolation(error: unknown): boolean {
  if (typeof error !== "object" || error === null) {
    return false;
  }
  if ((error as { code?: string }).code === "23505") {
    return true;
  }
  const cause = (error as { cause?: unknown }).cause;
  return (
    typeof cause === "object" && cause !== null && (cause as { code?: string }).code === "23505"
  );
}
