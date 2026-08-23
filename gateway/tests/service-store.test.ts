import { randomBytes } from "node:crypto";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { EnvelopeCrypto, FileMasterKey } from "../service/crypto";
import { PgStore, secretHeaderFromDocument } from "../service/store";
import { BufferedUsageSink } from "../service/usage";
import {
  bearerSha256Hex,
  createServiceDb,
  seedConnectedServer,
  seedIdentity,
  seedSession,
  setToolPolicy,
  type ServiceDb,
} from "./helpers/service-db";

const TOPOS_PUBLIC_URL = "https://team.example.com";

let db: ServiceDb;
let pool: pg.Pool;
let store: PgStore;

beforeAll(async () => {
  db = await createServiceDb();
  pool = new pg.Pool({ connectionString: db.gatewayUrl, max: 5 });
  store = new PgStore(pool, new EnvelopeCrypto(new FileMasterKey(randomBytes(32))), TOPOS_PUBLIC_URL, () => {});
  await seedIdentity(db, "ws1", "u1");
  await seedIdentity(db, "ws1", "u2");
}, 120_000);

afterAll(async () => {
  await pool?.end();
  await db?.drop();
});

describe("sessionByTokenSha256", () => {
  it("resolves an active session by its bearer's sha256", async () => {
    await seedSession(db, "sn_alpha", "ws1", "u1", "bearer-alpha");
    const ref = await store.sessionByTokenSha256(bearerSha256Hex("bearer-alpha"));
    expect(ref).toEqual({
      sessionId: "sn_alpha",
      workspaceId: "ws1",
      userId: "u1",
      displayName: "sn_alpha",
    });
  });

  it("ignores a pending session", async () => {
    await seedSession(db, "sn_pending", "ws1", "u1", "bearer-pending", "pending");
    expect(await store.sessionByTokenSha256(bearerSha256Hex("bearer-pending"))).toBeNull();
  });

  it("answers null for an unknown hash and for a malformed one", async () => {
    expect(await store.sessionByTokenSha256(bearerSha256Hex("no-such-bearer"))).toBeNull();
    expect(await store.sessionByTokenSha256("not-hex")).toBeNull();
  });

  it("ignores an active session past the workspace's session_max_age_ms", async () => {
    // A workspace that expires sessions after an hour, and a still-'active' row created two hours
    // ago: the web lane already rejects it, and the gateway must too — expiry never deletes the
    // row, so a status-only check would let a stale bearer call tools forever.
    await seedIdentity(db, "wsexp", "u1");
    await db.q("UPDATE web.workspace SET session_max_age_ms = 3600000 WHERE id = 'wsexp'");
    await seedSession(db, "sn_stale", "wsexp", "u1", "bearer-stale");
    await db.q(
      "UPDATE web.cli_session SET created_at = now() - interval '2 hours' WHERE id = 'sn_stale'",
    );
    expect(await store.sessionByTokenSha256(bearerSha256Hex("bearer-stale"))).toBeNull();

    // A fresh session in the same workspace still resolves.
    await seedSession(db, "sn_fresh", "wsexp", "u1", "bearer-fresh");
    expect(await store.sessionByTokenSha256(bearerSha256Hex("bearer-fresh"))).not.toBeNull();
  });
});

