import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { resetEngineMemoryForTests } from "../core/state";
import {
  BEARER,
  FakeStore,
  FakeUsage,
  bodyJson,
  initLegacySession,
  legacyFake,
  makeCtx,
  nextId,
  resolvedServer,
  rpc,
  sessionRef,
  SERVER_ID,
  UPSTREAM_URL,
  WS,
  type LegacyFake,
  type TestCtx,
} from "./core-helpers";

describe("tool policy", () => {
  let t: TestCtx;
  let store: FakeStore;
  let usage: FakeUsage;
  let upstream: LegacyFake;
  let sid: string;

  beforeEach(async () => {
    store = new FakeStore();
    usage = new FakeUsage();
    upstream = legacyFake("2025-06-18");
    t = makeCtx(store, usage, { [UPSTREAM_URL]: upstream.handler });
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer({ authMode: "none" }));
    sid = await initLegacySession(t, "2025-06-18");
    usage.events.length = 0;
  });
  afterEach(() => resetEngineMemoryForTests());

  const listReq = () => ({ jsonrpc: "2.0", id: nextId(), method: "tools/list" });
  const callReq = (name: string) => ({ jsonrpc: "2.0", id: nextId(), method: "tools/call", params: { name, arguments: { a: 1 } } });
  const withSession = () => ({ "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" });

  it("passes the full tools/list through under mode all, and observes every tool", async () => {
    const resp = await rpc(t, listReq(), withSession());
    expect(resp.status).toBe(200);
    const body = await bodyJson(resp);
    const tools = (body["result"] as Record<string, unknown>)["tools"] as { name: string }[];
    expect(tools.map((x) => x.name)).toEqual(["issue_create", "issue_delete", "issue_list"]);
    expect(store.observed).toHaveLength(1);
    expect(store.observed[0]?.tools.map((x) => x.name)).toEqual(["issue_create", "issue_delete", "issue_list"]);
    expect(store.observed[0]?.tools[0]?.description).toBe("Create an issue");
  });

  it("filters tools/list to the enabled set under mode selected, but observes UNfiltered", async () => {
    store.setPolicy(WS, SERVER_ID, { mode: "selected", selected: new Set(["issue_list"]) });
    const resp = await rpc(t, listReq(), withSession());
    const body = await bodyJson(resp);
    const tools = (body["result"] as Record<string, unknown>)["tools"] as { name: string }[];
    expect(tools.map((x) => x.name)).toEqual(["issue_list"]);
    // Observation is the truth about the server, not the policy.
    expect(store.observed[0]?.tools.map((x) => x.name)).toEqual(["issue_create", "issue_delete", "issue_list"]);
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("ok");
    expect(usage.events[0]?.toolName).toBeNull();
  });

  it("lets an enabled tool through under mode selected", async () => {
    store.setPolicy(WS, SERVER_ID, { mode: "selected", selected: new Set(["issue_list"]) });
    const resp = await rpc(t, callReq("issue_list"), withSession());
    const body = await bodyJson(resp);
    expect(body["result"]).toBeDefined();
    expect(upstream.toolCallCount).toBe(1);
    expect(usage.events[0]?.outcome).toBe("ok");
    expect(usage.events[0]?.toolName).toBe("issue_list");
  });

  it("denies a non-enabled tool with the exact copy; the upstream is never called", async () => {
    store.setPolicy(WS, SERVER_ID, { mode: "selected", selected: new Set(["issue_list"]) });
    const resp = await rpc(t, callReq("issue_delete"), withSession());
    expect(resp.status).toBe(200);
    const body = await bodyJson(resp);
    const error = body["error"] as Record<string, unknown>;
    expect(error["message"]).toBe("The tool issue_delete is not enabled for this workspace.");
    expect(error["code"]).toBe(-32602);
    expect(upstream.toolCallCount).toBe(0);
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("denied_tool");
    expect(usage.events[0]?.toolName).toBe("issue_delete");
    expect(usage.events[0]?.method).toBe("tools/call");
  });

  it("denies even a tool the server does not offer (policy speaks before upstream)", async () => {
    store.setPolicy(WS, SERVER_ID, { mode: "selected", selected: new Set() });
    const resp = await rpc(t, callReq("made_up_tool"), withSession());
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["message"]).toBe("The tool made_up_tool is not enabled for this workspace.");
    expect(upstream.toolCallCount).toBe(0);
  });

  it("re-reads the policy every request: a row flip takes effect on the next call", async () => {
    const ok = await rpc(t, callReq("issue_create"), withSession());
    expect((await bodyJson(ok))["result"]).toBeDefined();
    store.setPolicy(WS, SERVER_ID, { mode: "selected", selected: new Set(["issue_list"]) });
    const denied = await rpc(t, callReq("issue_create"), withSession());
    expect(((await bodyJson(denied))["error"] as Record<string, unknown>)["message"]).toBe(
      "The tool issue_create is not enabled for this workspace.",
    );
    expect(upstream.toolCallCount).toBe(1);
  });

  it("fails closed on a tools/call without a tool name", async () => {
    const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/call", params: {} }, withSession());
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["code"]).toBe(-32602);
    expect(upstream.toolCallCount).toBe(0);
  });
});
