/**
 * The gateway's client stack toward upstream MCP servers — one full client per revision, chosen
 * by DETECTION, never by trusting headers: response bodies are the only era truth. The verdict
 * is cached in module memory per serverId and re-probed when a cached assumption later fails.
 *
 * Detection order (the published backward-compatibility procedure):
 *   1. a modern `server/discover` probe — a DiscoverResult, or a 400 whose body is a recognized
 *      modern error (−32020/−32021/−32022), proves a 2026-07-28 server;
 *   2. otherwise a legacy `initialize` — the result's `protocolVersion` names the revision and
 *      the response may mint an `Mcp-Session-Id`;
 *   3. otherwise a GET expecting an SSE stream whose first event is `endpoint` — the 2024-11-05
 *      two-endpoint transport.
 *
 * Token custody: the CLIENT's bearer never appears here — only the workspace credential the
 * store resolved, attached per upstream request (`Authorization: Bearer` or the document's named
 * secret header). On an upstream 401 a refreshable oauth credential is refreshed ONCE per
 * gateway request and the call retried once; a second 401 passes through.
 */

import type { Credential, GatewayContext, ResolvedServer, SessionRef } from "./ports";
import {
  BEHAVIOR,
  copy,
  ERR_GATEWAY,
  ERR_HEADER_MISMATCH,
  ERR_METHOD_NOT_FOUND,
  ERR_MISSING_CAPABILITY,
  ERR_UNSUPPORTED_VERSION,
  META_CAPABILITIES,
  META_CLIENT_INFO,
  META_SERVER_INFO,
  META_VERSION,
  MODERN,
  LATEST_LEGACY,
  asJsonRpcMessage,
  isRevision,
  isResponse,
  nextGatewayId,
  rpcError,
  type JsonRpcMessage,
  type JsonRpcNotification,
  type JsonRpcRequest,
  type Revision,
} from "./protocol";
import { isRefreshable, refreshCredential } from "./refresh";
import { consumeSse, SseParser } from "./sse";
import { memory, type Sse2024Handle, type UpstreamSession, type UpstreamVerdict } from "./state";

/** Mutable per-gateway-request auth state: the once-only refresh latch lives here. */
export interface AuthState {
  credential: Credential | null;
  secret: string | null;
  refreshed: boolean;
}

export interface UpstreamCall {
  ctx: GatewayContext;
  session: SessionRef;
  server: ResolvedServer;
  auth: AuthState;
}

/** What the gateway presents upstream as "the client" (forwarded from the real client same-era). */
export interface ClientIdentity {
  capabilities: Record<string, unknown>;
  clientInfo: unknown;
  /** The real client's revision. Cross-era the gateway declares EMPTY capabilities upstream —
   * it may only advertise interactions it can actually relay back to that client. */
  version: Revision;
}

export const GATEWAY_IDENTITY: ClientIdentity = {
  capabilities: {},
  clientInfo: { name: "topos-gateway", version: "1" },
  version: MODERN,
};

/** The capabilities to declare in a legacy handshake toward this upstream. */
function legacyCaps(identity: ClientIdentity): Record<string, unknown> {
  return BEHAVIOR[identity.version].handshake === "initialize" ? identity.capabilities : {};
}

/** The capabilities to declare in modern per-request `_meta` toward a 2026-07-28 upstream. */
function modernCaps(identity: ClientIdentity): Record<string, unknown> {
  return identity.version === MODERN ? identity.capabilities : {};
}

export type UpstreamReply =
  | { kind: "json"; message: JsonRpcMessage; status: number }
  | { kind: "sse"; body: ReadableStream<Uint8Array> }
  | { kind: "accepted" }
  | { kind: "fail"; outcome: "upstream_error" | "unauthorized"; status: number };

