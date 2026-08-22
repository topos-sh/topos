import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { applyGatewayDdl } from "../helpers/gateway-ddl";
import { mcpRevisionId } from "../helpers/mcp-ids";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";

/**
 * THE SELECTION TRAP, CLOSED — the tool policy's one refusal and the checklist rule that makes it
 * almost impossible to reach.
 *
 * `selected` with nothing checked is a spellable state that means every tool on the server is off.
 * It reads as narrowing and lands as switching a server off, which is how a Linear connection got
 * silently disabled in production the day the panel first shipped. Two things stop it:
 *
 *  - THE WRITE REFUSES IT, at the data layer, so no door can spell it — and the route says why in
 *    the one sentence a person needs;
 *  - THE CHECKLIST STARTS FULL, so narrowing is unchecking and the state is never the default one
 *    a save would produce. A standing selection is preserved instead — opening the page is not an
 *    answer, and re-saving what the page rendered must not widen a workspace's own narrowing.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let ws = "";

const ORIGIN = "http://x";
const MEMBER = { id: "u_mem", name: "Mo Member", email: "mo@example.com" };
const SERVER = "mcps_tools";
const BUNDLE_NAME = "linear";

/** What the gateway lane's `tools/refresh` answered last — the transport is stubbed, never dialed. */
let laneAnswer: Response = Response.json({ outcome: "recorded", tools: 2 });
const laneCalls: { url: string; body: unknown }[] = [];