describe("connectedServer", () => {
  it("resolves the server's current revision for an unpinned connection", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "curr", { authMode: "oauth" });
    const server = await store.connectedServer("ws1", seeded.serverId);
    expect(server).not.toBeNull();
    expect(server?.url).toBe("https://mcp.example.com/curr");
    expect(server?.transport).toBe("streamable-http");
    expect(server?.authMode).toBe("oauth");
    expect(server?.secretHeader).toBeNull();
    expect(server?.displayName).toBe("Server curr");
    expect(server?.workspaceDisplayName).toBe("ws1");
    expect(server?.authorizeUrl).toBe(`${TOPOS_PUBLIC_URL}/mcp/${seeded.bundleName}`);
  });

  it("a pin outranks the server's current", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "pin", { pinned: true });
    const server = await store.connectedServer("ws1", seeded.serverId);
    expect(server?.url).toBe("https://mcp.example.com/pin/pinned");
  });

  it("an archived connection resolves to nothing", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "arch", { bundleStatus: "archived" });
    expect(await store.connectedServer("ws1", seeded.serverId)).toBeNull();
  });

  it("a packages-only server (no address) resolves to nothing", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "pkg", { url: null });
    expect(await store.connectedServer("ws1", seeded.serverId)).toBeNull();
  });

  it("an unconnected server resolves to nothing", async () => {
    expect(await store.connectedServer("ws1", "msrv_never")).toBeNull();
  });

  it("a connection the owner set 'direct' resolves to nothing — the mandate is enforced here", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "dirm", { authMode: "oauth" });
    await db.q("UPDATE web.bundle_mcp SET gateway_policy = 'direct' WHERE server_id = $1", [
      seeded.serverId,
    ]);
    expect(await store.connectedServer("ws1", seeded.serverId)).toBeNull();
  });

  it("a 'required' connection resolves like an unmandated one", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "reqm", { authMode: "oauth" });
    await db.q("UPDATE web.bundle_mcp SET gateway_policy = 'required' WHERE server_id = $1", [
      seeded.serverId,
    ]);
    expect(await store.connectedServer("ws1", seeded.serverId)).not.toBeNull();
  });

  it("a workspace whose gateway switch is off resolves nothing at all", async () => {
    await seedIdentity(db, "wsoff", "u1");
    const seeded = await seedConnectedServer(db, "wsoff", "swoff", { authMode: "oauth" });
    await db.q("UPDATE web.workspace SET mcp_gateway = 'off' WHERE id = 'wsoff'");
    expect(await store.connectedServer("wsoff", seeded.serverId)).toBeNull();
    // The switch back on is the whole re-enable — nothing was deleted.
    await db.q("UPDATE web.workspace SET mcp_gateway = 'on' WHERE id = 'wsoff'");
    expect(await store.connectedServer("wsoff", seeded.serverId)).not.toBeNull();
  });

  it("surfaces the document's declared secret-header slot", async () => {
    const url = "https://mcp.example.com/hdr";
    const seeded = await seedConnectedServer(db, "ws1", "hdr", {
      url,
      authMode: "manual",
      document: {
        name: "io.test/hdr",
        remotes: [
          { type: "sse", url: "https://mcp.example.com/ignored" },
          { type: "streamable-http", url, headers: [{ name: "X-API-Key", isSecret: true }] },
        ],
      },
    });
    const server = await store.connectedServer("ws1", seeded.serverId);
    expect(server?.secretHeader).toBe("X-API-Key");
  });
});

describe("secretHeaderFromDocument", () => {
  it("answers null for absent, non-secret, or malformed header declarations", () => {
    expect(secretHeaderFromDocument(null)).toBeNull();
    expect(secretHeaderFromDocument({})).toBeNull();
    expect(
      secretHeaderFromDocument({
        remotes: [
          {
            type: "streamable-http",
            url: "https://x.test",
            headers: [{ name: "X-Trace", value: "1" }],
          },
        ],
      }),
    ).toBeNull();
  });
});

describe("toolPolicy", () => {
  it("no row means all", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "polall");
    const policy = await store.toolPolicy("ws1", seeded.serverId);
    expect(policy.mode).toBe("all");
  });

  it("selected mode carries exactly the enabled names", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "polsel");
    await setToolPolicy(db, "ws1", seeded.serverId, "selected", ["read_file", "list_dir"]);
    const policy = await store.toolPolicy("ws1", seeded.serverId);
    expect(policy.mode).toBe("selected");
    expect([...policy.selected].sort()).toEqual(["list_dir", "read_file"]);
  });
});

