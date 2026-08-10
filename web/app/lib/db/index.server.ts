import { drizzle, type NodePgDatabase } from "drizzle-orm/node-postgres";
import { Pool } from "pg";
import { serverEnv } from "@/env.server";
import * as schema from "./schema";

export type Db = NodePgDatabase<typeof schema>;

// Lazy singletons: env is parsed at first use, not at import (CI builds run without secrets).
let pool: Pool | undefined;
let db: Db | undefined;

export function getPool(): Pool {
  if (pool === undefined) {
    const opened = new Pool({ connectionString: serverEnv().DATABASE_URL });
    // A POOL WITH NO `error` LISTENER TAKES THE PROCESS DOWN. pg re-emits an IDLE client's error
    // on the pool, and an EventEmitter's unheard `error` event throws — so a Postgres restart, a
    // failover, or an administrator terminating a backend (`57P01`) becomes an unhandled throw in
    // whatever code happened to be running, rather than the non-event it is. The pool discards the
    // broken client and opens another on the next checkout; a query actually IN FLIGHT still
    // rejects at its own call site, which is where it can be handled.
    opened.on("error", (error) => {
      console.warn("[db] idle client dropped (the pool will replace it):", error.message);
    });
    pool = opened;
    db = undefined;
  }
  return pool;
}

/**
 * Close the pool and FORGET it, so the next `getPool()` opens a live one rather than handing
 * back an ended pool that rejects every query. Used by tests tearing a scratch database down:
 * ending before the drop is what keeps a forced `DROP DATABASE` from terminating a connection
 * this process still holds.
 */
export async function endPool(): Promise<void> {
  const open = pool;
  pool = undefined;
  db = undefined;
  await open?.end();
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
