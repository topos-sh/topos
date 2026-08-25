import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { applyGatewayDdl } from "../helpers/gateway-ddl";
import { mcpRevisionId } from "../helpers/mcp-ids";
import {
  asMember,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";

/**
 * THE CONNECTED SERVER'S GATEWAY PANEL, against a REAL scratch Postgres — the three reads the page
 * makes and the one write it offers.
 *
 * What the suite is actually holding still:
 *  - a workspace that never opened the Tools panel reads as `all`, because no row means `all`;
 *  - the checklist is REPLACED by what a save posts, so unchecking is a real answer;
 *  - the policy hangs off the CONNECTION: disconnecting takes it and its selections with it, which
 *    is a database cascade rather than cleanup code somebody has to remember to write;
 *  - sign-in resolution is the gateway's own — the person's credential first, the workspace
 *    service account second — and the page never sees more than metadata.
 *
 * The Usage table has its own suite (gateway-usage.test.ts) — it is a read of its own shape.
 */

let db: ScratchDb;
let ws = "";

const MEMBER = "u_mem";
const OTHER = "u_other";
const SERVER = "mcps_gate";
const OTHER_SERVER = "mcps_second";

async function dal() {
  return await import("@/lib/db/queries.gateway.server");
}

const member = () => asMember(ws, MEMBER, "member", "Mo Member");
const otherMember = () => asMember(ws, OTHER, "member", "Ola Other");

async function seedServer(id: string, name: string) {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ($1, NULL, $2, $2, 'oauth', 'active')`,
    [id, name],
  );
  const revision = mcpRevisionId(id);
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, upstream_version, document, transport, url, published_at, published_by)
     VALUES ($1, $2, 1, '1.0.0', $3::jsonb, 'streamable-http', 'https://mcp.example.com/mcp',
             now(), 'Staff')`,
    [revision, id, JSON.stringify({ name, version: "1.0.0" })],
  );
  await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [id, revision]);
}