export interface UpstreamReady {
  verdict: UpstreamVerdict;
  us: UpstreamSession;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

// -- auth attach + refresh-once-retry ---------------------------------------------------------

function withAuth(call: UpstreamCall, headers: Record<string, string>): Record<string, string> {
  if (call.auth.secret === null) return headers;
  const name = call.server.secretHeader ?? "authorization";
  const value = call.server.secretHeader ? call.auth.secret : `Bearer ${call.auth.secret}`;
  return { ...headers, [name]: value };
}

/**
 * The one upstream fetch path. Bodies are always strings here, so the single retry after a
 * refresh can resend them verbatim. Throws on network failure — callers map to upstream_error.
 */
async function authedFetch(
  call: UpstreamCall,
  url: string,
  init: { method: string; headers: Record<string, string>; body?: string; signal?: AbortSignal },
): Promise<Response> {
  const attempt = () =>
    call.ctx.env.fetch(url, {
      method: init.method,
      headers: withAuth(call, init.headers),
      ...(init.body !== undefined ? { body: init.body } : {}),
      ...(init.signal ? { signal: init.signal } : {}),
    });
  const first = await attempt();
  if (first.status !== 401 || call.auth.refreshed) return first;
  const cred = call.auth.credential;
  if (!cred || !isRefreshable(cred)) return first;
  call.auth.refreshed = true;
  const rotated = await refreshCredential(
    call.ctx,
    { workspaceId: call.session.workspaceId, serverId: call.server.serverId, userId: call.session.userId },
    cred,
  );
  if (rotated === null) return first;
  call.auth.secret = rotated;
  first.body?.cancel().catch(() => {});
  return attempt();
}

// -- headers per revision ---------------------------------------------------------------------

const POST_ACCEPT = "application/json, text/event-stream";

function encodeSentinel(value: string): string {
  if (/^[\x21-\x7e]+$/.test(value)) return value;
  const bytes = new TextEncoder().encode(value);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return `=?base64?${btoa(bin)}?=`;
}

function requestHeaders(version: Revision, us: UpstreamSession | null, msg?: JsonRpcRequest | JsonRpcNotification): Record<string, string> {
  const behavior = BEHAVIOR[version];
  const headers: Record<string, string> = { "content-type": "application/json", accept: POST_ACCEPT };
  if (behavior.versionHeader) headers["mcp-protocol-version"] = version;
  if (behavior.sessionHeader && us?.mcpSessionId) headers["mcp-session-id"] = us.mcpSessionId;
  if (behavior.methodHeaders && msg) {
    headers["mcp-method"] = msg.method;
    if (msg.method === "tools/call" || msg.method === "prompts/get") {
      const name = msg.params?.["name"];
      if (typeof name === "string") headers["mcp-name"] = encodeSentinel(name);
    } else if (msg.method === "resources/read") {
      const uri = msg.params?.["uri"];
      if (typeof uri === "string") headers["mcp-name"] = encodeSentinel(uri);
    }
  }
  return headers;
}

/** Inject the required 2026-07-28 `_meta` fields, preserving any client-supplied passthrough. */
function withModernMeta(msg: JsonRpcRequest, identity: ClientIdentity): JsonRpcRequest {
  const params = msg.params ?? {};
  const meta = isRecord(params["_meta"]) ? params["_meta"] : {};
  return {
    ...msg,
    params: {
      ...params,
      _meta: {
        ...meta,
        [META_VERSION]: MODERN,
        [META_CAPABILITIES]: isRecord(meta[META_CAPABILITIES]) ? meta[META_CAPABILITIES] : modernCaps(identity),
        ...(meta[META_CLIENT_INFO] !== undefined || identity.clientInfo === undefined
          ? {}
          : { [META_CLIENT_INFO]: identity.clientInfo }),
      },
    },
  };
}

// -- response envelope handling ---------------------------------------------------------------

function isSseResponse(resp: Response): boolean {
  return (resp.headers.get("content-type") ?? "").toLowerCase().includes("text/event-stream");
}

function isJsonResponse(resp: Response): boolean {
  return (resp.headers.get("content-type") ?? "").toLowerCase().includes("application/json");
}

async function parseJsonBody(resp: Response, wantId: string | number): Promise<JsonRpcMessage | null> {
  const parsed: unknown = await resp.json().catch(() => null);
  if (Array.isArray(parsed)) {
    // 2025-03-26 servers MAY batch responses; we only ever send single requests — pick ours.
    for (const entry of parsed) {
      const msg = asJsonRpcMessage(entry);
      if (msg && isResponse(msg) && msg.id === wantId) return msg;
    }
    return null;
  }
  return asJsonRpcMessage(parsed);
}

/** How long the unary handshake-sized SSE read waits for its response id before giving up. A
 *  server that opens the stream but never emits the answer (keep-alives, unrelated events) would
 *  otherwise hold the read forever; the deadline is polled off the injected clock, so the core
 *  schedules no timer of its own. Passthrough streaming (a client's own tools/call answer) does
 *  NOT come through here — those are piped and bounded by the client connection. */
const SSE_HANDSHAKE_DEADLINE_MS = 60_000;

/** Extract the response with `id` out of an SSE body (handshake-sized reads only), then cancel. */
export async function responseFromSse(
  body: ReadableStream<Uint8Array>,
  id: string | number,
  now: () => number,
): Promise<JsonRpcMessage | null> {
  const parser = new SseParser();
  const decoder = new TextDecoder();
  const reader = body.getReader();
  const deadline = now() + SSE_HANDSHAKE_DEADLINE_MS;
  try {
    for (;;) {
      if (now() >= deadline) {
        reader.cancel().catch(() => {});
        return null;
      }
      const { done, value } = await reader.read();
      if (done) return null;
      for (const ev of parser.push(decoder.decode(value, { stream: true }))) {
        if (ev.data === "") continue;
        const parsed: unknown = JSON.parse(ev.data);
        const entries = Array.isArray(parsed) ? parsed : [parsed];
        for (const entry of entries) {
          const msg = asJsonRpcMessage(entry);
          if (msg && isResponse(msg) && msg.id === id) {
            reader.cancel().catch(() => {});
            return msg;
          }
        }
      }
    }
  } catch {
    return null;
  } finally {
    reader.releaseLock();
  }
}

// -- era detection + session establishment ----------------------------------------------------

const MODERN_ERROR_CODES = new Set([ERR_HEADER_MISMATCH, ERR_MISSING_CAPABILITY, ERR_UNSUPPORTED_VERSION]);

async function probeModern(call: UpstreamCall): Promise<UpstreamVerdict | null> {
  const id = nextGatewayId();
  const probe = withModernMeta({ jsonrpc: "2.0", id, method: "server/discover", params: {} }, GATEWAY_IDENTITY);
  let resp: Response;
  try {
    resp = await authedFetch(call, call.server.url, {
      method: "POST",
      headers: requestHeaders(MODERN, null, probe),
      body: JSON.stringify(probe),
    });
  } catch {
    return null;
  }
  if (resp.ok && (isJsonResponse(resp) || isSseResponse(resp))) {
    const msg = isSseResponse(resp)
      ? resp.body
        ? await responseFromSse(resp.body, id, call.ctx.env.now)
        : null
      : await parseJsonBody(resp, id);
    if (msg && isResponse(msg) && "result" in msg) {
      const result = msg.result;
      const supported = Array.isArray(result["supportedVersions"])
        ? (result["supportedVersions"] as unknown[]).filter((v): v is string => typeof v === "string")
        : [];
      if (!supported.includes(MODERN)) return null;
      const meta = isRecord(result["_meta"]) ? result["_meta"] : {};
      return {
        version: MODERN,
        url: call.server.url,
        serverInfo: isRecord(meta[META_SERVER_INFO]) ? meta[META_SERVER_INFO] : null,
        capabilities: isRecord(result["capabilities"]) ? result["capabilities"] : {},
        instructions: typeof result["instructions"] === "string" ? result["instructions"] : null,
        supportedVersions: supported,
      };
    }
    return null;
  }
  if (resp.status === 400) {
    // A recognized modern error body proves a modern server even when the probe was rejected.
    const parsed: unknown = await resp.json().catch(() => null);
    const msg = asJsonRpcMessage(parsed);
    if (msg && isResponse(msg) && "error" in msg && MODERN_ERROR_CODES.has(msg.error.code)) {
      return {
        version: MODERN,
        url: call.server.url,
        serverInfo: null,
        capabilities: {},
        instructions: null,
        supportedVersions: null,
      };
    }
  } else {
    resp.body?.cancel().catch(() => {});
  }
  return null;
}

interface LegacyInit {
  verdict: UpstreamVerdict;
  mcpSessionId: string | null;
}

async function legacyInitialize(
  call: UpstreamCall,
  identity: ClientIdentity,
  askVersion: Revision,
): Promise<LegacyInit | "transport-miss" | null> {
  const id = nextGatewayId();
  const msg: JsonRpcRequest = {
    jsonrpc: "2.0",
    id,
    method: "initialize",
    params: {
      protocolVersion: askVersion,
      capabilities: legacyCaps(identity),
      clientInfo: identity.clientInfo ?? GATEWAY_IDENTITY.clientInfo,
    },
  };
  let resp: Response;
  try {
    resp = await authedFetch(call, call.server.url, {
      method: "POST",
      headers: { "content-type": "application/json", accept: POST_ACCEPT },
      body: JSON.stringify(msg),
    });
  } catch {
    return null;
  }
  if (resp.status === 404 || resp.status === 405) {
    resp.body?.cancel().catch(() => {});
    return "transport-miss"; // The POST-fails signal that precedes the 2024-11-05 GET probe.
  }
  if (!resp.ok) {
    resp.body?.cancel().catch(() => {});
    return null;
  }
  const reply = isSseResponse(resp)
    ? resp.body
      ? await responseFromSse(resp.body, id, call.ctx.env.now)
      : null
    : await parseJsonBody(resp, id);
  if (!reply || !isResponse(reply) || !("result" in reply)) return null;
  const version = reply.result["protocolVersion"];
  if (!isRevision(version) || version === MODERN) return null;
  const sessionId = resp.headers.get("mcp-session-id"); // Case-insensitive get covers both spellings.
  return {
    verdict: {
      version,
      url: call.server.url,
      serverInfo: isRecord(reply.result["serverInfo"]) ? reply.result["serverInfo"] : null,
      capabilities: isRecord(reply.result["capabilities"]) ? reply.result["capabilities"] : {},
      instructions: typeof reply.result["instructions"] === "string" ? reply.result["instructions"] : null,
      supportedVersions: null,
    },
    mcpSessionId: sessionId,
  };
}

async function sendInitialized(call: UpstreamCall, us: UpstreamSession): Promise<void> {
  const note: JsonRpcNotification = { jsonrpc: "2.0", method: "notifications/initialized" };
  try {
    const resp = await authedFetch(call, call.server.url, {
      method: "POST",
      headers: requestHeaders(us.version, us, note),
      body: JSON.stringify(note),
    });
    resp.body?.cancel().catch(() => {});
  } catch {
    // Best-effort: a lost initialized notification only degrades politeness, not correctness.
  }
}

/** Open the 2024-11-05 upstream: GET the SSE stream, wait for `endpoint`, keep the pump alive. */
async function open2024(call: UpstreamCall): Promise<Sse2024Handle | null> {
  const abort = new AbortController();
  let resp: Response;
  try {
    resp = await authedFetch(call, call.server.url, {
      method: "GET",
      headers: { accept: "text/event-stream" },
      signal: abort.signal,
    });
  } catch {
    return null;
  }
  if (!resp.ok || !isSseResponse(resp) || !resp.body) {
    resp.body?.cancel().catch(() => {});
    return null;
  }
  const handle: Sse2024Handle = { endpointUrl: "", pending: new Map(), notify: null, abort };
  let resolveEndpoint: (ok: boolean) => void;
  const endpointReady = new Promise<boolean>((resolve) => {
    resolveEndpoint = resolve;
  });
  const origin = new URL(call.server.url).origin;
  // Floating pump: reads server messages for the life of the upstream session. Single-process
  // module memory owns the handle; aborting the controller ends the pump.
  void consumeSse(resp.body, async (ev) => {
    if (ev.event === "endpoint") {
      const resolved = new URL(ev.data, call.server.url);
      // The endpoint event names where credentials get POSTed — never follow it off-origin.
      if (resolved.origin !== origin) {
        call.ctx.env.log("warn", "2024-11-05 endpoint event left the server origin; refusing", {
          serverId: call.server.serverId,
        });
        abort.abort();
        resolveEndpoint(false);
        return;
      }
      handle.endpointUrl = resolved.toString();
      resolveEndpoint(true);
      return;
    }
    if (ev.data === "") return;
    let parsed: unknown;
    try {
      parsed = JSON.parse(ev.data);
    } catch {
      return;
    }
    const msg = asJsonRpcMessage(parsed);
    if (!msg) return;
    if (isResponse(msg) && msg.id !== null) {
      const waiter = handle.pending.get(msg.id);
      if (waiter) {
        handle.pending.delete(msg.id);
        waiter(msg);
      }
      return;
    }
    if ("method" in msg && "id" in msg) {
      // Server-initiated request on the old transport. Answer ping ourselves; route the rest to
      // an attached client channel or refuse cleanly upstream.
      if (msg.method === "ping") {
        await post2024(call, handle, { jsonrpc: "2.0", id: msg.id, result: {} });
        return;
      }
      if (handle.notify && handle.notify(msg as unknown as Record<string, unknown>)) return;
      await post2024(call, handle, rpcError(msg.id ?? null, ERR_METHOD_NOT_FOUND, copy.crossEra(msg.method)));
      return;
    }
    if ("method" in msg) {
      handle.notify?.(msg as unknown as Record<string, unknown>);
    }
  }).catch(() => {});
  const ok = await endpointReady;
  return ok ? handle : null;
}

async function post2024(call: UpstreamCall, handle: Sse2024Handle, msg: unknown): Promise<boolean> {
  try {
    const resp = await authedFetch(call, handle.endpointUrl, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(msg),
    });
    resp.body?.cancel().catch(() => {});
    return resp.ok || resp.status === 202;
  } catch {
    return false;
  }
}

