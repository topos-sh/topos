/**
 * Residency: what a long-lived process is allowed to keep, and what it must let go of.
 *
 * Two halves. The first drives the facade directly, because the interesting cases — ten thousand
 * client sessions, an entry idle for nine hours — are not reachable through a request in a test.
 * EVERY time in this half is a plain number starting at zero: nothing here reads a wall clock, so
 * a `Date.now()` that crept into `core/` would make this arithmetic nonsense and the suite red.
 * The second half proves the same rules through the engine, where the sockets are real.
 */
import { afterEach, describe, expect, it } from "vitest";
import {
  memory,
  resetEngineMemoryForTests,
  RESIDENCY_FOR_TESTS,
  type ClientSession,
  type LegacySseChannel,
  type Sse2024Handle,
  type UpstreamSession,
} from "../core/state";
import {
  BEARER,
  FakeStore,
  FakeUsage,
  fake2411,
  initLegacySession,
  legacyFake,
  makeCtx,
  nextId,
  open2411Client,
  oauthCredential,
  resolvedServer,
  rpc,
  sessionRef,
  SERVER_ID,
  TOPOS_SESSION,
  UPSTREAM_URL,
  WS,
  type FetchHandler,
  type TestCtx,
} from "./core-helpers";

const TTL = RESIDENCY_FOR_TESTS.idleTtlMs;
const CAP = RESIDENCY_FOR_TESTS.maxClientSessions;

/** A 2024-11-05 upstream handle: what it holds that matters here is a socket and its abort. */
function pump(): Sse2024Handle {
  return { endpointUrl: "https://up.test/messages", pending: new Map(), notify: null, abort: new AbortController() };
}

function upstreamOf(handle: Sse2024Handle): UpstreamSession {
  return { version: "2024-11-05", mcpSessionId: null, initialized: true, sse2024: handle, credentialId: null };
}

function session(mcpSessionId: string, toposSessionId = "sn_laptop", serverId = "srv1"): ClientSession {
  return {
    mcpSessionId,
    toposSessionId,
    serverId,
    clientVersion: "2025-06-18",
    clientCapabilities: {},
    clientInfo: undefined,
    initialized: true,
  };
}

function channel(token: string, toposSessionId = "sn_laptop", serverId = "srv1"): LegacySseChannel {
  return {
    token,
    toposSessionId,
    serverId,
    writer: { send: () => {}, close: () => {}, closed: false },
    clientCapabilities: {},
    clientInfo: undefined,
    initialized: false,
    detachUpstream: null,
  };
}

/**
 * Push the map exactly one entry past its cap with sessions of unrelated pairs, so precisely one
 * eviction happens and which entry it was is the thing under test. `standing` is how many sessions
 * the case already put there.
 */
function fillPastTheCap(standing: number, now: number): void {
  for (let i = 0; i < CAP + 1 - standing; i += 1) {
    memory.putClientSession(session(`filler-${i}`, `other-sess-${i}`), now);
  }
}

afterEach(() => resetEngineMemoryForTests());

describe("evicting a client session past the cap", () => {
  it("releases the conversation it was the last reference to, aborting its pump", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);
    memory.putClientSession(session("mcp-1"), 0);

    fillPastTheCap(1, 0);

    expect(memory.clientSession("mcp-1", 0)).toBeNull();
    expect(memory.upstream("sn_laptop", "srv1", 0)).toBeNull();
    expect(handle.abort.signal.aborted).toBe(true);
  });

  it("keeps a conversation another client session of the same pair is still using", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);
    memory.putClientSession(session("mcp-1"), 0);
    // A second session of the SAME machine against the same server — a client that re-initialized
    // without a DELETE, which is the ordinary way two of these exist at once.
    memory.putClientSession(session("mcp-2"), 1);

    fillPastTheCap(2, 1);

    expect(memory.clientSession("mcp-1", 1)).toBeNull(); // The oldest still fell out...
    expect(memory.clientSession("mcp-2", 1)).not.toBeNull();
    expect(memory.upstream("sn_laptop", "srv1", 1)).not.toBeNull(); // ...but it took nothing with it.
    expect(handle.abort.signal.aborted).toBe(false);
  });

  it("keeps a conversation an open 2024-11-05 channel is still using", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);
    memory.putClientSession(session("mcp-1"), 0);
    memory.putLegacyChannel(channel("tok-1"));

    fillPastTheCap(1, 0);

    expect(memory.clientSession("mcp-1", 0)).toBeNull();
    expect(memory.upstream("sn_laptop", "srv1", 0)).not.toBeNull();
    expect(handle.abort.signal.aborted).toBe(false);
  });

  it("evicts by last USE, not by age", () => {
    memory.putClientSession(session("mcp-old-but-busy", "sess-a"), 0);
    memory.putClientSession(session("mcp-new-but-idle", "sess-b"), 1);
    memory.clientSession("mcp-old-but-busy", 2); // One touch is the whole difference.

    fillPastTheCap(2, 2);

    expect(memory.clientSession("mcp-old-but-busy", 2)).not.toBeNull();
    expect(memory.clientSession("mcp-new-but-idle", 2)).toBeNull();
  });
});