async function connect(bundleId: string, bundleName: string, serverId: string) {
  await seedBundle(db, ws, bundleId, bundleName, { kind: "mcp", withPointer: false });
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id) VALUES ($1, $2, $3)`,
    [bundleId, ws, serverId],
  );
}

// The real lineage checks the id shape (`cred_<32hex>`) — mint readable, valid ids.
const CRED_WS = `cred_${"a".repeat(32)}`;
const CRED_MINE = `cred_${"b".repeat(32)}`;
const CRED_ELSEWHERE = `cred_${"c".repeat(32)}`;

async function seedCredential(id: string, serverId: string, userId: string | null) {
  await db.q(
    `INSERT INTO gateway.credential (id, workspace_id, server_id, user_id, auth_kind, created_by)
     VALUES ($1, $2, $3, $4, 'oauth', 'seed')`,
    [id, ws, serverId, userId],
  );
}

async function seedTool(serverId: string, name: string, offered = true) {
  await db.q(
    `INSERT INTO gateway.observed_tool (workspace_id, server_id, name, description, currently_offered)
     VALUES ($1, $2, $3, $4, $5)`,
    [ws, serverId, name, `What ${name} does.`, offered],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_gateway_panel");
  await applyGatewayDdl(db.url);
  ws = await bootWorkspace();
  await seedUser(db, MEMBER, "Mo Member", "mo@example.com");
  await seatUser(db, ws, MEMBER, "member");
  await seedUser(db, OTHER, "Ola Other", "ola@example.com");
  await seatUser(db, ws, OTHER, "member");

  await seedServer(SERVER, "com.example/gated");
  await connect("b_gated", "gated-server", SERVER);
  await seedServer(OTHER_SERVER, "com.example/second");
  await connect("b_second", "second-server", OTHER_SERVER);
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the tool policy", () => {
  it("reads as `all` for a connection nobody has narrowed", async () => {
    const view = await (await dal()).mcpToolsView(member(), SERVER);
    expect(view.mode).toBe("all");
    expect(view.tools).toEqual([]);
    expect(view.selectedButUnseen).toEqual([]);
  });

  it("merges the observed list with the workspace's selection", async () => {
    const { mcpToolsView, setMcpToolPolicy } = await dal();
    await seedTool(SERVER, "search");
    await seedTool(SERVER, "create_issue");
    await seedTool(SERVER, "retired_tool", false);

    expect(
      await setMcpToolPolicy(member(), SERVER, { mode: "selected", tools: ["search", "ghost"] }),
    ).toBe("saved");

    const view = await mcpToolsView(member(), SERVER);
    expect(view.mode).toBe("selected");
    // Name-ordered, so a checklist never reshuffles between two reads of the same rows.
    expect(view.tools.map((tool) => tool.name)).toEqual(["create_issue", "retired_tool", "search"]);
    expect(view.tools.find((tool) => tool.name === "search")?.selected).toBe(true);
    expect(view.tools.find((tool) => tool.name === "create_issue")?.selected).toBe(false);
    expect(view.tools.find((tool) => tool.name === "retired_tool")?.currentlyOffered).toBe(false);
    // A selection naming a tool the server does not offer is kept, and never rendered as one.
    expect(view.selectedButUnseen).toEqual(["ghost"]);
  });

  it("replaces the selection wholesale — unchecking is an answer, not a no-op", async () => {
    const { mcpToolsView, setMcpToolPolicy } = await dal();
    await setMcpToolPolicy(member(), SERVER, { mode: "selected", tools: ["create_issue"] });
    const view = await mcpToolsView(member(), SERVER);
    expect(view.tools.filter((tool) => tool.selected).map((tool) => tool.name)).toEqual([
      "create_issue",
    ]);
    expect(view.selectedButUnseen).toEqual([]);
  });

  it("keeps the selection standing when the mode goes back to `all`", async () => {
    const { mcpToolsView, setMcpToolPolicy } = await dal();
    await setMcpToolPolicy(member(), SERVER, { mode: "all", tools: ["create_issue"] });
    const view = await mcpToolsView(member(), SERVER);
    expect(view.mode).toBe("all");
    expect(view.tools.find((tool) => tool.name === "create_issue")?.selected).toBe(true);
  });

  it("lands one audit row per save, naming the server and never the tool names", async () => {
    const rows = await db.q<{ kind: string; subject: string; details: Record<string, unknown> }>(
      `SELECT kind, subject, details FROM web.audit_event
       WHERE workspace_id = $1 AND kind = 'mcp_tools_set' ORDER BY id`,
      [ws],
    );
    expect(rows.length).toBeGreaterThanOrEqual(3);
    expect(rows[0]?.subject).toBe(SERVER);
    expect(rows[0]?.details).toEqual({ mode: "selected", selected: 2 });
    expect(JSON.stringify(rows)).not.toContain("create_issue");
  });

  it("refuses a server this workspace is not connected to, typed rather than thrown", async () => {
    // `selected` with a real checklist — an EMPTY one is refused ahead of the connection check,
    // and that refusal has its own suite (gateway-tool-selection.test.ts).
    const outcome = await (await dal()).setMcpToolPolicy(member(), "mcps_nobody", {
      mode: "selected",
      tools: ["search"],
    });
    expect(outcome).toBe("not_connected");
  });

  it("goes with the connection — disconnecting cascades the policy and its selections away", async () => {
    const { mcpToolsView, setMcpToolPolicy } = await dal();
    await setMcpToolPolicy(member(), OTHER_SERVER, { mode: "selected", tools: ["a", "b"] });
    expect(
      (
        await db.q<{ n: string }>(
          `SELECT count(*) AS n FROM web.mcp_tool_selection WHERE server_id = $1`,
          [OTHER_SERVER],
        )
      )[0]?.n,
    ).toBe("2");

    await db.q(`DELETE FROM web.bundle_mcp WHERE bundle_id = 'b_second'`);

    expect(
      (
        await db.q<{ n: string }>(
          `SELECT count(*) AS n FROM web.mcp_tool_policy WHERE server_id = $1`,
          [OTHER_SERVER],
        )
      )[0]?.n,
    ).toBe("0");
    expect(
      (
        await db.q<{ n: string }>(
          `SELECT count(*) AS n FROM web.mcp_tool_selection WHERE server_id = $1`,
          [OTHER_SERVER],
        )
      )[0]?.n,
    ).toBe("0");
    // And the read falls back to what no row has always meant.
    expect((await mcpToolsView(member(), OTHER_SERVER)).mode).toBe("all");
  });
});

describe("the sign-in state", () => {
  it("says nobody has connected one when nobody has", async () => {
    const state = await (await dal()).mcpSignInState(member(), SERVER);
    expect(state).toEqual({ mine: null, workspace: null });
  });

  it("sees the workspace account, and prefers a person's own over it", async () => {
    const { mcpCredentialIdFor, mcpSignInState } = await dal();
    await seedCredential(CRED_WS, SERVER, null);
    let state = await mcpSignInState(member(), SERVER);
    expect(state.mine).toBeNull();
    expect(state.workspace?.credentialId).toBe(CRED_WS);

    await seedCredential(CRED_MINE, SERVER, MEMBER);
    state = await mcpSignInState(member(), SERVER);
    expect(state.mine?.credentialId).toBe(CRED_MINE);
    expect(state.workspace?.credentialId).toBe(CRED_WS);
    // The disconnect arm resolves its id from the SCOPE, never from the browser.
    expect(await mcpCredentialIdFor(member(), SERVER, "mine")).toBe(CRED_MINE);
    expect(await mcpCredentialIdFor(member(), SERVER, "workspace")).toBe(CRED_WS);
  });

  it("never shows one person's sign-in to another", async () => {
    const state = await (await dal()).mcpSignInState(otherMember(), SERVER);
    expect(state.mine).toBeNull();
    expect(state.workspace?.credentialId).toBe(CRED_WS);
  });

  it("carries metadata only — the row shape has nowhere to put a secret", async () => {
    const state = await (await dal()).mcpSignInState(member(), SERVER);
    expect(Object.keys(state.mine ?? {}).sort()).toEqual([
      "authKind",
      "createdAt",
      "credentialId",
      "lastRefreshedAt",
    ]);
  });

  it("is workspace-scoped: another workspace's credential is not visible", async () => {
    await db.q(
      `INSERT INTO gateway.credential (id, workspace_id, server_id, user_id, auth_kind, created_by)
       VALUES ($3, 'ws_elsewhere', $1, $2, 'oauth', 'seed')`,
      [SERVER, MEMBER, CRED_ELSEWHERE],
    );
    const state = await (await dal()).mcpSignInState(member(), SERVER);
    expect(state.mine?.credentialId).toBe(CRED_MINE);
  });
});