async function request2024(call: UpstreamCall, handle: Sse2024Handle, msg: JsonRpcRequest): Promise<JsonRpcMessage | null> {
  const answer = new Promise<JsonRpcMessage>((resolve) => {
    handle.pending.set(msg.id, (m) => resolve(m as JsonRpcMessage));
  });
  const posted = await post2024(call, handle, msg);
  if (!posted) {
    handle.pending.delete(msg.id);
    return null;
  }
  // No engine-side timers (the core owns no clock beyond now()); an unanswered request resolves
  // only when the pump dies. The host's fetch timeouts bound this in practice.
  return answer;
}

/**
 * Ensure the upstream's era is known and a conversation exists for this (topos session, server).
 * `identity` is what we present as the client — the real client's declarations same-era.
 */
export async function ensureUpstream(call: UpstreamCall, identity: ClientIdentity): Promise<UpstreamReady | null> {
  const { session, server } = call;
  const credentialId = call.auth.credential?.id ?? null;
  let verdict = memory.verdict(server.serverId);
  if (verdict && verdict.url !== server.url) {
    // The resolved revision moved the endpoint — everything cached about the old one is stale.
    memory.dropVerdict(server.serverId);
    memory.dropUpstream(session.sessionId, server.serverId);
    verdict = null;
  }
  const existing = memory.upstream(session.sessionId, server.serverId);
  // Reuse the conversation only if the SAME credential still backs it. A disconnect+reconnect
  // under a different upstream account changes the credential id; reusing the old upstream
  // Mcp-Session-Id would leave it bound to the prior principal.
  if (existing && existing.credentialId !== credentialId) {
    memory.dropUpstream(session.sessionId, server.serverId);
  } else if (verdict && existing && existing.version === verdict.version) {
    return { verdict, us: existing };
  }

  if (!verdict) {
    verdict = await probeModern(call);
    if (!verdict) {
      const init = await legacyInitialize(call, identity, LATEST_LEGACY);
      if (init === null) return null;
      if (init === "transport-miss") {
        const pumped = await open2024(call);
        if (!pumped) return null;
        // Old transport found: run the legacy handshake over the message stream.
        const id = nextGatewayId();
        const reply = await request2024(call, pumped, {
          jsonrpc: "2.0",
          id,
          method: "initialize",
          params: {
            protocolVersion: "2024-11-05",
            capabilities: legacyCaps(identity),
            clientInfo: identity.clientInfo ?? GATEWAY_IDENTITY.clientInfo,
          },
        });
        if (!reply || !("result" in reply)) {
          pumped.abort.abort();
          return null;
        }
        void post2024(call, pumped, { jsonrpc: "2.0", method: "notifications/initialized" });
        verdict = {
          version: "2024-11-05",
          url: server.url,
          serverInfo: isRecord(reply.result["serverInfo"]) ? reply.result["serverInfo"] : null,
          capabilities: isRecord(reply.result["capabilities"]) ? reply.result["capabilities"] : {},
          instructions: typeof reply.result["instructions"] === "string" ? reply.result["instructions"] : null,
          supportedVersions: null,
        };
        memory.setVerdict(server.serverId, verdict);
        const us: UpstreamSession = { version: "2024-11-05", mcpSessionId: null, initialized: true, sse2024: pumped, credentialId };
        memory.setUpstream(session.sessionId, server.serverId, us);
        return { verdict, us };
      }
      memory.setVerdict(server.serverId, init.verdict);
      const us: UpstreamSession = {
        version: init.verdict.version,
        mcpSessionId: init.mcpSessionId,
        initialized: true,
        sse2024: null,
        credentialId,
      };
      memory.setUpstream(session.sessionId, server.serverId, us);
      await sendInitialized(call, us);
      return { verdict: init.verdict, us };
    }
    memory.setVerdict(server.serverId, verdict);
  }

  // Verdict known; establish this pair's conversation.
  if (verdict.version === MODERN) {
    const us: UpstreamSession = { version: MODERN, mcpSessionId: null, initialized: true, sse2024: null, credentialId };
    memory.setUpstream(session.sessionId, server.serverId, us);
    return { verdict, us };
  }
  if (verdict.version === "2024-11-05") {
    const handle = await open2024(call);
    if (!handle) return null;
    const id = nextGatewayId();
    const reply = await request2024(call, handle, {
      jsonrpc: "2.0",
      id,
      method: "initialize",
      params: {
        protocolVersion: "2024-11-05",
        capabilities: legacyCaps(identity),
        clientInfo: identity.clientInfo ?? GATEWAY_IDENTITY.clientInfo,
      },
    });
    if (!reply || !("result" in reply)) {
      handle.abort.abort();
      return null;
    }
    void post2024(call, handle, { jsonrpc: "2.0", method: "notifications/initialized" });
    const us: UpstreamSession = { version: "2024-11-05", mcpSessionId: null, initialized: true, sse2024: handle, credentialId };
    memory.setUpstream(session.sessionId, server.serverId, us);
    return { verdict, us };
  }
  const init = await legacyInitialize(call, identity, verdict.version);
  if (init === "transport-miss" || init === null) {
    // The cached verdict no longer matches the server's behavior — drop it; next call re-probes.
    memory.dropVerdict(server.serverId);
    return null;
  }
  const us: UpstreamSession = { version: init.verdict.version, mcpSessionId: init.mcpSessionId, initialized: true, sse2024: null, credentialId };
  memory.setVerdict(server.serverId, init.verdict);
  memory.setUpstream(session.sessionId, server.serverId, us);
  await sendInitialized(call, us);
  return { verdict: init.verdict, us };
}

