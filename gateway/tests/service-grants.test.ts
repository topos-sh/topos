import { randomBytes } from "node:crypto";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { EnvelopeCrypto } from "../service/crypto";
import { PgStore } from "../service/store";
import {
  createServiceDb,
  seedConnectedServer,
  seedIdentity,
  type ServiceDb,
} from "./helpers/service-db";

/**
 * THE GRANT BOUNDARY, probed by LOGGING IN as each role — never SET ROLE, which adopts neither
 * the role's search_path nor its login-time privilege surface. `topos_web` may read credential
 * METADATA, observed tools, and usage; the ciphertext table, the key table, and the ceremony
 * rows refuse it. `topos_gateway` reads the web tables the store needs.
 */

let db: ServiceDb;
let webPool: pg.Pool;
let gatewayPool: pg.Pool;

beforeAll(async () => {
  db = await createServiceDb();
  await seedIdentity(db, "ws1", "u1");
  const seeded = await seedConnectedServer(db, "ws1", "grants");
  webPool = new pg.Pool({ connectionString: db.webUrl, max: 2 });
  gatewayPool = new pg.Pool({ connectionString: db.gatewayUrl, max: 2 });
  const store = new PgStore(
    gatewayPool,
    new EnvelopeCrypto(randomBytes(32)),
    "https://team.example.com",
    () => {},
  );
  await store.storeCredential({
    workspaceId: "ws1",
    serverId: seeded.serverId,
    userId: "u1",
    authKind: "manual",
    payload: { secret: "sealed" },
    createdByDisplay: "u1",
  });
}, 120_000);

afterAll(async () => {
  await webPool?.end();
  await gatewayPool?.end();
  await db?.drop();
});

async function deniedFor(pool: pg.Pool, sql: string): Promise<boolean> {
  try {
    await pool.query(sql);
    return false;
  } catch (error) {
    return (error as { code?: string }).code === "42501"; // insufficient_privilege
  }
}

describe("as topos_web", () => {
  it("reads credential metadata", async () => {
    const rows = await webPool.query(
      "SELECT id, workspace_id, server_id, user_id, auth_kind, created_by FROM gateway.credential",
    );
    expect(rows.rows.length).toBeGreaterThan(0);
  });

  it("reads observed_tool and usage_event", async () => {
    await webPool.query("SELECT workspace_id, name FROM gateway.observed_tool");
    await webPool.query("SELECT workspace_id, outcome FROM gateway.usage_event");
  });

  it("is DENIED the ciphertext table", async () => {
    expect(await deniedFor(webPool, "SELECT * FROM gateway.credential_secret")).toBe(true);
  });

  it("is DENIED the workspace key table", async () => {
    expect(await deniedFor(webPool, "SELECT * FROM gateway.workspace_key")).toBe(true);
  });

  it("is DENIED the oauth ceremony rows", async () => {
    expect(await deniedFor(webPool, "SELECT * FROM gateway.oauth_flow")).toBe(true);
  });

  it("cannot write any gateway table it can read", async () => {
    expect(
      await deniedFor(webPool, "DELETE FROM gateway.credential WHERE id = 'cred_none'"),
    ).toBe(true);
    expect(
      await deniedFor(
        webPool,
        "INSERT INTO gateway.observed_tool (workspace_id, server_id, name) VALUES ('w', 's', 't')",
      ),
    ).toBe(true);
  });
});

describe("as topos_gateway", () => {
  it("reads the web tables the store needs", async () => {
    for (const table of [
      "cli_session",
      "bundle_mcp",
      "mcp_server",
      "mcp_server_revision",
      "seat",
      "workspace",
      "bundle",
      "mcp_tool_policy",
      "mcp_tool_selection",
    ]) {
      await gatewayPool.query(`SELECT 1 FROM web.${table} LIMIT 1`);
    }
  });

  it("cannot write the web schema", async () => {
    expect(
      await deniedFor(
        gatewayPool,
        "UPDATE web.cli_session SET status = 'active' WHERE id = 'sn_none'",
      ),
    ).toBe(true);
  });
});
