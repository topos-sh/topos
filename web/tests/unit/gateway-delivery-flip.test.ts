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
 * HOW DELIVERY ROUTES A CONNECTED SERVER once a gateway is deployed — the three layers, through
 * the real delivery query against a real scratch Postgres.
 *
 * The env is armed for this whole file (the gateway's public base is a deployment fact, not a
 * per-request one). What must hold:
 *  - with NO mandate (the Auto state), an addressable server is routed through the gateway only
 *    once a sign-in stands there — the caller's own or the workspace's — or when it needs none
 *    (`auth_mode = 'none'`); until then the machine keeps the server's own address, so turning
 *    the gateway on never breaks a working server. Unestablished auth counts as needing one.
 *  - a member's opt-out row routes THEIR delivery direct, and only under Auto;
 *  - the connection's 'direct' mandate routes everyone direct; its 'required' mandate routes
 *    everyone through the gateway with no sign-in gating. (The other half of `required` — a
 *    machine that cannot be handed an address receives NO ROW, never a quiet direct fallback —
 *    needs a deployment running NO gateway, so it is proved in mcp-delivery.test.ts, which runs
 *    with the variable unset; this file's env is armed for every case in it.)
 *  - the workspace switch off routes everything direct, mandates included;
 *  - a PACKAGE-ONLY server is never rewritten and never withheld — there is no address to route;
 *  - the address names the CALLING session, and a read with no session behind it (a web page's
 *    own feed view) is never rewritten and never withheld.
 *
 * The unflipped deployment is covered where it belongs — mcp-delivery.test.ts runs with no
 * gateway configured and asserts the stored documents come back verbatim.
 */

let db: ScratchDb;
let ws = "";

const MEMBER = "u_mem";
const OTHER = "u_other";
const BASE = "https://gw.example.com";
const UPSTREAM = "https://upstream.example.com/mcp";

async function lane() {
  return await import("@/lib/db/queries.lane.server");
}

async function seedServer(
  id: string,
  name: string,
  document: Record<string, unknown>,
  authMode: string | null,
) {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ($1, NULL, $2, $2, $3, 'active')`,
    [id, name, authMode],
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
      addressed ? UPSTREAM : null,
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

function remoteServer(name: string): Record<string, unknown> {
  return {
    name,
    version: "1.0.0",
    remotes: [{ type: "streamable-http", url: UPSTREAM }],
  };
}

async function credential(id: string, serverId: string, userId: string | null) {
  await db.q(
    `INSERT INTO gateway.credential (id, workspace_id, server_id, user_id, auth_kind)
     VALUES ($1, $2, $3, $4, 'oauth')`,
    [id, ws, serverId, userId],
  );
}

async function delivered(session = "sn_laptop") {
  const body = await (await lane()).deliveryFor(asSession(ws, MEMBER, session));
  return body.mcp_servers;
}

function urlOf(rows: Awaited<ReturnType<typeof delivered>>, skillId: string): string | undefined {
  const row = rows.find((r) => r.skill_id === skillId);
  return (row?.document.remotes as { url: string }[] | undefined)?.[0]?.url;
}

beforeAll(async () => {
  db = await createScratchDb("web_gateway_flip", { GATEWAY_PUBLIC_URL: BASE });
  ws = await bootWorkspace();
  // The slice of the gateway's own schema this tier reads (mirroring the gateway's migration):
  // on a deployment whose delivery hands out gateway addresses, the gateway has migrated it.
  await db.q(`CREATE SCHEMA IF NOT EXISTS gateway`);
  await db.q(
    `CREATE TABLE gateway.credential (
       id text PRIMARY KEY,
       workspace_id text NOT NULL,
       server_id text NOT NULL,
       user_id text,
       auth_kind text NOT NULL,
       created_at timestamptz NOT NULL DEFAULT now(),
       last_refreshed_at timestamptz
     )`,
  );
  await seedUser(db, MEMBER, "Mo Member", "mo@example.com");
  await seatUser(db, ws, MEMBER, "member");
  await seedUser(db, OTHER, "Oa Other", "oa@example.com");
  await seatUser(db, ws, OTHER, "member");
  await seedSession(db, "sn_laptop", ws, MEMBER, "active", "Mo's laptop");
  await seedSession(db, "sn_desktop", ws, MEMBER, "active", "Mo's desktop");

  // Auto, sign-in required, NO sign-in stands.
  await seedServer("mcps_oauth", "com.example/oauth", remoteServer("com.example/oauth"), "oauth");
  await connect("b_oauth", "oauth-server", "mcps_oauth");
  // Auto, the caller's OWN sign-in stands.
  await seedServer("mcps_cred", "com.example/cred", remoteServer("com.example/cred"), "oauth");
  await connect("b_cred", "cred-server", "mcps_cred");
  await credential("cred_mine", "mcps_cred", MEMBER);
  // Auto, only the WORKSPACE sign-in stands.
  await seedServer(
    "mcps_wscred",
    "com.example/wscred",
    remoteServer("com.example/wscred"),
    "manual",
  );
  await connect("b_wscred", "wscred-server", "mcps_wscred");
  await credential("cred_ws", "mcps_wscred", null);
  // Auto, only ANOTHER member's sign-in stands — not this caller's to ride on.
  await seedServer(
    "mcps_theirs",
    "com.example/theirs",
    remoteServer("com.example/theirs"),
    "oauth",
  );
  await connect("b_theirs", "theirs-server", "mcps_theirs");
  await credential("cred_theirs", "mcps_theirs", OTHER);
  // Auto, no sign-in NEEDED.
  await seedServer("mcps_none", "com.example/open", remoteServer("com.example/open"), "none");
  await connect("b_none", "open-server", "mcps_none");
  // Auto, auth nobody established — treated as needing a sign-in.
  await seedServer("mcps_unest", "com.example/unest", remoteServer("com.example/unest"), null);
  await connect("b_unest", "unest-server", "mcps_unest");
  // Auto, no sign-in needed, but THIS member chose direct.
  await seedServer("mcps_opt", "com.example/opted", remoteServer("com.example/opted"), "none");
  await connect("b_opt", "opted-server", "mcps_opt");
  await db.q(
    `INSERT INTO web.mcp_gateway_optout (workspace_id, server_id, user_id) VALUES ($1, $2, $3)`,
    [ws, "mcps_opt", MEMBER],
  );
  // The 'direct' mandate, over a server the gateway could otherwise take.
  await seedServer("mcps_dir", "com.example/direct", remoteServer("com.example/direct"), "none");
  await connect("b_dir", "direct-server", "mcps_dir");
  await db.q(`UPDATE web.bundle_mcp SET gateway_policy = 'direct' WHERE server_id = 'mcps_dir'`);
  // The 'required' mandate, NO sign-in standing.
  await seedServer(
    "mcps_req",
    "com.example/required",
    remoteServer("com.example/required"),
    "oauth",
  );
  await connect("b_req", "required-server", "mcps_req");
  await db.q(`UPDATE web.bundle_mcp SET gateway_policy = 'required' WHERE server_id = 'mcps_req'`);
  // Package-only servers — one Auto, one under a (meaningless) 'required'.
  await seedServer(
    "mcps_pkg",
    "com.example/packaged",
    {
      name: "com.example/packaged",
      version: "1.0.0",
      packages: [{ registryType: "npm", identifier: "@example/pkg", version: "1.0.0" }],
    },
    "none",
  );
  await connect("b_pkg", "packaged-server", "mcps_pkg");
  await seedServer(
    "mcps_reqpkg",
    "com.example/reqpkg",
    {
      name: "com.example/reqpkg",
      version: "1.0.0",
      packages: [{ registryType: "npm", identifier: "@example/reqpkg", version: "1.0.0" }],
    },
    "none",
  );
  await connect("b_reqpkg", "reqpkg-server", "mcps_reqpkg");
  await db.q(
    `UPDATE web.bundle_mcp SET gateway_policy = 'required' WHERE server_id = 'mcps_reqpkg'`,
  );
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("routing under Auto (no mandate)", () => {
  it("keeps a sign-in server on its own address until a sign-in stands", async () => {
    const rows = await delivered();
    expect(urlOf(rows, "b_oauth")).toBe(UPSTREAM);
    const meta = rows.find((r) => r.skill_id === "b_oauth")?.document._meta as
      | Record<string, unknown>
      | undefined;
    expect(meta?.["sh.topos/gateway"]).toBeUndefined();
  });

  it("routes through the gateway once the caller's own sign-in stands", async () => {
    const rows = await delivered();
    expect(urlOf(rows, "b_cred")).toBe(`${BASE}/sn_laptop/mcps_cred`);
    const meta = rows.find((r) => r.skill_id === "b_cred")?.document._meta as Record<
      string,
      unknown
    >;
    expect(meta["sh.topos/gateway"]).toBe(true);
    // The tier flips WITH the address: through the gateway, this machine has no sign-in to run.
    expect(meta["sh.topos/auth"]).toBe("none");
  });

  it("routes through the gateway on the workspace sign-in alone", async () => {
    expect(urlOf(await delivered(), "b_wscred")).toBe(`${BASE}/sn_laptop/mcps_wscred`);
  });

  it("does not route on a sign-in that is somebody else's alone", async () => {
    expect(urlOf(await delivered(), "b_theirs")).toBe(UPSTREAM);
  });

  it("routes a server that needs no sign-in immediately", async () => {
    expect(urlOf(await delivered(), "b_none")).toBe(`${BASE}/sn_laptop/mcps_none`);
  });

  it("treats unestablished auth as needing a sign-in", async () => {
    expect(urlOf(await delivered(), "b_unest")).toBe(UPSTREAM);
  });

  it("honors the member's own opt-out", async () => {
    expect(urlOf(await delivered(), "b_opt")).toBe(UPSTREAM);
  });

  it("gives a second machine its own address", async () => {
    expect(urlOf(await delivered("sn_desktop"), "b_cred")).toBe(`${BASE}/sn_desktop/mcps_cred`);
  });

  it("follows a revoked sign-in back to the server's own address", async () => {
    await db.q(`DELETE FROM gateway.credential WHERE id = 'cred_mine'`);
    expect(urlOf(await delivered(), "b_cred")).toBe(UPSTREAM);
    await credential("cred_mine", "mcps_cred", MEMBER);
    expect(urlOf(await delivered(), "b_cred")).toBe(`${BASE}/sn_laptop/mcps_cred`);
  });
});

describe("routing under a mandate", () => {
  it("'direct' routes everyone direct, sign-in or not", async () => {
    expect(urlOf(await delivered(), "b_dir")).toBe(UPSTREAM);
  });

  it("'required' routes through the gateway with no sign-in standing", async () => {
    expect(urlOf(await delivered(), "b_req")).toBe(`${BASE}/sn_laptop/mcps_req`);
  });

  it("'required' overrides a member's opt-out", async () => {
    await db.q(
      `INSERT INTO web.mcp_gateway_optout (workspace_id, server_id, user_id) VALUES ($1, $2, $3)`,
      [ws, "mcps_req", MEMBER],
    );
    expect(urlOf(await delivered(), "b_req")).toBe(`${BASE}/sn_laptop/mcps_req`);
  });

  it("'required' never withholds a package-only server — nothing to route", async () => {
    const rows = await delivered();
    expect(rows.find((r) => r.skill_id === "b_reqpkg")?.document.packages).toBeDefined();
  });
});

describe("reads with no machine behind them", () => {
  it("rewrites nothing and withholds nothing", async () => {
    const body = await (await lane()).deliveryFor(asMember(ws, MEMBER));
    expect(urlOf(body.mcp_servers, "b_cred")).toBe(UPSTREAM);
    expect(urlOf(body.mcp_servers, "b_req")).toBe(UPSTREAM);
    const meta = body.mcp_servers.find((r) => r.skill_id === "b_cred")?.document._meta as
      | Record<string, unknown>
      | undefined;
    expect(meta?.["sh.topos/gateway"]).toBeUndefined();
  });
});

describe("packages stay local", () => {
  it("hands a package-only server over exactly as stored", async () => {
    const rows = await delivered();
    expect(rows.find((r) => r.skill_id === "b_pkg")?.document).toEqual({
      name: "com.example/packaged",
      version: "1.0.0",
      packages: [{ registryType: "npm", identifier: "@example/pkg", version: "1.0.0" }],
    });
  });
});

// LAST: flips the one workspace row under every other case, and puts it back.
describe("the workspace switch", () => {
  it("off routes everything direct — mandates and sign-ins included", async () => {
    await db.q(`UPDATE web.workspace SET mcp_gateway = 'off' WHERE id = $1`, [ws]);
    try {
      const rows = await delivered();
      expect(urlOf(rows, "b_cred")).toBe(UPSTREAM);
      expect(urlOf(rows, "b_none")).toBe(UPSTREAM);
      // The 'required' row is DELIVERED direct, not withheld: the switch overrides the mandate.
      expect(urlOf(rows, "b_req")).toBe(UPSTREAM);
    } finally {
      await db.q(`UPDATE web.workspace SET mcp_gateway = 'on' WHERE id = $1`, [ws]);
    }
  });
});
