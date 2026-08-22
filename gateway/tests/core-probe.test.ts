import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { probeServerTools } from "../core/probe";
import { memory, resetEngineMemoryForTests } from "../core/state";
import {
  fake2411,
  FakeStore,
  FakeUsage,
  legacyFake,
  makeCtx,
  modernFake,
  oauthCredential,
  resolvedServer,
  SERVER_ID,
  UPSTREAM_URL,
  USER,
  WS,
  type FetchHandler,
  type TestCtx,
} from "./core-helpers";

/**
 * THE TOOL PROBE — the list has to exist before a policy can narrow it, so connecting a sign-in
 * reads it rather than waiting for some agent's first call.
 *
 * What this suite holds still:
 *  - the probe records the UNFILTERED list, whatever the standing policy says, because the panel's
 *    checklist must show what the server offers and not what policy already allows;
 *  - it writes NO usage row — the sink it is handed here is the real recording one, and it stays
 *    empty, so "no usage row" is a property of the probe rather than of how a host wires it;
 *  - a server that refuses, is unreachable, or answers something that is not a tool list leaves the
 *    observed list untouched and answers a value, never a throw;
 *  - the credential is attached exactly as a live call attaches it;
 *  - the conversation it opened goes with it: no upstream session is left standing under the
 *    synthetic probe id, and a 2024-11-05 SSE pump is not left running.
 */

const PROBE_SESSION = `probe:${WS}:${SERVER_ID}`;

let t: TestCtx;
let store: FakeStore;
let usage: FakeUsage;

function boot(routes: Record<string, FetchHandler>): void {
  store = new FakeStore();
  usage = new FakeUsage();
  t = makeCtx(store, usage, routes);
}

beforeEach(() => {
  resetEngineMemoryForTests();
});
afterEach(() => resetEngineMemoryForTests());

describe("what a probe records", () => {
  it("records the full list a legacy server offers, and no usage row", async () => {
    const upstream = legacyFake("2025-06-18", { secret: "up-secret-1" });
    boot({ [UPSTREAM_URL]: upstream.handler });
    store.setServer(WS, resolvedServer());
    store.setCredential(WS, SERVER_ID, USER, oauthCredential());

    const outcome = await probeServerTools(t.ctx, {
      workspaceId: WS,
      serverId: SERVER_ID,
      userId: USER,
    });

    expect(outcome).toEqual({ kind: "recorded", tools: 3 });
    expect(store.observed).toHaveLength(1);
    expect(store.observed[0]?.tools.map((tool) => tool.name)).toEqual([
      "issue_create",
      "issue_delete",
      "issue_list",
    ]);
    expect(store.observed[0]?.tools[0]?.description).toBe("Create an issue");
    // The whole point of the sink being real here: a probe is not an agent's call.
    expect(usage.events).toEqual([]);
  });

  it("records the UNFILTERED list even where policy already narrows the server to one tool", async () => {
    const upstream = legacyFake("2025-11-25");
    boot({ [UPSTREAM_URL]: upstream.handler });
    store.setServer(WS, resolvedServer({ authMode: "none" }));
    store.setPolicy(WS, SERVER_ID, { mode: "selected", selected: new Set(["issue_list"]) });

    await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER });

    expect(store.observed[0]?.tools.map((tool) => tool.name)).toEqual([
      "issue_create",
      "issue_delete",
      "issue_list",
    ]);
  });

  it("reads a modern server through the same detection the live path uses", async () => {
    const upstream = modernFake();
    boot({ [UPSTREAM_URL]: upstream.handler });
    store.setServer(WS, resolvedServer({ authMode: "none" }));

    expect(
      await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER }),
    ).toEqual({ kind: "recorded", tools: 3 });
    expect(upstream.discoverCount).toBe(1);
    expect(store.observed[0]?.tools).toHaveLength(3);
  });

  it("attaches the credential — a server that demands the secret answers the probe", async () => {
    const upstream = legacyFake("2025-06-18", { secret: "api-key-9", secretHeader: "x-api-key" });
    boot({ [UPSTREAM_URL]: upstream.handler });
    store.setServer(WS, resolvedServer({ authMode: "manual", secretHeader: "x-api-key" }));
    store.setCredential(WS, SERVER_ID, USER, {
      id: "cred_manual",
      kind: "manual",
      secret: "api-key-9",
    });

    expect(
      (await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER })).kind,
    ).toBe("recorded");
  });

  it("asks with the WORKSPACE sign-in when the connect was a workspace one", async () => {
    const upstream = legacyFake("2025-06-18", { secret: "workspace-secret" });
    boot({ [UPSTREAM_URL]: upstream.handler });
    store.setServer(WS, resolvedServer());
    store.setCredential(WS, SERVER_ID, null, oauthCredential({ secret: "workspace-secret" }));

    expect(
      (await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: null })).kind,
    ).toBe("recorded");
  });
});

