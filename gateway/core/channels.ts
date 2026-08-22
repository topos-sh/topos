/**
 * Notification-channel bridging — how unsolicited server messages (list_changed above all)
 * reach each client era:
 *
 *   client 2024-11-05        ← its one HTTP+SSE message stream
 *   client 2025-03-26..11-25 ← the GET stream it opens with its session
 *   client 2026-07-28        ← an opted-in `subscriptions/listen` request stream
 *
 * and where the gateway sources them per upstream era:
 *
 *   upstream 2024-11-05        ← the standing message-stream pump
 *   upstream 2025-03-26..11-25 ← a GET stream the gateway opens (405 tolerated)
 *   upstream 2026-07-28        ← a `subscriptions/listen` the gateway maintains
 *
 * Same-era channels pass through with full fidelity; cross-era only the tool surface's
 * list_changed is translated — everything else is dropped or answered, never faked.
 */

import { consumeSse, sseSource, type SseWriter } from "./sse";
import {
  asJsonRpcMessage,
  META_SUBSCRIPTION,
  nextGatewayId,
  rpcResult,
  type JsonRpcId,
  type JsonRpcMessage,
  type Revision,
} from "./protocol";
import { mapStreamMessage, type BridgeContext } from "./bridge";
import { memory } from "./state";
import {
  openUpstreamGetStream,
  sendUpstreamOneWay,
  sendUpstreamRequest,
  GATEWAY_IDENTITY,
  type ClientIdentity,
  type UpstreamCall,
} from "./upstream";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function channelBridge(call: UpstreamCall, clientVersion: Revision, upstreamVersion: Revision, identity: ClientIdentity): BridgeContext {
  return {
    ctx: call.ctx,
    workspaceId: call.session.workspaceId,
    serverId: call.server.serverId,
    clientVersion,
    upstreamVersion,
    requestId: null,
    requestMethod: "",
    policy: null,
    clientWantsLog: false,
    serverInfo: null,
    answerUpstream: (msg) => {
      void sendUpstreamOneWay(call, msg, identity);
    },
    routeListChanged: (method) => routeToListenChannels(call, method),
  };
}

/** Deliver a list_changed heard off-channel to any open modern listen streams of this pair. */
export function routeToListenChannels(call: UpstreamCall, method: string): void {
  if (method !== "notifications/tools/list_changed") return;
  for (const ch of memory.listens(call.session.sessionId, call.server.serverId)) {
    if (ch.tools) ch.send({ jsonrpc: "2.0", method });
  }
}

function emitMessage(writer: SseWriter, msg: JsonRpcMessage | Record<string, unknown>): void {
  writer.send({ event: "message", data: JSON.stringify(msg) });
}

/**
 * Attach an upstream notification source feeding `emit`; returns a detach. The upstream session
 * must already exist (ensureUpstream ran). Failures degrade to a silent channel — notifications
 * are best-effort by nature; requests/responses never ride this path.
 */
export async function attachUpstreamNotifications(
  call: UpstreamCall,
  clientVersion: Revision,
  identity: ClientIdentity,
  emit: (msg: JsonRpcMessage) => void,
): Promise<() => void> {
  const us = memory.upstream(call.session.sessionId, call.server.serverId);
  if (!us) return () => {};
  const bridge = channelBridge(call, clientVersion, us.version, identity);

  if (us.version === "2024-11-05") {
    const handle = us.sse2024;
    if (!handle) return () => {};
    handle.notify = (raw) => {
      const msg = asJsonRpcMessage(raw);
      if (!msg) return false;
      void mapStreamMessage(bridge, msg).then((out) => {
        for (const m of out) emit(m);
      });
      return true;
    };
    return () => {
      if (handle.notify) handle.notify = null;
    };
  }

  if (us.version === "2026-07-28") {
    // Cross-era by construction here (a modern client's listen is served in serveModernListen):
    // subscribe upstream to the tool surface only and translate.
    const controller = new AbortController();
    void (async () => {
      const listen = await sendUpstreamRequest(
        call,
        {
          jsonrpc: "2.0",
          id: nextGatewayId(),
          method: "subscriptions/listen",
          params: { notifications: { toolsListChanged: true } },
        },
        { identity: GATEWAY_IDENTITY },
      );
      if (listen.kind !== "sse") return;
      controller.signal.addEventListener("abort", () => {
        listen.body.cancel().catch(() => {});
      });
      await consumeSse(listen.body, (ev) => {
        if (ev.data === "") return;
        let parsed: unknown;
        try {
          parsed = JSON.parse(ev.data);
        } catch {
          return;
        }
        const msg = asJsonRpcMessage(parsed);
        if (!msg || !("method" in msg) || "id" in msg) return;
        if (msg.method !== "notifications/tools/list_changed") return;
        emit({ jsonrpc: "2.0", method: msg.method }); // Stripped of subscription _meta: native shape.
      }).catch(() => {});
    })();
    return () => controller.abort();
  }

  // Legacy Streamable HTTP upstream: its GET stream is the source. 405 = no stream offered.
  const controller = new AbortController();
  void (async () => {
    const resp = await openUpstreamGetStream(call, null);
    if (!resp || !resp.ok || !resp.body) {
      resp?.body?.cancel().catch(() => {});
      return;
    }
    controller.signal.addEventListener("abort", () => {
      resp.body?.cancel().catch(() => {});
    });
    await consumeSse(resp.body, async (ev) => {
      if (ev.data === "") return;
      let parsed: unknown;
      try {
        parsed = JSON.parse(ev.data);
      } catch {
        return;
      }
      const entries = Array.isArray(parsed) ? parsed : [parsed];
      for (const entry of entries) {
        const msg = asJsonRpcMessage(entry);
        if (!msg) continue;
        for (const m of await mapStreamMessage(bridge, msg)) emit(m);
      }
    }).catch(() => {});
  })();
  return () => controller.abort();
}

