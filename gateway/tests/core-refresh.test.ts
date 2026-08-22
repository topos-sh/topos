/**
 * OAuth refresh: at most one refresh per gateway request, one retry, serialization across
 * concurrent requests through the store's refresh lock, and clean 401 passthrough on failure.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { resetEngineMemoryForTests } from "../core/state";
import {
  BEARER,
  FakeStore,
  FakeUsage,
  bodyJson,
  makeCtx,
  modernBody,
  modernFake,
  modernHeaders,
  oauthCredential,
  resolvedServer,
  rpc,
  sessionRef,
  tokenEndpoint,
  SERVER_ID,
  UPSTREAM_URL,
  WS,
  type TestCtx,
} from "./core-helpers";

describe("oauth refresh", () => {
  let store: FakeStore;
  let usage: FakeUsage;
  // Mutable auth expectation: flipping `secret` invalidates the credential upstream-side.
  let upstreamAuth: { secret: string };

  beforeEach(() => {
    store = new FakeStore();
    usage = new FakeUsage();
    upstreamAuth = { secret: "up-secret-1" };
  });
  afterEach(() => resetEngineMemoryForTests());

  async function seeded(tokenFail = false): Promise<{ t: TestCtx; grants: () => number; bodies: () => string[]; upstream: ReturnType<typeof modernFake> }> {
    const upstream = modernFake(upstreamAuth);
    const token = tokenEndpoint("up-secret-2", { fail: tokenFail });
    const t = makeCtx(store, usage, {
      [UPSTREAM_URL]: upstream.handler,
      "https://auth.test/token": token.handler,
    });
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer());
    store.setCredential(WS, SERVER_ID, null, oauthCredential());
    return { t, grants: () => token.state.grants, bodies: () => token.state.bodies, upstream };
  }

  async function warmUp(t: TestCtx): Promise<void> {
    const resp = await rpc(t, modernBody("tools/list"), modernHeaders("tools/list"));
    expect(resp.status).toBe(200);
    usage.events.length = 0;
  }

  const call = (t: TestCtx) =>
    rpc(t, modernBody("tools/call", { name: "issue_create", arguments: {} }), modernHeaders("tools/call", "issue_create"));

  it("refreshes once on an upstream 401 and retries once, with the rotated secret stored", async () => {
    const { t, grants, bodies, upstream } = await seeded();
    await warmUp(t);
    upstreamAuth.secret = "up-secret-2"; // The old token is now dead upstream.
    const resp = await call(t);
    expect(resp.status).toBe(200);
    const body = await bodyJson(resp);
    expect(body["result"]).toBeDefined();
    expect(grants()).toBe(1);
    expect(bodies()[0]).toContain("grant_type=refresh_token");
    expect(bodies()[0]).toContain("refresh_token=refresh-1");
    expect(bodies()[0]).toContain("client_id=client-1");
    expect(store.rotations).toEqual([{ id: "cred_1", secret: "up-secret-2", refreshToken: "refresh-2" }]);
    expect(upstream.toolCallCount).toBe(1);
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("ok");
  });

  it("passes the 401 through with outcome unauthorized when the refresh fails", async () => {
    const { t, grants } = await seeded(true);
    await warmUp(t);
    upstreamAuth.secret = "up-secret-2";
    const resp = await call(t);
    expect(resp.status).toBe(401);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["message"]).toBe("The upstream server rejected the gateway's sign-in.");
    expect(grants()).toBe(1);
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("unauthorized");
    expect(store.rotations).toHaveLength(0);
  });

  it("refreshes at most once per request even when the retry is rejected too", async () => {
    const { t, grants } = await seeded();
    await warmUp(t);
    upstreamAuth.secret = "never-matches"; // The token endpoint's rotation still won't satisfy it.
    const resp = await call(t);
    expect(resp.status).toBe(401);
    expect(grants()).toBe(1);
    expect(usage.events[0]?.outcome).toBe("unauthorized");
  });

  it("never touches the token endpoint for a non-refreshable (manual) credential", async () => {
    const { t, grants } = await seeded();
    store.setCredential(WS, SERVER_ID, null, { id: "cred_m", kind: "manual", secret: "up-secret-1" });
    await warmUp(t);
    upstreamAuth.secret = "up-secret-2";
    const resp = await call(t);
    expect(resp.status).toBe(401);
    expect(grants()).toBe(0);
    expect(usage.events[0]?.outcome).toBe("unauthorized");
  });

  it("serializes concurrent refreshes: two racing calls, ONE token grant, both succeed", async () => {
    const { t, grants, upstream } = await seeded();
    await warmUp(t);
    upstreamAuth.secret = "up-secret-2";
    const [a, b] = await Promise.all([call(t), call(t)]);
    expect(a.status).toBe(200);
    expect(b.status).toBe(200);
    expect(grants()).toBe(1); // The loser of the lock race adopts the rotated secret instead.
    expect(store.lockAcquisitions.filter((id) => id === "cred_1").length).toBeGreaterThanOrEqual(1);
    expect(upstream.toolCallCount).toBe(2);
    expect(usage.events.map((e) => e.outcome)).toEqual(["ok", "ok"]);
  });

  it("keeps refreshed material out of every log line", async () => {
    const { t } = await seeded();
    await warmUp(t);
    upstreamAuth.secret = "up-secret-2";
    await call(t);
    const flat = JSON.stringify(t.logs);
    expect(flat).not.toContain("up-secret-1");
    expect(flat).not.toContain("up-secret-2");
    expect(flat).not.toContain("refresh-1");
    expect(flat).not.toContain("refresh-2");
  });
});