describe("credential custody", () => {
  it("personal outranks the workspace service account; others fall back to it", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "cred");
    const wsCred = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: null,
      authKind: "manual",
      payload: { secret: "workspace-secret" },
      createdByDisplay: "owner",
    });
    const personal = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: "u1",
      authKind: "oauth",
      payload: { secret: "personal-token", refreshToken: "r1" },
      createdByDisplay: "u1",
    });
    const forU1 = await store.credentialFor("ws1", seeded.serverId, "u1");
    expect(forU1?.id).toBe(personal);
    expect(forU1?.kind).toBe("oauth");
    expect(forU1?.secret).toBe("personal-token");
    expect(forU1?.refreshToken).toBe("r1");
    const forU2 = await store.credentialFor("ws1", seeded.serverId, "u2");
    expect(forU2?.id).toBe(wsCred);
    expect(forU2?.secret).toBe("workspace-secret");
  });

  it("re-connecting a slot replaces the standing credential", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "recred");
    const first = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: "u1",
      authKind: "manual",
      payload: { secret: "one" },
      createdByDisplay: "u1",
    });
    const second = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: "u1",
      authKind: "manual",
      payload: { secret: "two" },
      createdByDisplay: "u1",
    });
    expect(second).not.toBe(first);
    const rows = await db.q(
      "SELECT id FROM gateway.credential WHERE workspace_id = 'ws1' AND server_id = $1 AND user_id = 'u1'",
      [seeded.serverId],
    );
    expect(rows.map((r) => r.id)).toEqual([second]);
    expect((await store.credentialFor("ws1", seeded.serverId, "u1"))?.secret).toBe("two");
  });

  it("deleteCredential cascades the secret and answers whether a row fell", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "delcred");
    const id = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: "u1",
      authKind: "manual",
      payload: { secret: "gone" },
      createdByDisplay: "u1",
    });
    expect(await store.deleteCredential(id)).toBe(true);
    expect(await store.deleteCredential(id)).toBe(false);
    const secrets = await db.q(
      "SELECT 1 AS one FROM gateway.credential_secret WHERE credential_id = $1",
      [id],
    );
    expect(secrets).toEqual([]);
    expect(await store.credentialFor("ws1", seeded.serverId, "u1")).toBeNull();
  });

  it("revocation lands immediately even with the workspace's data key already cached", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "revcache");
    const id = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: "u1",
      authKind: "manual",
      payload: { secret: "live" },
      createdByDisplay: "u1",
    });
    // This read warms the data-key cache for ws1 — the row is then deleted out from under it.
    expect((await store.credentialFor("ws1", seeded.serverId, "u1"))?.secret).toBe("live");
    expect(await store.deleteCredential(id)).toBe(true);
    // No key was consulted: the row is simply gone, which is what makes revocation a row change.
    expect(await store.credentialFor("ws1", seeded.serverId, "u1")).toBeNull();
  });

  it("saveRotatedCredential re-encrypts and stamps last_refreshed_at", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "rot");
    const id = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: "u1",
      authKind: "oauth",
      payload: { secret: "old", refreshToken: "keep-me", tokenEndpoint: "https://as.test/token" },
      createdByDisplay: "u1",
    });
    const before = await db.q(
      "SELECT ciphertext FROM gateway.credential_secret WHERE credential_id = $1",
      [id],
    );
    await store.saveRotatedCredential(id, { secret: "new" });
    const after = await db.q(
      "SELECT ciphertext FROM gateway.credential_secret WHERE credential_id = $1",
      [id],
    );
    expect(Buffer.compare(before[0]?.ciphertext as Buffer, after[0]?.ciphertext as Buffer)).not.toBe(0);
    const rotated = await store.credentialFor("ws1", seeded.serverId, "u1");
    expect(rotated?.secret).toBe("new");
    // An exchange that returns no new refresh token keeps the standing one.
    expect(rotated?.refreshToken).toBe("keep-me");
    expect(rotated?.tokenEndpoint).toBe("https://as.test/token");
    const meta = await db.q("SELECT last_refreshed_at FROM gateway.credential WHERE id = $1", [id]);
    expect(meta[0]?.last_refreshed_at).not.toBeNull();
  });

  it("withRefreshLock serializes competing refreshes and carries the rotation", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "lock");
    const id = await store.storeCredential({
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: "u1",
      authKind: "oauth",
      payload: { secret: "start" },
      createdByDisplay: "u1",
    });
    const order: string[] = [];
    let releaseFirst = () => {};
    const gate = new Promise<void>((resolve) => {
      releaseFirst = resolve;
    });
    const first = store.withRefreshLock(id, async () => {
      order.push("first-in");
      await store.saveRotatedCredential(id, { secret: "first" });
      await gate;
      order.push("first-out");
    });
    // Give the first callback time to take the row lock before the rival queues.
    await new Promise((resolve) => setTimeout(resolve, 50));
    const second = store.withRefreshLock(id, async () => {
      order.push("second-in");
      await store.saveRotatedCredential(id, { secret: "second" });
    });
    await new Promise((resolve) => setTimeout(resolve, 100));
    expect(order).toEqual(["first-in"]);
    releaseFirst();
    await Promise.all([first, second]);
    expect(order).toEqual(["first-in", "first-out", "second-in"]);
    expect((await store.credentialFor("ws1", seeded.serverId, "u1"))?.secret).toBe("second");
  });
});