async function seedServer(): Promise<void> {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ($1, NULL, 'com.example/linear', 'Linear', 'oauth', 'active')`,
    [SERVER],
  );
  const revision = mcpRevisionId(SERVER);
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, upstream_version, document, transport, url, published_at, published_by)
     VALUES ($1, $2, 1, '1.0.0', $3::jsonb, 'streamable-http', 'https://mcp.example.com/mcp',
             now(), 'Staff')`,
    [revision, SERVER, JSON.stringify({ name: "com.example/linear", version: "1.0.0" })],
  );
  await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [
    SERVER,
    revision,
  ]);
  await seedBundle(db, ws, "b_linear", BUNDLE_NAME, { kind: "mcp", withPointer: false });
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id) VALUES ($1, $2, $3)`,
    ["b_linear", ws, SERVER],
  );
}

async function seedTool(name: string): Promise<void> {
  await db.q(
    `INSERT INTO gateway.observed_tool (workspace_id, server_id, name, description)
     VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING`,
    [ws, SERVER, name, `What ${name} does.`],
  );
}

/** Post one arm of the server face's action, exactly as the panel's forms do. */
async function post(fields: [string, string][]): Promise<{ status: number; body: unknown }> {
  const { action } = await import("@/routes/skill-current");
  const form = new URLSearchParams(fields);
  try {
    const answer = (await action({
      request: new Request(`${ORIGIN}/mcp/${BUNDLE_NAME}`, {
        method: "POST",
        // The same-origin guard is a write's floor here: a POST with no Origin is the house 404.
        headers: { origin: ORIGIN, "content-type": "application/x-www-form-urlencoded" },
        body: form.toString(),
      }),
      params: { server: BUNDLE_NAME },
      context: {},
    } as unknown as Parameters<typeof action>[0])) as {
      init?: ResponseInit;
      data?: unknown;
    };
    return { status: answer.init?.status ?? 200, body: answer.data ?? answer };
  } catch (thrown) {
    if (thrown instanceof Response) {
      return { status: thrown.status, body: null };
    }
    throw thrown;
  }
}

async function policyRow(): Promise<{ mode: string } | undefined> {
  const rows = await db.q<{ mode: string }>(
    `SELECT mode FROM web.mcp_tool_policy WHERE workspace_id = $1 AND server_id = $2`,
    [ws, SERVER],
  );
  return rows[0];
}

async function selectedNames(): Promise<string[]> {
  const rows = await db.q<{ tool_name: string }>(
    `SELECT tool_name FROM web.mcp_tool_selection
      WHERE workspace_id = $1 AND server_id = $2 ORDER BY tool_name`,
    [ws, SERVER],
  );
  return rows.map((row) => row.tool_name);
}

beforeAll(async () => {
  db = await createScratchDb("web_gateway_tool_selection", {
    // The arms live behind `gatewayLane() !== null`; a deployment with no gateway 404s them.
    GATEWAY_INTERNAL_URL: "http://gateway.internal:8789",
    GATEWAY_INTERNAL_TOKEN: "internal-token-unit",
  });
  await applyGatewayDdl(db.url);
  ws = await bootWorkspace();
  await seedUser(db, MEMBER.id, MEMBER.name, MEMBER.email);
  await seatUser(db, ws, MEMBER.id, "member");
  await seedServer();
  await seedTool("search");
  await seedTool("create_issue");
  session = { user: MEMBER };
  // No test here reaches the network: the lane's one transport is `fetch`, stubbed for the file.
  vi.stubGlobal("fetch", async (input: RequestInfo | URL, init?: RequestInit) => {
    laneCalls.push({
      url: String(input),
      body: init?.body === undefined ? null : JSON.parse(String(init.body)),
    });
    return laneAnswer.clone();
  });
}, 60000);

afterAll(async () => {
  vi.unstubAllGlobals();
  await db.drop();
});

beforeEach(() => {
  laneCalls.length = 0;
});

describe("saving the tool policy", () => {
  it("refuses `selected` with nothing checked, and writes no policy at all", async () => {
    const answer = await post([
      ["intent", "gateway-tools"],
      ["mode", "selected"],
    ]);

    expect(answer.status).toBe(400);
    expect(answer.body).toEqual({
      intent: "gateway-tools",
      status: "error",
      message:
        "Selected tools with nothing checked would disable every tool on this server. Choose All tools, or check at least one.",
    });
    // The trap in one line: had this landed, every tool on the server would be off.
    expect(await policyRow()).toBeUndefined();
    expect(await selectedNames()).toEqual([]);
  });

  it("refuses it at the data layer too, so no other door can spell it", async () => {
    const { setMcpToolPolicy } = await import("@/lib/db/queries.gateway.server");
    const { asMember } = await import("./helpers/scratch-db");
    expect(
      await setMcpToolPolicy(asMember(ws, MEMBER.id, "member", MEMBER.name), SERVER, {
        mode: "selected",
        tools: [],
      }),
    ).toBe("empty_selection");
    expect(await policyRow()).toBeUndefined();
    // A refused write leaves no audit row either — nothing happened to record.
    const audits = await db.q<{ n: string }>(
      `SELECT count(*) AS n FROM web.audit_event WHERE workspace_id = $1 AND kind = 'mcp_tools_set'`,
      [ws],
    );
    expect(audits[0]?.n).toBe("0");
  });

  it("lands `selected` the moment one tool is checked", async () => {
    const answer = await post([
      ["intent", "gateway-tools"],
      ["mode", "selected"],
      ["tool", "search"],
    ]);
    expect(answer).toEqual({ status: 200, body: { intent: "gateway-tools", status: "ok" } });
    expect((await policyRow())?.mode).toBe("selected");
    expect(await selectedNames()).toEqual(["search"]);
  });

  it("still lands `all` with nothing checked — widening was never the trap", async () => {
    const answer = await post([
      ["intent", "gateway-tools"],
      ["mode", "all"],
    ]);
    expect(answer.status).toBe(200);
    expect((await policyRow())?.mode).toBe("all");
    expect(await selectedNames()).toEqual([]);
  });
});

describe("the checklist a fresh render starts with", () => {
  async function checks(tools: { name: string; selected: boolean }[]): Promise<string[]> {
    const { startingToolChecks } = await import("@/components/skill/mcp-gateway");
    const rows = tools.map((tool) => ({ ...tool, description: null, currentlyOffered: true }));
    return [...startingToolChecks(rows)].sort();
  }

  it("starts FULL for a workspace that has never narrowed this server", async () => {
    expect(
      await checks([
        { name: "search", selected: false },
        { name: "create_issue", selected: false },
      ]),
    ).toEqual(["create_issue", "search"]);
  });

  it("keeps a standing selection instead of widening it back out", async () => {
    expect(
      await checks([
        { name: "search", selected: true },
        { name: "create_issue", selected: false },
      ]),
    ).toEqual(["search"]);
  });

  it("starts full for a `selected` policy that selected nothing — the state that has to heal", async () => {
    expect(
      await checks([
        { name: "search", selected: false },
        { name: "create_issue", selected: false },
      ]),
    ).toEqual(["create_issue", "search"]);
  });

  it("is empty only when the server offers nothing", async () => {
    expect(await checks([])).toEqual([]);
  });
});

describe("asking the server for its tools again", () => {
  it("calls the lane for THIS viewer and lands", async () => {
    laneAnswer = Response.json({ outcome: "recorded", tools: 2 });
    const answer = await post([["intent", "gateway-tools-refresh"]]);

    expect(answer).toEqual({
      status: 200,
      body: { intent: "gateway-tools-refresh", status: "ok" },
    });
    expect(laneCalls).toHaveLength(1);
    expect(laneCalls[0]?.url).toBe("http://gateway.internal:8789/internal/v1/tools/refresh");
    expect(laneCalls[0]?.body).toEqual({
      workspaceId: ws,
      serverId: SERVER,
      userId: MEMBER.id,
    });
  });

  it("says a server with no sign-in cannot be read", async () => {
    laneAnswer = Response.json({ outcome: "no_credential" });
    const answer = await post([["intent", "gateway-tools-refresh"]]);
    expect(answer.body).toEqual({
      intent: "gateway-tools-refresh",
      status: "error",
      message: "Connect a sign-in first — this server won't list its tools without one.",
    });
  });

  it("says the server did not answer, and does not claim the list changed", async () => {
    laneAnswer = Response.json({ outcome: "unreachable" });
    const answer = await post([["intent", "gateway-tools-refresh"]]);
    expect(answer.body).toEqual({
      intent: "gateway-tools-refresh",
      status: "error",
      message: "This server didn't answer. The tool list is unchanged.",
    });
  });

  it("folds a gateway that answered nothing usable into the house refusal", async () => {
    laneAnswer = new Response("no", { status: 500 });
    const answer = await post([["intent", "gateway-tools-refresh"]]);
    expect(answer.status).toBe(500);
    expect(answer.body).toMatchObject({
      intent: "gateway-tools-refresh",
      status: "error",
      message: "That didn't go through. Try again.",
    });
  });
});