// -- the request path -------------------------------------------------------------------------

export interface SendOptions {
  identity: ClientIdentity;
  /** Registered for `notifications/cancelled` (legacy clients); cleared when the reply lands. */
  inflightKey?: { channel: string; id: string | number };
}

/** Send one JSON-RPC request upstream, era-appropriately, and hand back the raw reply envelope. */
export async function sendUpstreamRequest(call: UpstreamCall, msg: JsonRpcRequest, opts: SendOptions): Promise<UpstreamReply> {
  const ready = await ensureUpstream(call, opts.identity);
  if (!ready) return { kind: "fail", outcome: "upstream_error", status: 502 };
  const { us } = ready;

  if (us.version === "2024-11-05") {
    const handle = us.sse2024;
    if (!handle) return { kind: "fail", outcome: "upstream_error", status: 502 };
    const reply = await request2024(call, handle, msg);
    if (!reply) {
      memory.dropUpstream(call.session.sessionId, call.server.serverId);
      return { kind: "fail", outcome: "upstream_error", status: 502 };
    }
    return { kind: "json", message: reply, status: 200 };
  }

  const outbound = us.version === MODERN ? withModernMeta(msg, opts.identity) : msg;
  const controller = new AbortController();
  const untrack = opts.inflightKey
    ? memory.trackInflight(opts.inflightKey.channel, opts.inflightKey.id, controller)
    : null;
  try {
    for (let attempt = 0; ; attempt++) {
      const sessionAtSend = memory.upstream(call.session.sessionId, call.server.serverId);
      let resp: Response;
      try {
        resp = await authedFetch(call, call.server.url, {
          method: "POST",
          headers: requestHeaders(us.version, sessionAtSend, outbound),
          body: JSON.stringify(outbound),
          signal: controller.signal,
        });
      } catch {
        return { kind: "fail", outcome: "upstream_error", status: 502 };
      }
      if (resp.status === 404 && sessionAtSend?.mcpSessionId && attempt === 0) {
        // Dead upstream session: the defined recovery is a fresh initialize, then the retry.
        resp.body?.cancel().catch(() => {});
        memory.dropUpstream(call.session.sessionId, call.server.serverId);
        const again = await ensureUpstream(call, opts.identity);
        if (!again) return { kind: "fail", outcome: "upstream_error", status: 502 };
        continue;
      }
      return classify(call, resp, msg.id);
    }
  } finally {
    untrack?.();
  }
}