describe("the usage sink", () => {
  it("lands buffered events on flush, null tool names intact", async () => {
    const sink = new BufferedUsageSink(pool, () => {});
    sink.record({
      workspaceId: "ws1",
      serverId: "msrv_u",
      sessionId: "sn_u",
      userId: "u1",
      toolName: "read_file",
      method: "tools/call",
      outcome: "ok",
      durationMs: 12.6,
    });
    sink.record({
      workspaceId: "ws1",
      serverId: "msrv_u",
      sessionId: "sn_u",
      userId: "u1",
      toolName: null,
      method: "initialize",
      outcome: "unauthorized",
      durationMs: 3,
    });
    await sink.close();
    const rows = await db.q(
      `SELECT tool_name, method, outcome, duration_ms FROM gateway.usage_event
        WHERE server_id = 'msrv_u' ORDER BY id`,
    );
    expect(rows).toEqual([
      { tool_name: "read_file", method: "tools/call", outcome: "ok", duration_ms: 13 },
      { tool_name: null, method: "initialize", outcome: "unauthorized", duration_ms: 3 },
    ]);
  });
});

describe("recordObservedTools", () => {
  it("upserts the advertised list and retires names that stop appearing", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "tools");
    await store.recordObservedTools("ws1", seeded.serverId, [
      { name: "alpha", description: "first" },
      { name: "beta" },
    ]);
    await store.recordObservedTools("ws1", seeded.serverId, [
      { name: "alpha", description: "renamed" },
    ]);
    const rows = await db.q(
      `SELECT name, description, currently_offered FROM gateway.observed_tool
        WHERE workspace_id = 'ws1' AND server_id = $1 ORDER BY name`,
      [seeded.serverId],
    );
    expect(rows).toEqual([
      { name: "alpha", description: "renamed", currently_offered: true },
      { name: "beta", description: null, currently_offered: false },
    ]);
  });

  it("never throws into the caller", async () => {
    // A broken write (undefined workspace binds fine; use an over-long name? — force a failure
    // by dropping to a poisoned pool) must resolve, not reject.
    const closed = new pg.Pool({ connectionString: db.gatewayUrl, max: 1 });
    await closed.end();
    const poisoned = new PgStore(closed, new EnvelopeCrypto(new FileMasterKey(randomBytes(32))), TOPOS_PUBLIC_URL, () => {});
    await expect(
      poisoned.recordObservedTools("ws1", "msrv_x", [{ name: "t" }]),
    ).resolves.toBeUndefined();
  });
});