describe("the idle TTL", () => {
  it("sweeps an idle conversation on the next insert, aborting its pump", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);

    // Nothing touches it for the whole window; the next establishment is what notices.
    memory.setUpstream("sess2", "srv1", upstreamOf(pump()), TTL);

    expect(memory.upstream("sn_laptop", "srv1", TTL)).toBeNull();
    expect(handle.abort.signal.aborted).toBe(true);
    expect(memory.upstream("sess2", "srv1", TTL)).not.toBeNull();
  });

  it("leaves a recently used conversation alone", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);

    memory.setUpstream("sess2", "srv1", upstreamOf(pump()), TTL - 1);

    expect(memory.upstream("sn_laptop", "srv1", TTL - 1)).not.toBeNull();
    expect(handle.abort.signal.aborted).toBe(false);
  });

  it("restamps a conversation on every access, so use keeps it resident indefinitely", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);

    // A call every seven hours, for a week: never idle for a whole window, never swept.
    for (let at = TTL - 3_600_000; at < TTL * 21; at += TTL - 3_600_000) {
      expect(memory.upstream("sn_laptop", "srv1", at)).not.toBeNull();
      memory.setUpstream("sess2", "srv1", upstreamOf(pump()), at); // Something else inserts, sweeping.
    }
    expect(handle.abort.signal.aborted).toBe(false);
  });

  it("counts a touch on the client session as a touch on the conversation it speaks for", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);
    memory.putClientSession(session("mcp-1"), 0);

    // The client is busy on its session without reaching upstream — a notification, a local ping.
    memory.clientSession("mcp-1", TTL - 1);
    memory.setUpstream("sess2", "srv1", upstreamOf(pump()), TTL + 1);

    expect(memory.upstream("sn_laptop", "srv1", TTL + 1)).not.toBeNull();
    expect(handle.abort.signal.aborted).toBe(false);
  });

  it("counts a touch on an open 2024-11-05 channel the same way", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);
    memory.putLegacyChannel(channel("tok-1"));

    memory.legacyChannel("tok-1", TTL - 1);
    memory.setUpstream("sess2", "srv1", upstreamOf(pump()), TTL + 1);

    expect(memory.upstream("sn_laptop", "srv1", TTL + 1)).not.toBeNull();
  });

  it("sweeps an idle client session through the reference-counted teardown", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);
    memory.putClientSession(session("mcp-1"), 0);

    memory.putClientSession(session("mcp-2", "other-sess"), TTL);

    expect(memory.clientSession("mcp-1", TTL)).toBeNull();
    expect(memory.upstream("sn_laptop", "srv1", TTL)).toBeNull();
    expect(handle.abort.signal.aborted).toBe(true);
  });

  it("restamps a client session on access, so a busy one is never swept", () => {
    memory.putClientSession(session("mcp-1"), 0);
    memory.clientSession("mcp-1", TTL - 1);

    memory.putClientSession(session("mcp-2", "other-sess"), TTL + 1);

    expect(memory.clientSession("mcp-1", TTL + 1)).not.toBeNull();
  });
});

describe("the reference rule", () => {
  it("answers a DELETE that was the last reference with a release", () => {
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(pump()), 0);
    memory.putClientSession(session("mcp-1"), 0);
    expect(memory.dropClientSession("mcp-1")).toBe(true);
  });

  it("refuses to release while a sibling session, a channel, or a listen stream stands", () => {
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(pump()), 0);

    memory.putClientSession(session("mcp-1"), 0);
    memory.putClientSession(session("mcp-2"), 0);
    expect(memory.dropClientSession("mcp-1")).toBe(false);

    memory.putLegacyChannel(channel("tok-1"));
    expect(memory.dropClientSession("mcp-2")).toBe(false);
    memory.dropLegacyChannel("tok-1");

    memory.putClientSession(session("mcp-3"), 0);
    const removeListen = memory.addListen("sn_laptop", "srv1", { tools: true, send: () => {} });
    expect(memory.dropClientSession("mcp-3")).toBe(false);
    removeListen();

    memory.putClientSession(session("mcp-4"), 0);
    expect(memory.dropClientSession("mcp-4")).toBe(true);
  });

  it("releases the conversation when the last 2024-11-05 channel closes", () => {
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);
    memory.putLegacyChannel(channel("tok-1"));
    memory.putLegacyChannel(channel("tok-2"));

    memory.dropLegacyChannel("tok-1");
    expect(handle.abort.signal.aborted).toBe(false); // tok-2 is still on this conversation.

    memory.dropLegacyChannel("tok-2");
    expect(memory.upstream("sn_laptop", "srv1", 0)).toBeNull();
    expect(handle.abort.signal.aborted).toBe(true);
  });

  it("still tears down unconditionally when the conversation is INVALID, not merely unused", () => {
    // A changed credential or a dead upstream session makes the conversation useless to every
    // client that shares it — `dropUpstream` is never reference-counted.
    const handle = pump();
    memory.setUpstream("sn_laptop", "srv1", upstreamOf(handle), 0);
    memory.putClientSession(session("mcp-1"), 0);
    memory.putLegacyChannel(channel("tok-1"));

    memory.dropUpstream("sn_laptop", "srv1");

    expect(memory.upstream("sn_laptop", "srv1", 0)).toBeNull();
    expect(handle.abort.signal.aborted).toBe(true);
  });
});