/**
 * Serve a legacy Streamable HTTP client's GET stream (2025-03-26..2025-11-25). Same-era the
 * upstream's own GET stream feeds it; cross-era only translated tools/list_changed arrives.
 */
export async function serveClientGetStream(call: UpstreamCall, clientVersion: Revision, identity: ClientIdentity): Promise<Response> {
  const { stream, writer } = sseSource(() => detach());
  let detachFn: (() => void) | null = null;
  const detach = () => {
    detachFn?.();
    detachFn = null;
  };
  detachFn = await attachUpstreamNotifications(call, clientVersion, identity, (msg) => emitMessage(writer, msg));
  return new Response(stream, {
    status: 200,
    headers: { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" },
  });
}

/**
 * Serve a 2026-07-28 client's `subscriptions/listen`. Same-era: the request is forwarded and the
 * upstream stream piped byte-for-byte (ids forwarded verbatim, so the upstream's subscriptionId
 * metadata already names the client's own listen id). Cross-era: the gateway acknowledges the
 * honored subset itself (the tool surface) and translates from the legacy notification channel.
 */
export async function serveModernListen(call: UpstreamCall, msg: { id: JsonRpcId; params?: Record<string, unknown> }, identity: ClientIdentity): Promise<Response | null> {
  const us = memory.upstream(call.session.sessionId, call.server.serverId);
  if (!us) return null;

  if (us.version === "2026-07-28") {
    const reply = await sendUpstreamRequest(call, { jsonrpc: "2.0", id: msg.id, method: "subscriptions/listen", ...(msg.params ? { params: msg.params } : {}) }, { identity });
    if (reply.kind === "sse") {
      return new Response(reply.body, {
        status: 200,
        headers: { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" },
      });
    }
    if (reply.kind === "json") {
      return new Response(JSON.stringify(reply.message), {
        status: reply.status,
        headers: { "content-type": "application/json" },
      });
    }
    return null;
  }

  const requested = isRecord(msg.params?.["notifications"]) ? (msg.params["notifications"] as Record<string, unknown>) : {};
  const wantsTools = requested["toolsListChanged"] === true;
  const { stream, writer } = sseSource(() => teardown());
  let removeListen: (() => void) | null = null;
  let detachSource: (() => void) | null = null;
  const teardown = () => {
    removeListen?.();
    removeListen = null;
    detachSource?.();
    detachSource = null;
  };
  const withSubMeta = (m: Record<string, unknown>): Record<string, unknown> => {
    const params = isRecord(m["params"]) ? m["params"] : {};
    const meta = isRecord(params["_meta"]) ? params["_meta"] : {};
    return { ...m, params: { ...params, _meta: { ...meta, [META_SUBSCRIPTION]: msg.id } } };
  };
  // The honored subset the gateway can actually deliver across the era boundary: tools only.
  const honored: Record<string, unknown> = wantsTools ? { toolsListChanged: true } : {};
  writer.send({
    event: "message",
    data: JSON.stringify(
      withSubMeta({ jsonrpc: "2.0", method: "notifications/subscriptions/acknowledged", params: { notifications: honored } }),
    ),
  });
  if (wantsTools) {
    removeListen = memory.addListen(call.session.sessionId, call.server.serverId, {
      tools: true,
      send: (m) => writer.send({ event: "message", data: JSON.stringify(withSubMeta(m)) }),
    });
    detachSource = await attachUpstreamNotifications(call, "2026-07-28", identity, (m) => {
      if ("method" in m && m.method === "notifications/tools/list_changed") {
        writer.send({ event: "message", data: JSON.stringify(withSubMeta(m as unknown as Record<string, unknown>)) });
      }
    });
  }
  return new Response(stream, {
    status: 200,
    headers: { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" },
  });
}

/** The 2024-11-05 client transport: the GET stream with its `endpoint` event, module-tracked. */
export function serve2024ClientStream(requestUrl: string, toposSessionId: string, serverId: string, mintToken: () => string): Response {
  const token = mintToken();
  const { stream, writer } = sseSource(() => {
    const ch = memory.legacyChannels.get(token);
    ch?.detachUpstream?.();
    memory.legacyChannels.delete(token);
  });
  memory.legacyChannels.set(token, {
    token,
    toposSessionId,
    serverId,
    writer,
    clientCapabilities: {},
    clientInfo: undefined,
    initialized: false,
    detachUpstream: null,
  });
  const endpoint = new URL(requestUrl);
  endpoint.searchParams.set("sse", token);
  writer.send({ event: "endpoint", data: endpoint.toString() });
  return new Response(stream, {
    status: 200,
    headers: { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" },
  });
}

/** Answer a server-initiated ping locally on a client channel (never forwarded upstream). */
export function localPingResult(id: JsonRpcId): JsonRpcMessage {
  return rpcResult(id, {});
}
