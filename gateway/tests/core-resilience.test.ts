/**
 * Recovery behavior: dead upstream sessions re-initialize once and retry; a stale era verdict is
 * dropped on failure and re-probed on the next call; the 2024-11-05 channels carry notifications
 * in both directions.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { memory, resetEngineMemoryForTests } from "../core/state";
import {
  BEARER,
  FakeStore,
  FakeUsage,
  bodyJson,
  eventReader,
  fake2411,
  initLegacySession,
  legacyFake,
  makeCtx,
  modernFake,
  nextId,
  open2411Client,
  resolvedServer,
  rpc,
  sessionRef,
  SERVER_ID,
  UPSTREAM_URL,
  WS,
  type FetchHandler,
  type TestCtx,
} from "./core-helpers";

describe("recovery and the 2024-11-05 channels", () => {
  let store: FakeStore;
  let usage: FakeUsage;

  beforeEach(() => {
    store = new FakeStore();
    usage = new FakeUsage();
  });
  afterEach(() => resetEngineMemoryForTests());

  async function seed(routes: Record<string, FetchHandler>): Promise<TestCtx> {
    const t = makeCtx(store, usage, routes);
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer({ authMode: "none" }));
    return t;
  }

  it("re-initializes ONCE on an upstream 404 (dead session) and retries the call", async () => {
    const upstream = legacyFake("2025-06-18");
    const t = await seed({ [UPSTREAM_URL]: upstream.handler });
    const sid = await initLegacySession(t, "2025-06-18");
    expect(upstream.initCount).toBe(1);
    upstream.dropSessions(); // The upstream expired our session behind our back.
    const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" });
    const body = await bodyJson(resp);
    expect(body["result"]).toBeDefined();
    expect(upstream.initCount).toBe(2); // Exactly one fresh handshake.
  });

  it("drops a stale era verdict on failure and re-probes on the next call", async () => {
    const modern = modernFake({ secret: null });
    const routes: Record<string, FetchHandler> = { [UPSTREAM_URL]: modern.handler };
    const t = await seed(routes);
    const sid = await initLegacySession(t, "2025-06-18"); // Verdict: modern.
    expect(memory.verdict(SERVER_ID)?.version).toBe("2026-07-28");
    // The server is redeployed as a legacy one at the same URL.
    const legacy = legacyFake("2025-06-18");
    routes[UPSTREAM_URL] = legacy.handler;
    const first = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" });
    expect(first.status).toBe(502); // The stale-modern call fails once...
    expect(memory.verdict(SERVER_ID)).toBeNull(); // ...and the verdict is gone.
    const second = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" });
    const body = await bodyJson(second);
    expect(body["result"]).toBeDefined(); // Re-probed, re-initialized, healthy again.
  });

  it("invalidates the cached verdict when the resolved upstream URL changes", async () => {
    const modern = modernFake({ secret: null });
    const t = await seed({ [UPSTREAM_URL]: modern.handler, "https://up2.test/mcp": legacyFake("2025-06-18").handler });
    await initLegacySession(t, "2025-06-18");
    expect(memory.verdict(SERVER_ID)?.version).toBe("2026-07-28");
    store.setServer(WS, resolvedServer({ authMode: "none", url: "https://up2.test/mcp" }));
    resetEngineMemoryForTests(); // New client session against the moved server.
    const t2 = makeCtx(store, usage, { "https://up2.test/mcp": legacyFake("2025-06-18").handler });
    await initLegacySession(t2, "2025-06-18");
    expect(memory.verdict(SERVER_ID)?.version).toBe("2025-06-18");
    expect(memory.verdict(SERVER_ID)?.url).toBe("https://up2.test/mcp");
  });

  it("delivers a 2024-11-05 upstream's list_changed to a legacy client's GET stream", async () => {
    const upstream = fake2411("https://up.test");
    const t = await seed({ "https://up.test": upstream.handler });
    const sid = await initLegacySession(t, "2025-03-26");
    const get = await rpc(t, undefined as never, { "mcp-session-id": sid }, { method: "GET" });
    expect(get.status).toBe(200);
    const reader = eventReader(get.body as ReadableStream<Uint8Array>);
    upstream.notify(JSON.stringify({ jsonrpc: "2.0", method: "notifications/tools/list_changed" }));
    const ev = await reader.next();
    expect(JSON.parse(ev?.data ?? "{}")["method"]).toBe("notifications/tools/list_changed");
    await reader.cancel();
  });

  it("delivers upstream notifications onto a 2024-11-05 client's message stream", async () => {
    const upstream = legacyFake("2025-06-18");
    const t = await seed({ [UPSTREAM_URL]: upstream.handler });
    const client = await open2411Client(t);
    await client.post({ jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2024-11-05", capabilities: {} } });
    const init = await client.nextMessage();
    expect(((init["result"] as Record<string, unknown>)["serverInfo"] as Record<string, unknown>)["name"]).toBe("upstream-2025-06-18");
    await vi.waitFor(() => expect(upstream.openGetStreams).toBe(1)); // The bridge subscribed upstream.
    upstream.notify(JSON.stringify({ jsonrpc: "2.0", method: "notifications/tools/list_changed" }));
    const ev = await client.nextMessage();
    expect(ev["method"]).toBe("notifications/tools/list_changed");
    await client.cancel();
  });

  it("keeps the 2024-11-05 endpoint POST bound to its own stream and workspace session", async () => {
    const upstream = legacyFake("2025-06-18");
    const t = await seed({ [UPSTREAM_URL]: upstream.handler });
    const client = await open2411Client(t);
    // A POST addressed to a token that is not an open stream is refused.
    const bad = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, {}, { url: `https://gw.test/sn_laptop/srv1?sse=forged-token` });
    expect(bad.status).toBe(404);
    await client.cancel();
  });

  it("refuses the 2024-11-05 endpoint stream of another topos session", async () => {
    const upstream = legacyFake("2025-06-18");
    const t = await seed({ [UPSTREAM_URL]: upstream.handler });
    await store.addBearer("other-bearer", { sessionId: "sn_desktop", workspaceId: WS, userId: "user2", displayName: "other" });
    const client = await open2411Client(t);
    // The other session tries to ride the first session's stream token.
    const resp = await rpc(
      t,
      { jsonrpc: "2.0", id: nextId(), method: "tools/list" },
      {},
      { url: client.endpoint.replace("/sn_laptop/", "/sn_desktop/"), bearer: "other-bearer", sessionId: "sn_desktop" },
    );
    expect(resp.status).toBe(404);
    await client.cancel();
  });
});