async function classify(call: UpstreamCall, resp: Response, wantId: string | number): Promise<UpstreamReply> {
  if (resp.status === 202) {
    resp.body?.cancel().catch(() => {});
    return { kind: "accepted" };
  }
  if (resp.status === 401) {
    resp.body?.cancel().catch(() => {});
    return { kind: "fail", outcome: "unauthorized", status: 401 };
  }
  if (isSseResponse(resp) && resp.ok && resp.body) {
    return { kind: "sse", body: resp.body };
  }
  if (isJsonResponse(resp)) {
    const message = await parseJsonBody(resp, wantId);
    if (message) return { kind: "json", message, status: resp.status };
  } else {
    resp.body?.cancel().catch(() => {});
  }
  if (resp.status === 400 || resp.status === 405 || resp.status === 406) {
    // Unparseable protocol-level rejection: the cached era may be wrong — re-probe next call.
    memory.dropVerdict(call.server.serverId);
    memory.dropUpstream(call.session.sessionId, call.server.serverId);
  }
  return { kind: "fail", outcome: "upstream_error", status: resp.status };
}

/** Fire-and-forget a notification (or a client's JSON-RPC response) toward the upstream. */
export async function sendUpstreamOneWay(call: UpstreamCall, msg: JsonRpcNotification | Record<string, unknown>, identity: ClientIdentity): Promise<void> {
  const ready = await ensureUpstream(call, identity);
  if (!ready) return;
  const { us } = ready;
  if (us.version === "2024-11-05") {
    if (us.sse2024) await post2024(call, us.sse2024, msg);
    return;
  }
  const isNote = typeof (msg as Record<string, unknown>)["method"] === "string" && !("id" in msg);
  try {
    const resp = await authedFetch(call, call.server.url, {
      method: "POST",
      headers: requestHeaders(us.version, us, isNote ? (msg as JsonRpcNotification) : undefined),
      body: JSON.stringify(msg),
    });
    resp.body?.cancel().catch(() => {});
  } catch {
    // One-way messages are best-effort by definition.
  }
}

