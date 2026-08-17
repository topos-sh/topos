import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seedBundle,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * A WEDGED VAULT MUST NOT KEEP THE APP FROM BOOTING.
 *
 * The connection backfill runs at module load, BEFORE the server binds its port — deliberately,
 * because a bundle without its connection row delivers nothing and the name that decides which
 * server it means lives in the vault's bytes. That ordering is only safe if the backfill is
 * guaranteed to END. A vault that is DOWN ends it (the connection is refused); a vault that
 * accepts the TCP connection and then never answers does not — `fetch` carries no timeout of its
 * own, so the await never settles, the listener never binds, the container healthcheck never
 * passes, and the orchestrator restarts into the same stall.
 *
 * The stub below is exactly that vault: it accepts and says nothing, forever. The backfill must
 * give up on its own and let boot continue, leaving the work for the next boot.
 */

let db: ScratchDb;
let deaf: Server;

beforeAll(async () => {
  // Accepts the connection, reads the request, and never responds.
  deaf = createServer(() => {});
  await new Promise<void>((resolve) => deaf.listen(0, "127.0.0.1", resolve));
  const { port } = deaf.address() as AddressInfo;
  db = await createScratchDb("web_boot_budget", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: `http://127.0.0.1:${port}`,
  });
  const wsId = await bootWorkspace();
  // One published MCP bundle with no connection yet — the work the backfill would try to do.
  await seedBundle(db, wsId, "s_wedged", "wedged", {
    kind: "mcp",
    versionId: versionIdFor("s_wedged"),
  });
}, 60000);

afterAll(async () => {
  await db.drop();
  // The abandoned reads still hold sockets open — this stub answers nothing, so they never end
  // on their own and `close()` would wait for them forever. Drop them, then close.
  deaf.closeAllConnections();
  await new Promise<void>((resolve) => deaf.close(() => resolve()));
}, 30_000);

describe("the boot backfill against a vault that accepts and never answers", () => {
  it("gives up inside its budget instead of hanging boot", async () => {
    const { backfillMcpConnections } = await import("@/lib/db/mcp-backfill.server");
    const started = Date.now();
    const report = await backfillMcpConnections();
    const elapsed = Date.now() - started;

    // The whole point: it RETURNS. Generously bounded — the assertion is "bounded", not a
    // stopwatch reading, so a slow machine cannot make this flaky.
    expect(elapsed).toBeLessThan(20_000);
    // Nothing was connected, and the bundle it could not read is NAMED rather than dropped — the
    // next boot picks it up, because unreachable now is not unreachable forever.
    expect(report.connected + report.privatized).toBe(0);
    expect([...report.unreadable, ...report.deferred].join(" ")).toContain("wedged");
    // No row was invented for a document nobody could read.
    expect(await db.q(`SELECT 1 FROM web.bundle_mcp WHERE bundle_id = $1`, ["s_wedged"])).toEqual(
      [],
    );
  }, 40_000);

  it("the boot wrapper swallows it too, so module load always completes", async () => {
    const { backfillMcpConnectionsAtBoot } = await import("@/lib/db/mcp-backfill.server");
    await expect(backfillMcpConnectionsAtBoot()).resolves.toBeUndefined();
  }, 40_000);
});
