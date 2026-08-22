import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { handleGatewayRequest } from "../core/engine";
import { resetEngineMemoryForTests } from "../core/state";
import { sha256Hex } from "../core/protocol";
import {
  BEARER,
  FakeStore,
  FakeUsage,
  bodyJson,
  gwRequest,
  legacyFake,
  makeCtx,
  resolvedServer,
  sessionRef,
  UPSTREAM_URL,
  type TestCtx,
} from "./core-helpers";

describe("auth shell", () => {
  let t: TestCtx;
  let store: FakeStore;
  let usage: FakeUsage;

  beforeEach(async () => {
    store = new FakeStore();
    usage = new FakeUsage();
    const upstream = legacyFake("2025-06-18");
    t = makeCtx(store, usage, { [UPSTREAM_URL]: upstream.handler });
    await store.addBearer(BEARER, sessionRef());
    store.setServer("ws1", resolvedServer({ authMode: "none" }));
  });
  afterEach(() => resetEngineMemoryForTests());

  const anyRpc = { jsonrpc: "2.0", id: 1, method: "tools/list" };

  it("refuses a request without a bearer: 401, JSON-RPC-shaped, no usage row", async () => {
    const resp = await handleGatewayRequest(t.ctx, gwRequest({ bearer: null, body: anyRpc }));
    expect(resp.status).toBe(401);
    const body = await bodyJson(resp);
    expect(body["jsonrpc"]).toBe("2.0");
    expect((body["error"] as Record<string, unknown>)["message"]).toBe("Unauthorized.");
    expect(usage.events).toHaveLength(0);
  });

  it("refuses an unknown bearer: 401 and no usage row (nothing to attribute)", async () => {
    const resp = await handleGatewayRequest(t.ctx, gwRequest({ bearer: "not-a-real-token", body: anyRpc }));
    expect(resp.status).toBe(401);
    expect(usage.events).toHaveLength(0);
    expect(t.logs.some((l) => l.level === "warn")).toBe(true);
  });

  it("hashes the raw bearer with sha256 before the store lookup", async () => {
    await handleGatewayRequest(t.ctx, gwRequest({ bearer: null, body: anyRpc }));
    expect(store.seenTokenHashes).toHaveLength(0); // No bearer, no lookup.
    await handleGatewayRequest(t.ctx, gwRequest({ body: anyRpc }));
    expect(store.seenTokenHashes[0]).toBe(await sha256Hex(BEARER));
    // The raw token never reached the store.
    expect(store.seenTokenHashes[0]).not.toContain(BEARER);
  });

  it("refuses a valid bearer under a foreign path session id: 401, outcome unauthorized", async () => {
    const resp = await handleGatewayRequest(t.ctx, gwRequest({ sessionId: "someone-elses-session", body: anyRpc }));
    expect(resp.status).toBe(401);
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("unauthorized");
  });

  it("answers 404 for a server the workspace is not connected to, outcome unauthorized", async () => {
    const resp = await handleGatewayRequest(t.ctx, gwRequest({ serverId: "srv-unknown", body: anyRpc }));
    expect(resp.status).toBe(404);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["message"]).toBe("Unknown server for this workspace.");
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("unauthorized");
    expect(usage.events[0]?.serverId).toBe("srv-unknown");
  });

  it("refuses any request carrying a browser Origin header with 403", async () => {
    const resp = await handleGatewayRequest(t.ctx, gwRequest({ body: anyRpc, headers: { origin: "https://evil.example" } }));
    expect(resp.status).toBe(403);
    expect(usage.events).toHaveLength(0);
  });

  it("fails closed on a non-JSON body: 400 parse error, one ok usage row", async () => {
    const req = gwRequest({ body: undefined });
    const raw = new Request(req.request.url, { method: "POST", body: "{nope" });
    const resp = await handleGatewayRequest(t.ctx, { ...req, request: raw });
    expect(resp.status).toBe(400);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["code"]).toBe(-32700);
    expect(usage.events).toHaveLength(1);
  });

  it("fails closed on a non-message JSON body", async () => {
    const resp = await handleGatewayRequest(t.ctx, gwRequest({ body: { hello: "world" } }));
    expect(resp.status).toBe(400);
    expect(usage.events).toHaveLength(1);
  });

  it("refuses a sessionless non-initialize request with 400", async () => {
    const resp = await handleGatewayRequest(t.ctx, gwRequest({ body: anyRpc }));
    expect(resp.status).toBe(400);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["message"]).toContain("Mcp-Session-Id");
  });
});
