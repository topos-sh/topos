import { readFileSync } from "node:fs";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { join, resolve } from "node:path";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { canonicalServerJson } from "@/lib/mcp/fetch.server";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seedBundle,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * THE ONE-SHOT BACKFILL, against a real database and a stub vault.
 *
 * Every MCP bundle that predates the catalog needs the row saying which server it names, and that
 * name lives inside bytes only the vault holds — so the step runs in this tier, and this suite
 * gives it exactly the vault it would meet: an in-process stub the ONE custody transport is
 * re-pointed at (`PLANE_INTERNAL_URL`), serving real version listings and real document bytes.
 *
 * What is proved is the routing. A bundle whose document names a catalog server connects to THAT
 * server and starts receiving what staff publish. A bundle that matches nothing gets its workspace
 * a private server holding its own document — nothing exported, nothing shared. A document that
 * cannot be read is named rather than skipped, and left for the next boot. And running twice
 * changes nothing, which is what makes a boot step safe to have.
 *
 * The migration that retires the review path for the kind is driven here too, straight from the
 * committed file: an open proposal against an MCP bundle goes, everything else stays.
 */

const WEB_ROOT = resolve(__dirname, "..", "..");

/** `${bundle}/${version}` → the file listing the stub serves for that version. */
const listings = new Map<string, { path: string; mode: string; object_id: string }[]>();
/** object_id → the bytes behind it. */
const objects = new Map<string, Buffer>();

function objectIdFor(seed: string): string {
  return `${seed.replaceAll("_", "")}0`.padEnd(64, "b").slice(0, 64);
}

/** Hand the stub vault one bundle's published `server.json`. */
function publishToVault(bundleId: string, document: Record<string, unknown>): void {
  const objectId = objectIdFor(bundleId);
  listings.set(`${bundleId}/${versionIdFor(bundleId)}`, [
    { path: "server.json", mode: "100644", object_id: objectId },
  ]);
  objects.set(objectId, Buffer.from(canonicalServerJson(document)));
}

function serverDocument(name: string, version: string): Record<string, unknown> {
  return {
    name,
    description: "A server the workspace was already publishing.",
    version,
    remotes: [{ type: "streamable-http", url: "https://mcp.example.com/mcp" }],
  };
}

let db: ScratchDb;
let wsId = "";
let stub: Server;

beforeAll(async () => {
  stub = createServer((request, response) => {
    const url = request.url ?? "";
    const version = url.match(
      /^\/internal\/v1\/workspaces\/[^/]+\/bundles\/([^/]+)\/versions\/([^/]+)$/,
    );
    if (request.method === "GET" && version?.[1] && version[2]) {
      const files = listings.get(`${version[1]}/${version[2]}`);
      if (files) {
        response.writeHead(200, { "content-type": "application/json" });
        response.end(
          JSON.stringify({
            version_id: version[2],
            parents: [],
            author: "seed",
            message: "seed",
            bundle_digest: "d".repeat(64),
            created_at_ms: 1_700_000_000_000,
            files,
          }),
        );
        return;
      }
    }
    const object = url.match(
      /^\/internal\/v1\/workspaces\/[^/]+\/bundles\/[^/]+\/objects\/([^/]+)$/,
    );
    if (request.method === "GET" && object?.[1]) {
      const bytes = objects.get(object[1]);
      if (bytes) {
        response.writeHead(200, {
          "content-type": "application/octet-stream",
          "content-length": String(bytes.length),
        });
        response.end(bytes);
        return;
      }
    }
    response.writeHead(404, { "content-type": "application/json" });
    response.end(JSON.stringify({ code: "NOT_FOUND" }));
  });
  await new Promise<void>((resolve) => stub.listen(0, "127.0.0.1", resolve));
  const { port } = stub.address() as AddressInfo;

  db = await createScratchDb("web_mcp_backfill", {
    PLANE_INTERNAL_URL: `http://127.0.0.1:${port}`,
  });
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");

  // Already publishing a server the catalog knows.
  await seedBundle(db, wsId, "s_github", "github", { kind: "mcp" });
  publishToVault("s_github", serverDocument("com.github/mcp", "1.0.0"));
  // A second bundle serving the SAME server — one workspace, one connection, so this one is the
  // odd bundle out. Archived, which is also why the live one should be the one that gets it.
  await seedBundle(db, wsId, "s_github_old", "github-old", { kind: "mcp", status: "archived" });
  publishToVault("s_github_old", serverDocument("com.github/mcp", "0.9.0"));
  // A server nobody else has — this one becomes the workspace's own.
  await seedBundle(db, wsId, "s_internal", "internal", { kind: "mcp" });
  publishToVault("s_internal", serverDocument("com.example/internal", "3.1.4"));
  // Published, but the vault has nothing to say about it.
  await seedBundle(db, wsId, "s_gone", "gone", { kind: "mcp" });
  // A skill, which this step has no business touching.
  await seedBundle(db, wsId, "s_skill", "a-skill");
}, 90000);

afterAll(async () => {
  await new Promise<void>((done, fail) => stub.close((e) => (e ? fail(e) : done())));
  await db.drop();
});

