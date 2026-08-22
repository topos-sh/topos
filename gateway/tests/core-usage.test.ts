/**
 * Usage accounting: exactly one row per handled request, correct outcome vocabulary, timing from
 * the injected clock, and NEVER any arguments, results, or header values in a row or a log line.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { resetEngineMemoryForTests } from "../core/state";
import {
  BEARER,
  FakeStore,
  FakeUsage,
  initLegacySession,
  legacyFake,
  makeCtx,
  nextId,
  oauthCredential,
  resolvedServer,
  rpc,
  sessionRef,
  SERVER_ID,
  TOPOS_SESSION,
  UPSTREAM_URL,
  USER,
  WS,
  type FetchHandler,
  type TestCtx,
} from "./core-helpers";

describe("usage accounting", () => {
  let store: FakeStore;
  let usage: FakeUsage;
  let routes: Record<string, FetchHandler>;
  let t: TestCtx;
  let sid: string;

  beforeEach(async () => {
    store = new FakeStore();
    usage = new FakeUsage();
    const upstream = legacyFake("2025-06-18", { secret: "up-secret-1" });
    routes = { [UPSTREAM_URL]: upstream.handler };
    t = makeCtx(store, usage, routes);
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer());
    store.setCredential(WS, SERVER_ID, null, oauthCredential({ refreshToken: undefined, tokenEndpoint: undefined, clientId: undefined }));
    sid = await initLegacySession(t, "2025-06-18");
    usage.events.length = 0;
  });
  afterEach(() => resetEngineMemoryForTests());

  const headers = () => ({ "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" });
  const call = (name: string, args: Record<string, unknown> = {}) =>
    rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/call", params: { name, arguments: args } }, headers());

  it("records exactly one ok row for a successful tools/call, fully attributed", async () => {
    await call("issue_create");
    expect(usage.events).toHaveLength(1);
    const row = usage.events[0];
    expect(row).toMatchObject({
      workspaceId: WS,
      serverId: SERVER_ID,
      sessionId: TOPOS_SESSION,
      userId: USER,
      toolName: "issue_create",
      method: "tools/call",
      outcome: "ok",
    });
    expect(row?.durationMs).toBeGreaterThan(0);
  });

  it("records upstream_error when the upstream answers garbage", async () => {
    routes[UPSTREAM_URL] = () => new Response("boom", { status: 500 });
    const resp = await call("issue_create");
    expect(resp.status).toBe(502);
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("upstream_error");
    expect(usage.events[0]?.toolName).toBe("issue_create");
  });

  it("records unauthorized when the upstream rejects the credential and no refresh is possible", async () => {
    routes[UPSTREAM_URL] = () => new Response(null, { status: 401 });
    const resp = await call("issue_create");
    expect(resp.status).toBe(401);
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("unauthorized");
  });

  it("records denied_tool without any upstream contact", async () => {
    store.setPolicy(WS, SERVER_ID, { mode: "selected", selected: new Set() });
    routes[UPSTREAM_URL] = () => {
      throw new Error("the upstream must not be touched");
    };
    await call("issue_create");
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("denied_tool");
  });

  it("records no_credential when the sign-in is missing", async () => {
    store.clearCredentials();
    await call("issue_create");
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("no_credential");
  });

  it("records one row each for initialize, notifications, GET streams, and DELETE", async () => {
    await rpc(t, { jsonrpc: "2.0", method: "notifications/initialized" }, headers());
    expect(usage.events).toHaveLength(1);
    const get = await rpc(t, undefined as never, headers(), { method: "GET" });
    expect(usage.events).toHaveLength(2);
    await (get.body as ReadableStream<Uint8Array>).cancel();
    await rpc(t, undefined as never, headers(), { method: "DELETE" });
    expect(usage.events).toHaveLength(3);
    expect(usage.events.every((e) => e.outcome === "ok")).toBe(true);
  });

  it("keeps toolName null for non-tool methods", async () => {
    await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, headers());
    expect(usage.events[0]?.toolName).toBeNull();
    expect(usage.events[0]?.method).toBe("tools/list");
  });

  it("carries no arguments, results, or header values in rows or logs", async () => {
    await call("issue_create", { password: "hunter2-super-secret", note: "payload-text" });
    const flatRows = JSON.stringify(usage.events);
    const flatLogs = JSON.stringify(t.logs);
    for (const needle of ["hunter2-super-secret", "payload-text", "up-secret-1", BEARER, sid]) {
      expect(flatRows).not.toContain(needle);
      expect(flatLogs).not.toContain(needle);
    }
    // The row shape is exactly the UsageEvent contract — no extras smuggled in.
    expect(Object.keys(usage.events[0] ?? {}).sort()).toEqual(
      ["durationMs", "method", "outcome", "serverId", "sessionId", "toolName", "userId", "workspaceId"].sort(),
    );
  });

  it("takes durations from the injected clock, never the wall clock", async () => {
    await call("issue_create");
    const d = usage.events[0]?.durationMs ?? 0;
    expect(d % 7).toBe(0); // The fake clock ticks in 7ms steps; real time would not.
    expect(d).toBeGreaterThan(0);
  });
});
