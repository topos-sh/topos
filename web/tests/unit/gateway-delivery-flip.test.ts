import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { mcpRevisionId } from "../helpers/mcp-ids";
import {
  asMember,
  asSession,
  assignBundleRow,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * WHAT A MACHINE RECEIVES ONCE A GATEWAY IS DEPLOYED — the flip, through the real delivery query
 * against a real scratch Postgres.
 *
 * The env is armed for this whole file (the gateway's public base is a deployment fact, not a
 * per-request one), which leaves three things to hold still:
 *  - an ADDRESSABLE server comes back pointed at `<base>/<session id>/<server id>`, marked in
 *    `_meta`, with everything else about the document intact;
 *  - a PACKAGE-ONLY server comes back exactly as stored — there is no address to redirect;
 *  - the address names the CALLING session, so two machines never receive the same one, and a
 *    read with no session behind it (a web page's own feed view) is not rewritten at all.
 *
 * The unflipped deployment is covered where it belongs — mcp-delivery.test.ts runs with no gateway
 * configured and asserts the stored documents come back verbatim.
 */

let db: ScratchDb;
let ws = "";

const MEMBER = "u_mem";
const BASE = "https://gw.example.com";

async function lane() {
  return await import("@/lib/db/queries.lane.server");
}

async function seedServer(id: string, name: string, document: Record<string, unknown>) {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ($1, NULL, $2, $2, 'oauth', 'active')`,
    [id, name],
  );
  const revision = mcpRevisionId(id);
  const addressed = Array.isArray(document.remotes);
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, upstream_version, document, transport, url, published_at, published_by)
     VALUES ($1, $2, 1, '1.0.0', $3::jsonb, $4, $5, now(), 'Staff')`,
    [
      revision,
      id,
      JSON.stringify(document),
      addressed ? "streamable-http" : null,
      addressed ? "https://upstream.example.com/mcp" : null,
    ],
  );
  await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [id, revision]);
}

async function connect(bundleId: string, bundleName: string, serverId: string) {
  await seedBundle(db, ws, bundleId, bundleName, { kind: "mcp", withPointer: false });
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id) VALUES ($1, $2, $3)`,
    [bundleId, ws, serverId],
  );
  await assignBundleRow(db, ws, bundleId, MEMBER);
}

beforeAll(async () => {
  db = await createScratchDb("web_gateway_flip", { GATEWAY_PUBLIC_URL: BASE });
  ws = await bootWorkspace();
  await seedUser(db, MEMBER, "Mo Member", "mo@example.com");
  await seatUser(db, ws, MEMBER, "member");
  await seedSession(db, "cs_laptop", ws, MEMBER, "active", "Mo's laptop");
  await seedSession(db, "cs_desktop", ws, MEMBER, "active", "Mo's desktop");

  await seedServer("mcps_remote", "com.example/remote", {
    name: "com.example/remote",
    version: "1.0.0",
    remotes: [{ type: "streamable-http", url: "https://upstream.example.com/mcp" }],
    _meta: { "sh.topos/auth": "oauth" },
  });
  await connect("b_remote", "remote-server", "mcps_remote");

  await seedServer("mcps_pkg", "com.example/packaged", {
    name: "com.example/packaged",
    version: "1.0.0",
    packages: [{ registryType: "npm", identifier: "@example/pkg", version: "1.0.0" }],
  });
  await connect("b_pkg", "packaged-server", "mcps_pkg");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("delivery under a deployed gateway", () => {
  it("points an addressable server at the gateway, naming the calling session", async () => {
    const body = await (await lane()).deliveryFor(asSession(ws, MEMBER, "cs_laptop"));
    const served = body.mcp_servers.find((row) => row.skill_id === "b_remote");
    expect(served?.document.remotes).toEqual([
      { type: "streamable-http", url: `${BASE}/cs_laptop/mcps_remote` },
    ]);
    const meta = served?.document._meta as Record<string, unknown>;
    expect(meta["sh.topos/gateway"]).toBe(true);
    // The tier flips WITH the address: through the gateway, this machine has no sign-in to run.
    expect(meta["sh.topos/auth"]).toBe("none");
    expect(served?.document.name).toBe("com.example/remote");
    expect(served?.revision_id).toBe(mcpRevisionId("mcps_remote"));
  });

  it("gives a second machine its own address", async () => {
    const body = await (await lane()).deliveryFor(asSession(ws, MEMBER, "cs_desktop"));
    const served = body.mcp_servers.find((row) => row.skill_id === "b_remote");
    expect((served?.document.remotes as { url: string }[])[0]?.url).toBe(
      `${BASE}/cs_desktop/mcps_remote`,
    );
  });

  it("hands a package-only server over exactly as stored", async () => {
    const body = await (await lane()).deliveryFor(asSession(ws, MEMBER, "cs_laptop"));
    const served = body.mcp_servers.find((row) => row.skill_id === "b_pkg");
    expect(served?.document).toEqual({
      name: "com.example/packaged",
      version: "1.0.0",
      packages: [{ registryType: "npm", identifier: "@example/pkg", version: "1.0.0" }],
    });
    expect(served?.document._meta).toBeUndefined();
  });

  it("rewrites nothing for a read with no machine behind it", async () => {
    const body = await (await lane()).deliveryFor(asMember(ws, MEMBER));
    const served = body.mcp_servers.find((row) => row.skill_id === "b_remote");
    expect(served?.document.remotes).toEqual([
      { type: "streamable-http", url: "https://upstream.example.com/mcp" },
    ]);
    expect((served?.document._meta as Record<string, unknown>)["sh.topos/gateway"]).toBeUndefined();
  });
});