describe("the connection backfill", () => {
  it("routes every bundle to the server it names, and says what it could not do", async () => {
    const { backfillMcpConnections } = await import("@/lib/db/mcp-backfill.server");
    const report = await backfillMcpConnections();

    expect(report.connected).toBe(1);
    expect(report.privatized).toBe(1);
    expect(report.unreadable).toEqual([`${wsId}/gone`]);
    expect(report.contested).toEqual([`${wsId}/github-old`]);
    expect(report.refused).toEqual([]);
    expect(report.deferred).toEqual([]);
  }, 30_000);

  it("the catalog's own server is what the matching bundle connects to", async () => {
    const rows = await db.q<{ server_id: string; registry_name: string; workspace_id: string }>(
      `SELECT m.server_id, s.registry_name, s.workspace_id
       FROM web.bundle_mcp m JOIN web.mcp_server s ON s.id = m.server_id
       WHERE m.bundle_id = 's_github'`,
    );
    expect(rows[0]?.registry_name).toBe("com.github/mcp");
    // The GLOBAL row: null workspace. The bundle now receives what staff publish.
    expect(rows[0]?.workspace_id).toBeNull();
  });

  it("an unmatched document becomes the workspace's own server, holding its own bytes", async () => {
    const rows = await db.q<{
      workspace_id: string;
      registry_name: string;
      auth_mode: string | null;
      status: string;
      document: Record<string, unknown>;
      revision_status: string;
      source: string;
      published_by: string | null;
    }>(
      `SELECT s.workspace_id, s.registry_name, s.auth_mode, s.status,
              r.document, r.status AS revision_status, r.source, r.published_by
       FROM web.bundle_mcp m
       JOIN web.mcp_server s ON s.id = m.server_id
       JOIN web.mcp_server_revision r ON r.id = s.current_revision_id
       WHERE m.bundle_id = 's_internal'`,
    );
    const row = rows[0];
    expect(row?.workspace_id).toBe(wsId);
    expect(row?.registry_name).toBe("com.example/internal");
    // Nothing was ever established about this server's sign-in, and the migration does not guess.
    expect(row?.auth_mode).toBeNull();
    expect(row?.status).toBe("active");
    expect(row?.document.version).toBe("3.1.4");
    expect(row?.revision_status).toBe("published");
    expect(row?.source).toBe("owner");
    expect(row?.published_by).toBe("system");
  });

  it("leaves the unreadable one and the skill alone", async () => {
    const rows = await db.q<{ bundle_id: string }>(
      `SELECT bundle_id FROM web.bundle_mcp ORDER BY bundle_id`,
    );
    expect(rows.map((r) => r.bundle_id)).toEqual(["s_github", "s_internal"]);
  });

  it("a second pass changes nothing — the work list is what is missing, not what exists", async () => {
    const { backfillMcpConnections } = await import("@/lib/db/mcp-backfill.server");
    const before = await db.q<{ n: string }>(`SELECT count(*) AS n FROM web.mcp_server`);
    const report = await backfillMcpConnections();
    const after = await db.q<{ n: string }>(`SELECT count(*) AS n FROM web.mcp_server`);
    expect(report.connected).toBe(0);
    expect(report.privatized).toBe(0);
    // The two it could not place are still named, so nothing is ever silently forgotten.
    expect(report.unreadable).toEqual([`${wsId}/gone`]);
    expect(report.contested).toEqual([`${wsId}/github-old`]);
    expect(after[0]?.n).toBe(before[0]?.n);
  }, 30_000);

  it("the boot wrapper never throws, whatever it met", async () => {
    const { backfillMcpConnectionsAtBoot } = await import("@/lib/db/mcp-backfill.server");
    await expect(backfillMcpConnectionsAtBoot()).resolves.toBeUndefined();
  }, 30_000);

  it("a document this build cannot store leaves no half-built server behind", async () => {
    const { backfillMcpConnections } = await import("@/lib/db/mcp-backfill.server");
    await seedBundle(db, wsId, "s_refused", "refused", { kind: "mcp" });
    // Readable, and still not storable: it declares a schema this build has never been taught.
    // Fail closed — record nothing, name it, and let somebody republish it.
    publishToVault("s_refused", {
      ...serverDocument("com.example/from-the-future", "1.0.0"),
      $schema: "https://static.modelcontextprotocol.io/schemas/2099-01-01/server.schema.json",
    });
    const report = await backfillMcpConnections();
    expect(report.privatized).toBe(0);
    expect(report.refused).toEqual([`${wsId}/refused`]);
    const servers = await db.q(
      `SELECT id FROM web.mcp_server WHERE registry_name = 'com.example/from-the-future'`,
    );
    expect(servers).toEqual([]);
  }, 30_000);
});

describe("the migration that retires the review path for the kind", () => {
  it("deletes open proposals against MCP bundles, and nothing else", async () => {
    await db.q(
      `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, status)
       VALUES ('p_mcp_open', $1, 's_github', $2, 'open')`,
      [wsId, versionIdFor("s_github")],
    );
    await db.q(
      `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, status,
                                 resolved_at)
       VALUES ('p_mcp_done', $1, 's_github', $2, 'approved', now())`,
      [wsId, versionIdFor("s_internal")],
    );
    await db.q(
      `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, status)
       VALUES ('p_skill_open', $1, 's_skill', $2, 'open')`,
      [wsId, versionIdFor("s_skill")],
    );
    const statement = readFileSync(
      join(WEB_ROOT, "drizzle", "0019_mcp-proposals-retired.sql"),
      "utf8",
    );
    await db.q(statement);
    const rows = await db.q<{ id: string }>(`SELECT id FROM web.proposal ORDER BY id`);
    expect(rows.map((r) => r.id)).toEqual(["p_mcp_done", "p_skill_open"]);
  });
});
