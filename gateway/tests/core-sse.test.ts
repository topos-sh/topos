/**
 * Streaming: SSE replies are PIPED event-by-event (never buffered whole), framing survives,
 * filtering applies inside streams, server-initiated requests pass same-era and are absorbed
 * cross-era, and the notification channels bridge list_changed in both directions.
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { resetEngineMemoryForTests } from "../core/state";
import {
  BEARER,
  FakeStore,
  FakeUsage,
  bodyJson,
  eventReader,
  makeCtx,
  modernBody,
  modernFake,
  modernHeaders,
  nextId,
  pushStream,
  resolvedServer,
  rpc,
  sessionRef,
  SERVER_ID,
  UPSTREAM_URL,
  WS,
  type FetchHandler,
  type TestCtx,
} from "./core-helpers";

/** A 2025-06-18 upstream whose non-initialize replies are SSE streams the TEST drives. */
function streamingFake() {
  const sessions = new Set<string>();
  let n = 0;
  const api = {
    stream: null as ReturnType<typeof pushStream> | null,
    getStream: null as ReturnType<typeof pushStream> | null,
    received: [] as Record<string, unknown>[],
    handler: (async (req: Request) => {
      if (req.method === "GET") {
        const sid = req.headers.get("mcp-session-id");
        if (sid === null || !sessions.has(sid)) return new Response(null, { status: 400 });
        api.getStream = pushStream();
        return new Response(api.getStream.stream, { status: 200, headers: { "content-type": "text/event-stream" } });
      }
      if (req.method === "DELETE") return new Response(null, { status: 200 });
      const versionHeader = req.headers.get("mcp-protocol-version");
      if (versionHeader !== null && versionHeader !== "2025-06-18" && versionHeader !== "2025-03-26") {
        return new Response(null, { status: 400 });
      }
      const msg = (await req.json()) as Record<string, unknown>;
      if (typeof msg["method"] === "string" && (msg["id"] === undefined || msg["id"] === null)) {
        return new Response(null, { status: 202 }); // Notifications (initialized etc.) — uncounted.
      }
      if (typeof msg["method"] !== "string") {
        api.received.push(msg); // JSON-RPC responses relayed back by the gateway.
        return new Response(null, { status: 202 });
      }
      if (msg["method"] === "initialize") {
        const sid = `s${(n += 1)}`;
        sessions.add(sid);
        return new Response(
          JSON.stringify({
            jsonrpc: "2.0",
            id: msg["id"],
            result: {
              protocolVersion: "2025-06-18",
              capabilities: { tools: { listChanged: true } },
              serverInfo: { name: "streaming-upstream", version: "1" },
            },
          }),
          { status: 200, headers: { "content-type": "application/json", "mcp-session-id": sid } },
        );
      }
      api.stream = pushStream();
      return new Response(api.stream.stream, { status: 200, headers: { "content-type": "text/event-stream" } });
    }) as FetchHandler,
  };
  return api;
}

