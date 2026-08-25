import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  createStaticHandler,
  createStaticRouter,
  type RouteObject,
  StaticRouterProvider,
} from "react-router";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { applyGatewayDdl } from "../helpers/gateway-ddl";
import { mcpRevisionId } from "../helpers/mcp-ids";
import {
  asMember,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * THE USAGE TABLE on a connected server's page, against a REAL scratch Postgres.
 *
 * The ledger is per-CALL and a working agent makes hundreds of them, so the table that rendered
 * one row per call rendered a hundred visually identical lines — same person, same machine, the
 * same "ok just now", and a `—` under Tool on nearly every one, because most calls through a
 * gateway are not tool calls. Nothing in it could be acted on, and everything past the hundredth
 * was silently gone.
 *
 * A row is now one SESSION — the unit a person can do something about — and the ledger is paged
 * rather than windowed. What this suite holds still:
 *
 *  - the counts are per session and the outcomes are split ok / not-ok, so one failing machine is
 *    visible next to a healthy one instead of averaged into a wall of lines;
 *  - a session that only ever spoke non-tool methods carries NO tools rather than a dash column;
 *  - rows come back by most recent activity, so the machine that just called is the first read;
 *  - the page number is CLAMPED, never trusted: a hand-typed `?page=99` reads the last page, and
 *    an unparseable one reads the first — an empty table would read as "no calls";
 *  - `?page=2` on the real route hands the page's own component the second page and says so.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let ws = "";

const ORIGIN = "http://x";
const MEMBER = { id: "u_mem", name: "Mo Member", email: "mo@example.com" };
const SERVER = "mcps_usage";
const QUIET_SERVER = "mcps_quiet";
const BUNDLE_NAME = "deepwiki";

const LAPTOP = "cs_laptop";
const RUNNER = "cs_runner";

const member = () => asMember(ws, MEMBER.id, "member", MEMBER.name);

async function dal() {
  return await import("@/lib/db/queries.gateway.server");
}

async function seedServer(id: string, name: string): Promise<void> {
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

async function connect(bundleId: string, bundleName: string, serverId: string): Promise<void> {
  await seedBundle(db, ws, bundleId, bundleName, { kind: "mcp", withPointer: false });
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id) VALUES ($1, $2, $3)`,
    [bundleId, ws, serverId],
  );
}

/** One call, placed on the clock by minutes-ago so the ordering under test is the real one. */
async function call(
  serverId: string,
  sessionId: string,
  userId: string,
  toolName: string | null,
  outcome: string,
  minutesAgo: number,
): Promise<void> {
  await db.q(
    `INSERT INTO gateway.usage_event
       (workspace_id, server_id, session_id, user_id, tool_name, method, outcome, duration_ms,
        created_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, 12, now() - ($8 || ' minutes')::interval)`,
    [
      ws,
      serverId,
      sessionId,
      userId,
      toolName,
      toolName === null ? "initialize" : "tools/call",
      outcome,
      String(minutesAgo),
    ],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_gateway_usage", {
    // The panel is read only where a gateway is deployed; with no lane there is no table at all.
    GATEWAY_INTERNAL_URL: "http://gateway.internal:8789",
    GATEWAY_INTERNAL_TOKEN: "internal-token-unit",
  });
  await applyGatewayDdl(db.url);
  ws = await bootWorkspace();
  await seedUser(db, MEMBER.id, MEMBER.name, MEMBER.email);
  await seatUser(db, ws, MEMBER.id, "member");
  await seedSession(db, LAPTOP, ws, MEMBER.id, "active", "Mo's laptop");
  await seedSession(db, RUNNER, ws, MEMBER.id, "active", "release runner");
  await seedServer(SERVER, "com.example/deepwiki");
  await connect("b_deepwiki", BUNDLE_NAME, SERVER);
  await seedServer(QUIET_SERVER, "com.example/quiet");
  await connect("b_quiet", "quiet-server", QUIET_SERVER);
  session = { user: MEMBER };
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("aggregating the ledger per session", () => {
  it("is empty before anything has been called", async () => {
    const page = await (await dal()).mcpUsageSessions(member(), QUIET_SERVER);
    expect(page).toEqual({ sessions: [], page: 1, pageCount: 1, total: 0 });
  });

  it("gives one row per session, counted and split ok / failed, newest activity first", async () => {
    // The laptop: five calls over an hour, two of which did not end ok.
    await call(SERVER, LAPTOP, MEMBER.id, null, "ok", 60);
    await call(SERVER, LAPTOP, MEMBER.id, "search", "ok", 40);
    await call(SERVER, LAPTOP, MEMBER.id, "search", "ok", 30);
    await call(SERVER, LAPTOP, MEMBER.id, "create_issue", "denied_tool", 20);
    await call(SERVER, LAPTOP, MEMBER.id, "create_issue", "upstream_error", 10);
    // The runner: two clean calls, and the most recent activity on the server.
    await call(SERVER, RUNNER, MEMBER.id, "search", "ok", 9);
    await call(SERVER, RUNNER, MEMBER.id, "search", "ok", 2);

    const page = await (await dal()).mcpUsageSessions(member(), SERVER);

    expect(page.total).toBe(2);
    expect(page.pageCount).toBe(1);
    expect(page.page).toBe(1);
    // Seven calls, two rows — the whole point of the change.
    expect(page.sessions).toHaveLength(2);
    expect(page.sessions.map((row) => row.sessionId)).toEqual([RUNNER, LAPTOP]);

    const [runner, laptop] = page.sessions;
    expect(runner).toMatchObject({
      person: "Mo Member",
      machine: "release runner",
      calls: 2,
      ok: 2,
      failed: 0,
      tools: ["search"],
    });
    expect(laptop).toMatchObject({
      machine: "Mo's laptop",
      calls: 5,
      ok: 3,
      failed: 2,
      // Distinct and alphabetical — five calls, two tools named once each.
      tools: ["create_issue", "search"],
    });
    // WHY they failed, not just how many: one call the tool policy refused and one the server
    // broke on are different problems, and the counts sum to `failed`.
    expect(laptop?.failures).toEqual([
      { kind: "denied_tool", count: 1 },
      { kind: "upstream_error", count: 1 },
    ]);
    expect(runner?.failures).toEqual([]);
    // The stretch the row covers, not the moment one call landed.
    expect(laptop?.firstCallMs).toBeLessThan(laptop?.lastCallMs ?? 0);
    expect(runner?.lastCallMs).toBeGreaterThan(laptop?.lastCallMs ?? 0);
  });

  it("carries no tools for a session that only ever spoke non-tool methods", async () => {
    await call(QUIET_SERVER, LAPTOP, MEMBER.id, null, "ok", 5);
    await call(QUIET_SERVER, LAPTOP, MEMBER.id, null, "no_credential", 4);

    const page = await (await dal()).mcpUsageSessions(member(), QUIET_SERVER);
    expect(page.sessions).toHaveLength(1);
    expect(page.sessions[0]?.tools).toEqual([]);
    expect(page.sessions[0]).toMatchObject({ calls: 2, ok: 1, failed: 1 });
    expect(page.sessions[0]?.failures).toEqual([{ kind: "no_credential", count: 1 }]);
  });

  it("orders the failure kinds by how many, so the biggest problem reads first", async () => {
    await seedSession(db, "cs_flaky", ws, MEMBER.id, "active", "flaky box");
    for (let i = 0; i < 3; i += 1) {
      await call(SERVER, "cs_flaky", MEMBER.id, "search", "upstream_error", 40 + i);
    }
    await call(SERVER, "cs_flaky", MEMBER.id, "create_issue", "denied_tool", 39);
    await call(SERVER, "cs_flaky", MEMBER.id, null, "unauthorized", 38);

    const page = await (await dal()).mcpUsageSessions(member(), SERVER);
    const flaky = page.sessions.find((row) => row.sessionId === "cs_flaky");
    expect(flaky?.failed).toBe(5);
    // Biggest first; the two ones tie and break by name, so the row never reorders itself.
    expect(flaky?.failures).toEqual([
      { kind: "upstream_error", count: 3 },
      { kind: "denied_tool", count: 1 },
      { kind: "unauthorized", count: 1 },
    ]);
  });

  it("stands in for a gone account and a removed machine rather than dropping the row", async () => {
    await call(SERVER, "cs_ghost", "u_ghost", "search", "unauthorized", 1);
    const page = await (await dal()).mcpUsageSessions(member(), SERVER);
    expect(page.sessions[0]).toMatchObject({
      person: "former member",
      machine: "a removed machine",
      calls: 1,
      ok: 0,
      failed: 1,
    });
  });

  it("clamps a page number past the end, and an unparseable one, onto a real page", async () => {
    const far = await (await dal()).mcpUsageSessions(member(), SERVER, { page: 99 });
    expect(far.page).toBe(1);
    expect(far.pageCount).toBe(1);
    expect(far.sessions.length).toBeGreaterThan(0);

    const nonsense = await (await dal()).mcpUsageSessions(member(), SERVER, { page: Number.NaN });
    expect(nonsense.page).toBe(1);
  });

  it("sees nothing of another workspace's calls against the same server", async () => {
    await call(SERVER, "cs_elsewhere", MEMBER.id, "search", "ok", 1);
    await db.q(
      `UPDATE gateway.usage_event SET workspace_id = 'ws_elsewhere' WHERE session_id = $1`,
      ["cs_elsewhere"],
    );
    const page = await (await dal()).mcpUsageSessions(member(), SERVER);
    expect(page.sessions.map((row) => row.sessionId)).not.toContain("cs_elsewhere");
  });
});

describe("the server page's Usage pages", () => {
  const PAGED_SERVER = "mcps_paged";
  const PAGED_BUNDLE = "linear";
  /** One over a page, so page 2 holds exactly the spill. */
  const MACHINES = 26;

  beforeAll(async () => {
    await seedServer(PAGED_SERVER, "com.example/linear");
    await connect("b_linear", PAGED_BUNDLE, PAGED_SERVER);
    for (let i = 0; i < MACHINES; i += 1) {
      const id = `cs_fleet_${String(i).padStart(2, "0")}`;
      await seedSession(db, id, ws, MEMBER.id, "active", `fleet ${i}`);
      // Machine 0 called most recently, machine 25 longest ago — so the oldest is the page-2 row.
      await call(PAGED_SERVER, id, MEMBER.id, "search", "ok", i + 1);
    }
  }, 60000);

  it("fills page 1 and spills the oldest machine onto page 2", async () => {
    const first = await (await dal()).mcpUsageSessions(member(), PAGED_SERVER);
    expect(first).toMatchObject({ page: 1, pageCount: 2, total: MACHINES });
    expect(first.sessions).toHaveLength(25);
    expect(first.sessions[0]?.machine).toBe("fleet 0");

    const second = await (await dal()).mcpUsageSessions(member(), PAGED_SERVER, { page: 2 });
    expect(second).toMatchObject({ page: 2, pageCount: 2, total: MACHINES });
    expect(second.sessions).toHaveLength(1);
    expect(second.sessions[0]?.machine).toBe("fleet 25");
  });

  it("hands the route's `?page=2` to the page, and page 1 without it", async () => {
    const { loader } = await import("@/routes/skill-current");
    const load = async (query: string) =>
      (await loader({
        request: new Request(`${ORIGIN}/mcp/${PAGED_BUNDLE}${query}`),
        params: { server: PAGED_BUNDLE },
        context: {},
      } as unknown as Parameters<typeof loader>[0])) as {
        gateway: { usage: { page: number; pageCount: number; total: number; sessions: unknown[] } };
      };

    const bare = await load("");
    expect(bare.gateway.usage).toMatchObject({ page: 1, pageCount: 2, total: MACHINES });
    expect(bare.gateway.usage.sessions).toHaveLength(25);

    const page2 = await load("?page=2");
    expect(page2.gateway.usage).toMatchObject({ page: 2, pageCount: 2, total: MACHINES });
    expect(page2.gateway.usage.sessions).toHaveLength(1);
  });
});

/** Render the panel the way the server face does, inside a router so its links resolve. */
async function renderPanel(usage: unknown): Promise<string> {
  const { McpGatewayPanel } = await import("@/components/skill/mcp-gateway");
  const view = {
    displayName: "Linear",
    authMode: "none",
    signedIn: null,
    canConnectPersonal: false,
    canConnectWorkspace: false,
    mode: "all",
    tools: [],
    usage,
  };
  const routes: RouteObject[] = [
    {
      // The real address, so the pager's search-only links resolve the way they do in the app.
      path: "/mcp/:server",
      Component: () =>
        createElement(McpGatewayPanel, {
          view: view as Parameters<typeof McpGatewayPanel>[0]["view"],
        }),
    },
  ];
  const handler = createStaticHandler(routes);
  const context = await handler.query(new Request(`${ORIGIN}/mcp/${BUNDLE_NAME}`));
  if (context instanceof Response) {
    throw new Error("expected a rendered context, got a Response");
  }
  const router = createStaticRouter(handler.dataRoutes, context);
  return renderToStaticMarkup(createElement(StaticRouterProvider, { router, context }));
}

function usageRow(overrides: Record<string, unknown>) {
  return {
    sessionId: "cs_laptop",
    person: "Mo Member",
    machine: "Mo's laptop",
    calls: 5,
    ok: 3,
    failed: 2,
    failures: [
      { kind: "no_credential", count: 2 },
      { kind: "upstream_error", count: 1 },
    ],
    tools: ["create_issue", "search"],
    firstCallMs: Date.now() - 3_600_000,
    lastCallMs: Date.now() - 60_000,
    ...overrides,
  };
}

describe("what the Usage table renders", () => {
  it("says how many sessions there are in total, and where in them the reader stands", async () => {
    const html = await renderPanel({
      sessions: [usageRow({}), usageRow({ sessionId: "cs_runner", machine: "release runner" })],
      page: 2,
      pageCount: 4,
      total: 84,
    });
    expect(html).toContain("84 sessions, newest activity first — page 2 of 4.");
    // Two rows for two sessions, whatever the call count on them.
    expect(html.match(/data-testid="mcp-usage-cs_/g)).toHaveLength(2);
    // The cell names the kinds: "3 failed" alone is a number nobody can act on.
    expect(html).toContain("3 ok · 2 failed (2 no sign-in, 1 server error)");
    expect(html).toContain("create_issue, search");
  });

  it("offers both directions in the middle of the ledger", async () => {
    const html = await renderPanel({ sessions: [usageRow({})], page: 2, pageCount: 4, total: 84 });
    expect(html).toContain('href="/mcp/deepwiki?page=1"');
    expect(html).toContain('href="/mcp/deepwiki?page=3"');
  });

  it("offers no page control at all when the whole ledger fits on one page", async () => {
    const html = await renderPanel({ sessions: [usageRow({})], page: 1, pageCount: 1, total: 1 });
    expect(html).toContain("1 session, newest activity first.");
    expect(html).not.toContain('data-testid="mcp-usage-pager"');
  });

  it("says only the counts for a session where nothing failed", async () => {
    const html = await renderPanel({
      sessions: [usageRow({ calls: 4, ok: 4, failed: 0, failures: [] })],
      page: 1,
      pageCount: 1,
      total: 1,
    });
    expect(html).toContain("4 ok · 0 failed");
    expect(html).not.toContain("4 ok · 0 failed (");
  });

  it("says `—` once for a session that called no tools, not a column of them", async () => {
    const html = await renderPanel({
      sessions: [usageRow({ tools: [], calls: 2, ok: 2, failed: 0, failures: [] })],
      page: 1,
      pageCount: 1,
      total: 1,
    });
    const row = html.slice(html.indexOf('data-testid="mcp-usage-cs_laptop"'));
    const cells = row.slice(0, row.indexOf("</tr>"));
    // One dash in the whole row: the Tools cell. Every other cell carries a real number.
    expect(cells.match(/—/g)).toHaveLength(1);
  });
});
