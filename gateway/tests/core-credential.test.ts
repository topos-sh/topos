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
  oauthCredential,
  resolvedServer,
  rpc,
  sessionRef,
  SERVER_ID,
  UPSTREAM_URL,
  WS,
  type LegacyFake,
  type TestCtx,
} from "./core-helpers";

describe("credential attach", () => {
  let store: FakeStore;
  let usage: FakeUsage;

  beforeEach(() => {
    store = new FakeStore();
    usage = new FakeUsage();
  });
  afterEach(() => resetEngineMemoryForTests());

  async function seeded(upstream: LegacyFake, serverOverrides: Parameters<typeof resolvedServer>[0] = {}): Promise<TestCtx> {
    const t = makeCtx(store, usage, { [UPSTREAM_URL]: upstream.handler });
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer(serverOverrides));
    return t;
  }

  it("answers the pinned no-credential copy with HTTP 401, outcome no_credential, upstream untouched", async () => {
    const upstream = legacyFake("2025-06-18");
    const t = await seeded(upstream); // authMode oauth, no credential seeded.
    const resp = await rpc(t, {
      jsonrpc: "2.0",
      id: nextId(),
      method: "initialize",
      params: { protocolVersion: "2025-06-18", capabilities: {} },
    });
    expect(resp.status).toBe(401);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["message"]).toBe(
      "No sign-in is connected for Linear in Acme. Ask a member to connect one at https://topos.example/w/acme/mcp/srv1.",
    );
    expect(upstream.calls).toHaveLength(0);
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("no_credential");
  });

  it("proceeds with no auth header at all when authMode is none", async () => {
    const upstream = legacyFake("2025-06-18");
    const t = await seeded(upstream, { authMode: "none" });
    await initLegacySession(t, "2025-06-18");
    const init = upstream.calls.find((c) => c.method === "initialize");
    expect(init).toBeDefined();
    expect(init?.headers["authorization"]).toBeUndefined();
  });

  it("attaches Authorization: Bearer for a resolved credential", async () => {
    const upstream = legacyFake("2025-06-18", { secret: "up-secret-1" });
    const t = await seeded(upstream);
    store.setCredential(WS, SERVER_ID, null, oauthCredential());
    await initLegacySession(t, "2025-06-18");
    const init = upstream.calls.find((c) => c.method === "initialize");
    expect(init?.headers["authorization"]).toBe("Bearer up-secret-1");
  });

  it("uses the document's named secret header for manual credentials", async () => {
    const upstream = legacyFake("2025-06-18", { secret: "api-key-9", secretHeader: "x-api-key" });
    const t = await seeded(upstream, { authMode: "manual", secretHeader: "X-API-Key" });
    store.setCredential(WS, SERVER_ID, null, { id: "cred_m", kind: "manual", secret: "api-key-9" });
    await initLegacySession(t, "2025-06-18");
    const init = upstream.calls.find((c) => c.method === "initialize");
    expect(init?.headers["x-api-key"]).toBe("api-key-9");
    expect(init?.headers["authorization"]).toBeUndefined();
  });

  it("prefers the person's own credential over the workspace one", async () => {
    const upstream = legacyFake("2025-06-18");
    const t = await seeded(upstream);
    store.setCredential(WS, SERVER_ID, null, oauthCredential({ secret: "workspace-secret" }));
    store.setCredential(WS, SERVER_ID, "user1", oauthCredential({ id: "cred_p", secret: "personal-secret" }));
    await initLegacySession(t, "2025-06-18");
    const init = upstream.calls.find((c) => c.method === "initialize");
    expect(init?.headers["authorization"]).toBe("Bearer personal-secret");
  });

  it("never lets the client's standing bearer reach the upstream", async () => {
    const upstream = legacyFake("2025-06-18");
    const t = await seeded(upstream, { authMode: "none" });
    const sid = await initLegacySession(t, "2025-06-18");
    await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": sid });
    for (const call of upstream.calls) {
      for (const value of Object.values(call.headers)) {
        expect(value).not.toContain(BEARER);
      }
    }
  });

  it("never leaks upstream response headers back to the client", async () => {
    const upstream = legacyFake("2025-06-18", { secret: "up-secret-1" });
    const t = await seeded(upstream);
    store.setCredential(WS, SERVER_ID, null, oauthCredential());
    const sid = await initLegacySession(t, "2025-06-18");
    const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": sid });
    // The fake stamps x-upstream-internal on every JSON reply; the engine must rebuild headers.
    expect(resp.headers.get("x-upstream-internal")).toBeNull();
  });

  it("re-resolves the credential per call: a revoked row answers no_credential next call", async () => {
    const upstream = legacyFake("2025-06-18", { secret: "up-secret-1" });
    const t = await seeded(upstream);
    store.setCredential(WS, SERVER_ID, null, oauthCredential());
    const sid = await initLegacySession(t, "2025-06-18");
    store.clearCredentials();
    const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": sid });
    expect(resp.status).toBe(401);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["message"]).toContain("No sign-in is connected for Linear in Acme.");
  });

  it("keeps every log line free of secrets and tokens", async () => {
    const upstream = legacyFake("2025-06-18", { secret: "up-secret-1" });
    const t = await seeded(upstream);
    store.setCredential(WS, SERVER_ID, null, oauthCredential());
    const sid = await initLegacySession(t, "2025-06-18");
    await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": sid });
    const flat = JSON.stringify(t.logs);
    expect(flat).not.toContain("up-secret-1");
    expect(flat).not.toContain(BEARER);
    expect(flat).not.toContain("refresh-1");
  });
});