// -- the same rules, through the engine, with real streams -------------------------------------

describe("residency through the engine", () => {
  let store: FakeStore;
  let usage: FakeUsage;
  let routes: Record<string, FetchHandler>;
  let t: TestCtx;

  const boot = (r: Record<string, FetchHandler>): void => {
    store = new FakeStore();
    usage = new FakeUsage();
    routes = r;
    t = makeCtx(store, usage, routes);
  };

  it("leaves a healthy client's session and conversation exactly where they were", async () => {
    boot({ [UPSTREAM_URL]: legacyFake("2025-06-18", { secret: "up-secret-1" }).handler });
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer());
    store.setCredential(WS, SERVER_ID, null, oauthCredential({ refreshToken: undefined, tokenEndpoint: undefined, clientId: undefined }));
    const sid = await initLegacySession(t, "2025-06-18");

    const headers = { "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" };
    for (let i = 0; i < 5; i += 1) {
      const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, headers);
      expect(resp.status).toBe(200);
    }
    expect(memory.clientSession(sid, t.ctx.env.now())).not.toBeNull();
    expect(memory.upstream(TOPOS_SESSION, SERVER_ID, t.ctx.env.now())).not.toBeNull();
  });

  it("keeps the conversation when one of two sessions of a machine sends DELETE", async () => {
    boot({ [UPSTREAM_URL]: legacyFake("2025-06-18", { secret: "up-secret-1" }).handler });
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer());
    store.setCredential(WS, SERVER_ID, null, oauthCredential({ refreshToken: undefined, tokenEndpoint: undefined, clientId: undefined }));
    const first = await initLegacySession(t, "2025-06-18");
    const second = await initLegacySession(t, "2025-06-18");

    const gone = await rpc(t, undefined as never, { "mcp-session-id": first }, { method: "DELETE" });
    expect(gone.status).toBe(200);

    // The second client is mid-conversation on the same pair: its session and the upstream stand.
    expect(memory.clientSession(first, t.ctx.env.now())).toBeNull();
    expect(memory.clientSession(second, t.ctx.env.now())).not.toBeNull();
    expect(memory.upstream(TOPOS_SESSION, SERVER_ID, t.ctx.env.now())).not.toBeNull();

    const still = await rpc(
      t,
      { jsonrpc: "2.0", id: nextId(), method: "tools/list" },
      { "mcp-session-id": second, "mcp-protocol-version": "2025-06-18" },
    );
    expect(still.status).toBe(200);
  });

  it("drops the upstream on the last session's DELETE, as it always has", async () => {
    boot({ [UPSTREAM_URL]: legacyFake("2025-06-18", { secret: "up-secret-1" }).handler });
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer());
    store.setCredential(WS, SERVER_ID, null, oauthCredential({ refreshToken: undefined, tokenEndpoint: undefined, clientId: undefined }));
    const only = await initLegacySession(t, "2025-06-18");

    await rpc(t, undefined as never, { "mcp-session-id": only }, { method: "DELETE" });

    expect(memory.upstream(TOPOS_SESSION, SERVER_ID, t.ctx.env.now())).toBeNull();
  });

  it("aborts a real 2024-11-05 pump when its client's stream ends", async () => {
    const base = "https://old.test";
    const upstream = fake2411(base);
    boot({ [base]: upstream.handler });
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer({ authMode: "none", url: `${base}/sse` }));

    const client = await open2411Client(t);
    await client.post({
      jsonrpc: "2.0",
      id: nextId(),
      method: "initialize",
      params: { protocolVersion: "2024-11-05", capabilities: {}, clientInfo: { name: "old", version: "1" } },
    });
    await client.nextMessage();

    const live = memory.upstream(TOPOS_SESSION, SERVER_ID, t.ctx.env.now());
    expect(live?.sse2024).not.toBeNull();
    const aborted = live?.sse2024?.abort.signal;

    await client.cancel(); // The client goes away; this transport has no DELETE to send.

    expect(memory.upstream(TOPOS_SESSION, SERVER_ID, t.ctx.env.now())).toBeNull();
    expect(aborted?.aborted).toBe(true);
  });
});