/** Open the legacy upstream's unsolicited-notification GET stream (may 405 — caller handles). */
export async function openUpstreamGetStream(call: UpstreamCall, lastEventId: string | null): Promise<Response | null> {
  const us = memory.upstream(call.session.sessionId, call.server.serverId);
  if (!us) return null;
  const headers: Record<string, string> = { accept: "text/event-stream" };
  if (BEHAVIOR[us.version].versionHeader) headers["mcp-protocol-version"] = us.version;
  if (us.mcpSessionId) headers["mcp-session-id"] = us.mcpSessionId;
  if (lastEventId !== null && BEHAVIOR[us.version].resumable) headers["last-event-id"] = lastEventId;
  try {
    return await authedFetch(call, call.server.url, { method: "GET", headers });
  } catch {
    return null;
  }
}

/** DELETE the upstream session on client teardown; 405 is a legal answer and ignored. */
export async function deleteUpstreamSession(call: UpstreamCall): Promise<void> {
  const us = memory.upstream(call.session.sessionId, call.server.serverId);
  if (!us) return;
  memory.dropUpstream(call.session.sessionId, call.server.serverId);
  if (!us.mcpSessionId) return;
  try {
    const resp = await authedFetch(call, call.server.url, {
      method: "DELETE",
      headers: { "mcp-session-id": us.mcpSessionId },
    });
    resp.body?.cancel().catch(() => {});
  } catch {
    // Teardown is best-effort; the upstream expires the session on its own clock.
  }
}

/** A gateway-authored error for upstream-failure answers, era-agnostic. */
export function upstreamFailureError(id: string | number | null, outcome: "upstream_error" | "unauthorized") {
  return rpcError(id, ERR_GATEWAY, outcome === "unauthorized" ? copy.upstreamAuthFailed() : copy.upstreamFailed());
}