describe("pages", () => {
  /** A server that hands the list out in two pages and refuses an unknown cursor. */
  function pagedUpstream(): FetchHandler {
    return async (req) => {
      const body = (await req.json()) as Record<string, unknown>;
      const id = body["id"];
      const method = body["method"];
      if (method === "initialize") {
        return Response.json({
          jsonrpc: "2.0",
          id,
          result: { protocolVersion: "2025-06-18", capabilities: { tools: {} }, serverInfo: { name: "paged" } },
        });
      }
      if (method !== "tools/list") {
        return new Response(null, { status: 202 });
      }
      const cursor = (body["params"] as Record<string, unknown> | undefined)?.["cursor"];
      if (cursor === undefined) {
        return Response.json({
          jsonrpc: "2.0",
          id,
          result: { tools: [{ name: "one" }, { name: "two" }], nextCursor: "page-2" },
        });
      }
      // A name repeated across pages is a real shape; the write must not see it twice.
      return Response.json({
        jsonrpc: "2.0",
        id,
        result: { tools: [{ name: "two" }, { name: "three" }] },
      });
    };
  }

  it("follows the cursor and writes ONE list, deduped", async () => {
    boot({ [UPSTREAM_URL]: pagedUpstream() });
    store.setServer(WS, resolvedServer({ authMode: "none" }));

    expect(
      await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER }),
    ).toEqual({ kind: "recorded", tools: 3 });
    expect(store.observed).toHaveLength(1);
    expect(store.observed[0]?.tools.map((tool) => tool.name)).toEqual(["one", "two", "three"]);
  });

  it("records NOTHING when a later page fails — a half list would retire the tools it never read", async () => {
    let calls = 0;
    boot({
      [UPSTREAM_URL]: async (req) => {
        const body = (await req.json()) as Record<string, unknown>;
        if (body["method"] === "initialize") {
          return Response.json({
            jsonrpc: "2.0",
            id: body["id"],
            result: { protocolVersion: "2025-06-18", capabilities: {}, serverInfo: { name: "x" } },
          });
        }
        calls += 1;
        if (calls === 1) {
          return Response.json({
            jsonrpc: "2.0",
            id: body["id"],
            result: { tools: [{ name: "one" }], nextCursor: "page-2" },
          });
        }
        return new Response("gone", { status: 500 });
      },
    });
    store.setServer(WS, resolvedServer({ authMode: "none" }));

    expect(
      await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER }),
    ).toEqual({ kind: "unreachable" });
    expect(store.observed).toEqual([]);
  });
});

describe("what a probe cannot do", () => {
  it("leaves the list untouched when the upstream refuses, and never throws", async () => {
    boot({ [UPSTREAM_URL]: () => new Response("no", { status: 500 }) });
    store.setServer(WS, resolvedServer({ authMode: "none" }));

    expect(
      await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER }),
    ).toEqual({ kind: "unreachable" });
    expect(store.observed).toEqual([]);
    expect(usage.events).toEqual([]);
  });

  it("says so when the address answers nothing at all", async () => {
    boot({});
    store.setServer(WS, resolvedServer({ authMode: "none", url: "https://nowhere.test/mcp" }));

    expect(
      (await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER })).kind,
    ).toBe("unreachable");
  });

  it("says a server needing a sign-in that nobody connected is not probeable", async () => {
    const upstream = legacyFake("2025-06-18");
    boot({ [UPSTREAM_URL]: upstream.handler });
    store.setServer(WS, resolvedServer());

    expect(
      await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER }),
    ).toEqual({ kind: "no_credential" });
    expect(upstream.calls).toEqual([]);
  });

  it("says so when the workspace has no connection to that server", async () => {
    boot({});
    expect(
      await probeServerTools(t.ctx, { workspaceId: WS, serverId: "srv_nobody", userId: USER }),
    ).toEqual({ kind: "not_connected" });
  });
});

describe("what a probe leaves behind", () => {
  it("keeps the era verdict but not its own conversation", async () => {
    const upstream = legacyFake("2025-06-18");
    boot({ [UPSTREAM_URL]: upstream.handler });
    store.setServer(WS, resolvedServer({ authMode: "none" }));

    await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER });

    // The verdict is shared knowledge about the server — the next real call is faster for it.
    expect(memory.verdict(SERVER_ID)?.version).toBe("2025-06-18");
    // The conversation is the probe's own, and it is gone.
    expect(memory.upstream(PROBE_SESSION, SERVER_ID, t.ctx.env.now())).toBeNull();
  });

  it("does not leave a 2024-11-05 SSE pump running", async () => {
    const base = "https://old.test";
    const upstream = fake2411(base);
    boot({ [base]: upstream.handler });
    store.setServer(WS, resolvedServer({ authMode: "none", url: `${base}/sse` }));

    expect(
      (await probeServerTools(t.ctx, { workspaceId: WS, serverId: SERVER_ID, userId: USER })).kind,
    ).toBe("recorded");
    expect(memory.upstream(PROBE_SESSION, SERVER_ID, t.ctx.env.now())).toBeNull();
  });
});
