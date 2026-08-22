/**
 * Cross-revision message mapping — ONE mapper for both reply envelopes (a JSON body and every
 * event of an SSE stream), so filtering and era rules cannot diverge between the two.
 *
 * The bridging scope is deliberately narrow: tools/list, tools/call, and list_changed carry full
 * fidelity across every revision pair (their body shapes are stable in all five revisions);
 * features with no cross-era equivalent pass through only when both sides share an era, and are
 * answered with a plain-sentence JSON-RPC error otherwise. Nothing is bridged that the protocol
 * does not define — a modern `input_required` continuation has no legacy mapping, so a legacy
 * client gets the sentence, never an invented handshake.
 */

import type { GatewayContext, ToolPolicy } from "./ports";
import { filterToolsListResult } from "./filter";
import {
  BEHAVIOR,
  copy,
  ERR_GATEWAY,
  ERR_METHOD_NOT_FOUND,
  META_SERVER_INFO,
  isRequest,
  isResponse,
  rpcError,
  sameEra,
  type JsonRpcFailure,
  type JsonRpcId,
  type JsonRpcMessage,
  type Revision,
} from "./protocol";
import type { SseEvent } from "./sse";
import { asJsonRpcMessage } from "./protocol";

export interface BridgeContext {
  ctx: GatewayContext;
  workspaceId: string;
  serverId: string;
  clientVersion: Revision;
  upstreamVersion: Revision;
  /** The client request this reply answers (null on pure notification channels). */
  requestId: JsonRpcId | null;
  requestMethod: string;
  /** Needed only when the reply may carry a tools/list result. */
  policy: ToolPolicy | null;
  /** Modern clients opt into notifications/message per request via `_meta` logLevel. */
  clientWantsLog: boolean;
  serverInfo: Record<string, unknown> | null;
  /** Deliver a JSON-RPC response back upstream (absorbed server-initiated requests). */
  answerUpstream: (msg: Record<string, unknown>) => void;
  /** A list_changed that cannot ride this channel — hand it to open listen streams. */
  routeListChanged: (method: string) => void;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

const LIST_CHANGED = new Set([
  "notifications/tools/list_changed",
  "notifications/prompts/list_changed",
  "notifications/resources/list_changed",
  "notifications/resources/updated",
]);

export type MappedResult =
  | { kind: "result"; result: Record<string, unknown> }
  | { kind: "error"; error: JsonRpcFailure; outcome: "ok" | "upstream_error" };

/** Map one upstream RESULT for the client's revision (filtering, resultType compat, decoration). */
export async function mapResult(b: BridgeContext, id: JsonRpcId, result: Record<string, unknown>): Promise<MappedResult> {
  let mapped = result;
  const resultType = mapped["resultType"];
  if (resultType !== undefined && resultType !== "complete" && resultType !== "input_required") {
    // 2026-07-28: an unrecognized resultType MUST be considered invalid.
    return { kind: "error", error: rpcError(id, ERR_GATEWAY, copy.upstreamFailed()), outcome: "upstream_error" };
  }
  if (resultType === "input_required" && BEHAVIOR[b.clientVersion].handshake === "initialize") {
    return { kind: "error", error: rpcError(id, ERR_GATEWAY, copy.crossEraInputRequired()), outcome: "ok" };
  }
  if (b.requestMethod === "tools/list" && b.policy) {
    mapped = await filterToolsListResult(b.ctx, b.workspaceId, b.serverId, b.policy, mapped);
  }
  if (BEHAVIOR[b.clientVersion].resultMeta) {
    // Serving a 2026-07-28 client: results carry resultType, list results carry cache hints.
    // cacheScope is forced private — the filtered view is authorization-scoped by definition.
    mapped = { ...mapped, resultType: mapped["resultType"] ?? "complete" };
    if (b.requestMethod === "tools/list") {
      mapped = {
        ...mapped,
        ttlMs: typeof mapped["ttlMs"] === "number" ? mapped["ttlMs"] : 0,
        cacheScope: "private",
      };
    }
    if (b.serverInfo) {
      const meta = isRecord(mapped["_meta"]) ? mapped["_meta"] : {};
      if (meta[META_SERVER_INFO] === undefined) {
        mapped = { ...mapped, _meta: { ...meta, [META_SERVER_INFO]: b.serverInfo } };
      }
    }
  }
  return { kind: "result", result: mapped };
}

/**
 * Map one upstream JSON-RPC message from a reply stream into 0..n messages for the client.
 * Server-initiated requests are forwarded only same-era-legacy; ping is always answered here.
 */
export async function mapStreamMessage(b: BridgeContext, msg: JsonRpcMessage): Promise<JsonRpcMessage[]> {
  if (isResponse(msg)) {
    if (b.requestId === null || msg.id !== b.requestId) return []; // Not ours — fail closed.
    if ("error" in msg) return [msg];
    const mapped = await mapResult(b, msg.id, msg.result);
    return [mapped.kind === "result" ? { jsonrpc: "2.0", id: msg.id, result: mapped.result } : mapped.error];
  }
  if (isRequest(msg)) {
    if (msg.method === "ping") {
      b.answerUpstream({ jsonrpc: "2.0", id: msg.id, result: {} });
      return [];
    }
    if (sameEra(b.clientVersion, b.upstreamVersion) && BEHAVIOR[b.clientVersion].serverRequests) {
      return [msg];
    }
    b.answerUpstream(rpcError(msg.id, ERR_METHOD_NOT_FOUND, copy.crossEra(msg.method)) as unknown as Record<string, unknown>);
    return [];
  }
  // Notification.
  const method = msg.method;
  if (LIST_CHANGED.has(method)) {
    if (BEHAVIOR[b.clientVersion].resultMeta) {
      // A modern client hears change notifications only on its subscriptions/listen stream.
      b.routeListChanged(method);
      return [];
    }
    if (sameEra(b.clientVersion, b.upstreamVersion)) return [msg];
    return method === "notifications/tools/list_changed" ? [msg] : [];
  }
  if (method === "notifications/progress") return [msg];
  if (method === "notifications/message") {
    if (BEHAVIOR[b.clientVersion].resultMeta && !b.clientWantsLog) return [];
    return [msg];
  }
  if (method === "notifications/subscriptions/acknowledged") return []; // Listen-bridge concern only.
  return sameEra(b.clientVersion, b.upstreamVersion) ? [msg] : [];
}

/** Map one SSE event: parse, map each JSON-RPC entry, re-frame. Keep-alive comments pass as-is. */
export async function mapSseEvent(b: BridgeContext, ev: SseEvent): Promise<SseEvent[]> {
  if (ev.data === "") {
    return ev.comments ? [{ data: "", comments: ev.comments }] : [];
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(ev.data);
  } catch {
    b.ctx.env.log("warn", "unparseable SSE event from upstream dropped", { serverId: b.serverId });
    return [];
  }
  const entries = Array.isArray(parsed) ? parsed : [parsed];
  const out: JsonRpcMessage[] = [];
  for (const entry of entries) {
    const msg = asJsonRpcMessage(entry);
    if (!msg) continue;
    out.push(...(await mapStreamMessage(b, msg)));
  }
  if (out.length === 0) {
    return ev.comments ? [{ data: "", comments: ev.comments }] : [];
  }
  // Resumability ids ride only where both sides define them (legacy↔legacy Streamable HTTP).
  const keepFraming =
    BEHAVIOR[b.clientVersion].resumable && BEHAVIOR[b.upstreamVersion].resumable;
  return out.map((m, i) => ({
    ...(ev.event !== undefined ? { event: ev.event } : {}),
    data: JSON.stringify(m),
    ...(keepFraming && ev.id !== undefined && i === 0 ? { id: ev.id } : {}),
    ...(keepFraming && ev.retry !== undefined && i === 0 ? { retry: ev.retry } : {}),
    ...(i === 0 && ev.comments ? { comments: ev.comments } : {}),
  }));
}
