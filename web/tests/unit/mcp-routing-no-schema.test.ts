import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { mcpRevisionId } from "../helpers/mcp-ids";
import {
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
 * `GATEWAY_PUBLIC_URL` IS SET AND THE GATEWAY'S SCHEMA IS NOT THERE — the deployment shape that
 * lies between "no gateway" and "a gateway": a self-hoster who set the variable without ever
 * starting one, and every rolling deploy in the moment before the gateway container has run its
 * lineage. This suite arms the variable and deliberately creates NO `gateway` schema.
 *
 * The routing facts read `gateway.credential`, and Postgres refuses a statement naming a missing
 * relation at PARSE time — so getting this wrong is not a wrong answer, it is an exception on
 * every read that routes. Since routing became three lanes that is the workspace catalog and the
 * lock read too: every project's MCP install, not one machine's feed. The lanes must answer the
 * DIRECT shape instead, which is exactly what a deployment with no gateway can honor.
 */

let db: ScratchDb;
let ws = "";

const MEMBER = "u_mem";
const SESSION = "sn_laptop";
const UPSTREAM = "https://upstream.example.com/mcp";

async function lane() {
  return await import("@/lib/db/queries.lane.server");
}

const caller = () => asSession(ws, MEMBER, SESSION);

beforeAll(async () => {
  db = await createScratchDb("web_mcp_no_gwschema", {
    GATEWAY_PUBLIC_URL: "https://gw.example.com",
  });
  ws = await bootWorkspace();
  await seedUser(db, MEMBER, "Mo Member", "mo@example.com");
  await seatUser(db, ws, MEMBER, "member");
  await seedSession(db, SESSION, ws, MEMBER, "active", "Mo's laptop");

  // A server the ruling would route on sight if it could read the gateway's rows: no sign-in
  // needed, no mandate, no opt-out.
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ('mcps_open', NULL, 'com.example/open', 'com.example/open', 'none', 'active')`,
  );
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, upstream_version, document, transport, url, published_at, published_by)
     VALUES ($1, 'mcps_open', 1, '1.0.0', $2::jsonb, 'streamable-http', $3, now(), 'Staff')`,
    [
      mcpRevisionId("mcps_open"),
      JSON.stringify({
        name: "com.example/open",
        description: "A server for the no-schema suite.",
        version: "1.0.0",
        remotes: [{ type: "streamable-http", url: UPSTREAM }],
      }),
      UPSTREAM,
    ],
  );
  await db.q(`UPDATE web.mcp_server SET current_revision_id = $1 WHERE id = 'mcps_open'`, [
    mcpRevisionId("mcps_open"),
  ]);
  await seedBundle(db, ws, "b_open", "open-server", { kind: "mcp", withPointer: false });
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id) VALUES ('b_open', $1, 'mcps_open')`,
    [ws],
  );
  await assignBundleRow(db, ws, "b_open", MEMBER);
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("every lane survives a gateway URL with no gateway schema behind it", () => {
  it("delivery answers the server's own address instead of throwing", async () => {
    const body = await (await lane()).deliveryFor(caller());
    const served = body.mcp_servers.find((row) => row.skill_id === "b_open");
    expect((served?.document?.remotes as { url: string }[])[0]?.url).toBe(UPSTREAM);
  });

  it("the workspace catalog does too — the lane every project row reads", async () => {
    const index = await (await lane()).laneMcpServersIndex(caller());
    const served = index.find((row) => row.skill_id === "b_open");
    expect((served?.document?.remotes as { url: string }[])[0]?.url).toBe(UPSTREAM);
  });

  it("and the lock read, which is a project's pinned install", async () => {
    const entry = await (await lane()).laneMcpRevision(
      caller(),
      "b_open",
      mcpRevisionId("mcps_open"),
    );
    expect((entry?.document?.remotes as { url: string }[])[0]?.url).toBe(UPSTREAM);
  });

  it("names the absence for what it is: the schema is not readable here", async () => {
    const { gatewayCredentialsReadable } = await import("@/lib/db/queries.gateway.server");
    expect(await gatewayCredentialsReadable()).toBe(false);
  });
});
