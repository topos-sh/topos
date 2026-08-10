import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { createScratchDb, type ScratchDb } from "./helpers/scratch-db";

/**
 * A SUITE THAT PASSES MUST NOT THEN FAIL — the teardown race, pinned.
 *
 * Scratch databases are torn down with `DROP DATABASE … WITH (FORCE)`, which terminates every
 * backend still attached (`57P01`, "terminating connection due to administrator command"). Any
 * connection this process is still holding therefore errors AFTER the tests that used it have
 * finished, and pg reports an IDLE client's error by re-emitting it on the pool. An EventEmitter
 * with no `error` listener THROWS on emit — so that terminated connection became an unhandled
 * error attributed to nothing, surfacing as a run that passed and then reported a failure.
 *
 * Two guards, both proven below rather than described:
 *
 *  1. the pool carries an `error` listener, so a terminated idle client is survivable, and
 *  2. `endPool()` closes AND forgets the pool, so the ordinary teardown path has no connection
 *     left for the drop to terminate — and a suite that keeps running afterwards gets a live
 *     pool rather than an ended one.
 *
 * The SECOND test deliberately does the WRONG order — dropping while the pool still holds an
 * idle client — and adds no listener of its own, so the app's is the only one there. That makes
 * it a real reproduction: revert the listener and this file fails with exactly the reported
 * error; keep it and the file simply passes.
 */

let db: ScratchDb;

beforeAll(async () => {
  db = await createScratchDb("web_pool_teardown");
}, 60_000);

afterAll(async () => {
  await db.drop();
});

/** The admin connection the forced drop is issued from — never the pool under test. */
async function adminExec(sql: string): Promise<void> {
  const admin = new Client({
    connectionString:
      process.env.TEST_DATABASE_URL ?? "postgresql://postgres:identity2@localhost:5443/postgres",
  });
  await admin.connect();
  try {
    await admin.query(sql);
  } finally {
    await admin.end();
  }
}

describe("a force-dropped database under a live pool", () => {
  it("endPool forgets the pool, so the next getPool opens a live one", async () => {
    const { getPool, endPool } = await import("@/lib/db/index.server");
    const first = getPool();
    await endPool();
    const second = getPool();
    // A new object, not the ended one handed back — an ended pool rejects every query, which is
    // how a "teardown" in the middle of a suite used to poison everything after it.
    expect(second).not.toBe(first);
    expect((await second.query("SELECT 1 AS one")).rows[0]).toEqual({ one: 1 });
  });
  it("terminates the idle client without taking the process down", async () => {
    const { endPool } = await import("@/lib/db/index.server");

    // A second scratch database, reached through a pool of its own — dropped out from under that
    // pool ON PURPOSE. Using the suite's own database here would leave the rest of the file with
    // nothing to talk to.
    const victim = `web_pool_victim_${Date.now()}`;
    await adminExec(`CREATE DATABASE ${victim}`);

    const url = new URL(
      process.env.TEST_DATABASE_URL ?? "postgresql://postgres:identity2@localhost:5443/postgres",
    );
    url.pathname = `/${victim}`;
    // Repointing the app at it needs a fresh module graph: the env parse is memoized, so a
    // reassigned DATABASE_URL is only read by a module instance that has not parsed one yet.
    const { installTestEnv } = await import("./helpers/test-env");
    await endPool();
    vi.resetModules();
    installTestEnv({ DATABASE_URL: url.toString() });
    const { getPool: victimPool, endPool: endVictimPool } = await import("@/lib/db/index.server");

    // A query, then nothing: the client goes back to the pool and sits there IDLE, which is the
    // only state in which pg reports a connection error on the pool rather than to a caller.
    const pool = victimPool();
    expect((await pool.query("SELECT 1 AS one")).rows[0]).toEqual({ one: 1 });

    // NO listener is attached here on purpose. One added by the test would satisfy the emit by
    // itself and prove nothing about the app; the app's own listener has to be what catches this.
    await adminExec(`DROP DATABASE IF EXISTS ${victim} WITH (FORCE)`);
    // The termination arrives on the socket asynchronously; give it a turn to land.
    await new Promise((resolve) => setTimeout(resolve, 250));

    // REACHING THIS LINE IS THE ASSERTION: the terminated idle client was reported to a listener
    // instead of thrown into whatever was running.
    expect(pool.totalCount).toBeGreaterThanOrEqual(0);

    // Ending a pool whose database is gone must not throw either — that is the path `drop()`
    // takes in every other suite's `afterAll`.
    await endVictimPool();
  }, 30_000);
});