describe("SSE piping", () => {
  let store: FakeStore;
  let usage: FakeUsage;
  let up: ReturnType<typeof streamingFake>;
  let t: TestCtx;
  let sid: string;

  beforeEach(async () => {
    store = new FakeStore();
    usage = new FakeUsage();
    up = streamingFake();
    t = makeCtx(store, usage, { [UPSTREAM_URL]: up.handler });
    await store.addBearer(BEARER, sessionRef());
    store.setServer(WS, resolvedServer({ authMode: "none" }));
    const init = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: { sampling: {} } } });
    sid = init.headers.get("mcp-session-id") ?? "";
    expect(sid).not.toBe("");
    usage.events.length = 0;
  });
  afterEach(() => resetEngineMemoryForTests());

  const legacyHeaders = () => ({ "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" });

  it("pipes events as they arrive — the first event reaches the client before the stream ends", async () => {
    const resp = await rpc(t, { jsonrpc: "2.0", id: "r1", method: "tools/call", params: { name: "slow", arguments: {} } }, legacyHeaders());
    expect(resp.status).toBe(200);
    expect(resp.headers.get("content-type")).toContain("text/event-stream");
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", method: "notifications/progress", params: { progressToken: "p", progress: 1 } }));
    const first = await reader.next(); // Arrives while the upstream stream is still open.
    expect(first?.event).toBe("message");
    expect(JSON.parse(first?.data ?? "{}")["method"]).toBe("notifications/progress");
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", id: "r1", result: { content: [{ type: "text", text: "done" }] } }));
    const second = await reader.next();
    expect(JSON.parse(second?.data ?? "{}")["result"]).toBeDefined();
    up.stream?.close();
    expect(await reader.next()).toBeNull();
    expect(usage.events).toHaveLength(1);
    expect(usage.events[0]?.outcome).toBe("ok");
    expect(usage.events[0]?.toolName).toBe("slow");
  });

  it("preserves keep-alive comment lines through the pipe", async () => {
    const resp = await rpc(t, { jsonrpc: "2.0", id: "r1", method: "tools/call", params: { name: "slow", arguments: {} } }, legacyHeaders());
    const reader = (resp.body as ReadableStream<Uint8Array>).getReader();
    up.stream?.raw(": keep-alive\n\n");
    const { value } = await reader.read();
    expect(new TextDecoder().decode(value)).toContain(": keep-alive");
    up.stream?.close();
    await reader.cancel();
  });

  it("filters a tools/list result inside an SSE stream and observes the unfiltered set", async () => {
    store.setPolicy(WS, SERVER_ID, { mode: "selected", selected: new Set(["keep_me"]) });
    const resp = await rpc(t, { jsonrpc: "2.0", id: "r2", method: "tools/list" }, legacyHeaders());
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    up.stream?.event(
      "message",
      JSON.stringify({
        jsonrpc: "2.0",
        id: "r2",
        result: { tools: [{ name: "keep_me", inputSchema: {} }, { name: "drop_me", inputSchema: {} }, { name: "also_drop", inputSchema: {} }] },
      }),
    );
    const ev = await reader.next();
    const tools = (JSON.parse(ev?.data ?? "{}")["result"] as Record<string, unknown>)["tools"] as { name: string }[];
    expect(tools.map((x) => x.name)).toEqual(["keep_me"]);
    await vi.waitFor(() => expect(store.observed).toHaveLength(1));
    expect(store.observed[0]?.tools.map((x) => x.name)).toEqual(["keep_me", "drop_me", "also_drop"]);
    up.stream?.close();
  });

  it("passes a server-initiated request through same-era, and forwards the client's answer back", async () => {
    const resp = await rpc(t, { jsonrpc: "2.0", id: "r3", method: "tools/call", params: { name: "asks", arguments: {} } }, legacyHeaders());
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", id: "up-42", method: "sampling/createMessage", params: { messages: [] } }));
    const ev = await reader.next();
    const req = JSON.parse(ev?.data ?? "{}") as Record<string, unknown>;
    expect(req["method"]).toBe("sampling/createMessage");
    expect(req["id"]).toBe("up-42");
    // The client answers over POST; the gateway relays it upstream.
    const answer = await rpc(t, { jsonrpc: "2.0", id: "up-42", result: { role: "assistant", content: { type: "text", text: "hi" } } }, legacyHeaders());
    expect(answer.status).toBe(202);
    await vi.waitFor(() => expect(up.received.length).toBe(1));
    expect(up.received[0]?.["id"]).toBe("up-42");
    up.stream?.close();
  });

  it("absorbs a server-initiated request cross-era: the modern client never sees it, the upstream gets a clean refusal", async () => {
    const resp = await rpc(t, modernBody("tools/call", { name: "asks", arguments: {} }, "m1"), modernHeaders("tools/call", "asks"));
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", id: "up-9", method: "elicitation/create", params: {} }));
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", id: "m1", result: { content: [{ type: "text", text: "done" }] } }));
    const ev = await reader.next(); // The FIRST thing the modern client sees is its own result.
    const msg = JSON.parse(ev?.data ?? "{}") as Record<string, unknown>;
    expect(msg["id"]).toBe("m1");
    expect((msg["result"] as Record<string, unknown>)["resultType"]).toBe("complete");
    await vi.waitFor(() => expect(up.received.length).toBe(1));
    const refusal = up.received[0] as Record<string, unknown>;
    expect(refusal["id"]).toBe("up-9");
    expect((refusal["error"] as Record<string, unknown>)["message"]).toBe(
      "The method elicitation/create is not supported across protocol revisions.",
    );
    up.stream?.close();
  });

  it("answers an upstream ping itself — never surfaced to the client", async () => {
    const resp = await rpc(t, { jsonrpc: "2.0", id: "r4", method: "tools/call", params: { name: "x", arguments: {} } }, legacyHeaders());
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", id: "ping-1", method: "ping" }));
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", id: "r4", result: { content: [] } }));
    const ev = await reader.next();
    expect(JSON.parse(ev?.data ?? "{}")["id"]).toBe("r4"); // The ping was consumed.
    await vi.waitFor(() => expect(up.received.length).toBe(1));
    expect(up.received[0]?.["id"]).toBe("ping-1");
    expect(up.received[0]?.["result"]).toEqual({});
    up.stream?.close();
  });

  it("cancelling the client stream cancels the upstream stream", async () => {
    const resp = await rpc(t, { jsonrpc: "2.0", id: "r5", method: "tools/call", params: { name: "x", arguments: {} } }, legacyHeaders());
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", method: "notifications/progress", params: { progressToken: "p", progress: 1 } }));
    await reader.next();
    await reader.cancel();
    await vi.waitFor(() => expect(up.stream?.cancelled()).toBe(true));
  });

  it("bridges list_changed onto a legacy client's GET stream (same era: piped from the upstream GET stream)", async () => {
    const resp = await rpc(t, undefined as never, legacyHeaders(), { method: "GET" });
    expect(resp.status).toBe(200);
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    await vi.waitFor(() => expect(up.getStream).not.toBeNull());
    up.getStream?.event("message", JSON.stringify({ jsonrpc: "2.0", method: "notifications/tools/list_changed" }));
    const ev = await reader.next();
    expect(JSON.parse(ev?.data ?? "{}")["method"]).toBe("notifications/tools/list_changed");
    await reader.cancel();
  });

  it("serves a modern client's subscriptions/listen over a LEGACY upstream: ack first, then translated list_changed", async () => {
    const resp = await rpc(
      t,
      modernBody("subscriptions/listen", { notifications: { toolsListChanged: true, resourceSubscriptions: ["res://a"] } }, "listen-1"),
      modernHeaders("subscriptions/listen"),
    );
    expect(resp.status).toBe(200);
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    const ack = JSON.parse((await reader.next())?.data ?? "{}") as Record<string, unknown>;
    expect(ack["method"]).toBe("notifications/subscriptions/acknowledged");
    const ackParams = ack["params"] as Record<string, unknown>;
    // Cross-era only the tool surface is honored — resource subscriptions are excluded honestly.
    expect(ackParams["notifications"]).toEqual({ toolsListChanged: true });
    expect((ackParams["_meta"] as Record<string, unknown>)["io.modelcontextprotocol/subscriptionId"]).toBe("listen-1");
    await vi.waitFor(() => expect(up.getStream).not.toBeNull());
    up.getStream?.event("message", JSON.stringify({ jsonrpc: "2.0", method: "notifications/tools/list_changed" }));
    const ev = JSON.parse((await reader.next())?.data ?? "{}") as Record<string, unknown>;
    expect(ev["method"]).toBe("notifications/tools/list_changed");
    expect(((ev["params"] as Record<string, unknown>)["_meta"] as Record<string, unknown>)["io.modelcontextprotocol/subscriptionId"]).toBe("listen-1");
    await reader.cancel();
  });

  it("bridges a MODERN upstream's list_changed onto a legacy client's GET stream via subscriptions/listen", async () => {
    resetEngineMemoryForTests();
    const modern = modernFake({ secret: null });
    const t2 = makeCtx(store, usage, { [UPSTREAM_URL]: modern.handler });
    const init = await rpc(t2, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {} } });
    const sid2 = init.headers.get("mcp-session-id") ?? "";
    const resp = await rpc(t2, undefined as never, { "mcp-session-id": sid2, "mcp-protocol-version": "2025-06-18" }, { method: "GET" });
    expect(resp.status).toBe(200);
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    await vi.waitFor(() => expect(modern.listenStreams.length).toBe(1));
    modern.notifyListen(
      JSON.stringify({
        jsonrpc: "2.0",
        method: "notifications/tools/list_changed",
        params: { _meta: { "io.modelcontextprotocol/subscriptionId": "gw:x" } },
      }),
    );
    const ev = JSON.parse((await reader.next())?.data ?? "{}") as Record<string, unknown>;
    expect(ev["method"]).toBe("notifications/tools/list_changed");
    expect(ev["params"]).toBeUndefined(); // Native legacy shape: the subscription metadata is stripped.
    await reader.cancel();
  });

  it("suppresses notifications/message for a modern client that declared no logLevel", async () => {
    const resp = await rpc(t, modernBody("tools/call", { name: "x", arguments: {} }, "m2"), modernHeaders("tools/call", "x"));
    const reader = eventReader(resp.body as ReadableStream<Uint8Array>);
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", method: "notifications/message", params: { level: "info", data: "chatty" } }));
    up.stream?.event("message", JSON.stringify({ jsonrpc: "2.0", id: "m2", result: { content: [] } }));
    const ev = await reader.next();
    expect(JSON.parse(ev?.data ?? "{}")["id"]).toBe("m2"); // The log notification was dropped.
    up.stream?.close();
  });
});
