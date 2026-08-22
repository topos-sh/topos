/**
 * The session engine — `handleGatewayRequest` is the ONE entry point a deployment calls per
 * proxied HTTP request (ports.ts is the frozen contract around it).
 *
 * Check order per request, fixed: bearer (sha256 → active session, path must name it) →
 * connected server → tool policy → credential → upstream. The gateway impersonates the upstream
 * to the client (its serverInfo, its tool names verbatim) while terminating the protocol on both
 * sides: the client-facing revision and the upstream revision are negotiated independently and
 * bridged per the behavior table. Exactly one usage event per handled JSON-RPC request (each
 * member of a 2025-03-26 batch counts as one); pre-auth rejections cannot be attributed to a
 * workspace, so they are logged, never usage-recorded.
 *
 * Secrets discipline: the client's bearer is hashed and dropped — it never reaches upstream;
 * upstream headers never reach the client (every client-facing Response is built fresh); logs
 * carry no tokens, bodies, or header values.
 */

import type { GatewayContext, GatewayRequest, ResolvedServer, SessionRef, ToolPolicy, UsageOutcome } from "./ports";
import {
  BEHAVIOR,
  copy,
  ERR_GATEWAY,
  ERR_HEADER_MISMATCH,
  ERR_INVALID_PARAMS,
  ERR_INVALID_REQUEST,
  ERR_METHOD_NOT_FOUND,
  ERR_PARSE,
  ERR_UNSUPPORTED_VERSION,
  LATEST_LEGACY,
  META_SERVER_INFO,
  MODERN,
  REVISIONS,
  asJsonRpcMessage,
  decodeSentinelHeader,
  isNotification,
  isRequest,
  isResponse,
  isRevision,
  jsonResponse,
  mintId,
  readModernMeta,
  rpcError,
  rpcResult,
  sameEra,
  sha256Hex,
  type JsonRpcMessage,
  type JsonRpcNotification,
  type JsonRpcRequest,
  type Revision,
} from "./protocol";
import { mapResult, mapSseEvent, type BridgeContext } from "./bridge";
import { isToolAllowed } from "./filter";
import { sseTransform } from "./sse";
import { memory, type ClientSession, type LegacySseChannel, type UpstreamVerdict } from "./state";
import {
  deleteUpstreamSession,
  ensureUpstream,
  responseFromSse,
  sendUpstreamOneWay,
  sendUpstreamRequest,
  upstreamFailureError,
  type ClientIdentity,
  type UpstreamCall,
} from "./upstream";
import {
  attachUpstreamNotifications,
  localPingResult,
  routeToListenChannels,
  serve2024ClientStream,
  serveClientGetStream,
  serveModernListen,
} from "./channels";

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

interface RequestCtx {
  ctx: GatewayContext;
  greq: GatewayRequest;
  session: SessionRef;
  server: ResolvedServer;
  call: UpstreamCall;
  started: number;
  usageCount: number;
}

/** How the current client speaks: the era view every shared flow branches on. */
interface ClientView {
  version: Revision;
  identity: ClientIdentity;
  /** Correlation scope for notifications/cancelled (session id / stream token). */
  channelKey: string;
  wantsLog: boolean;
}

type Handled = { kind: "message"; message: JsonRpcMessage; status: number } | { kind: "http"; response: Response };

const SSE_HEADERS = { "content-type": "text/event-stream", "cache-control": "no-store", "x-accel-buffering": "no" };

function record(rc: RequestCtx, outcome: UsageOutcome, method: string, toolName: string | null = null): void {
  rc.usageCount += 1;
  try {
    rc.ctx.usage.record({
      workspaceId: rc.session.workspaceId,
      serverId: rc.server.serverId,
      sessionId: rc.session.sessionId,
      userId: rc.session.userId,
      toolName,
      method,
      outcome,
      durationMs: Math.max(0, rc.ctx.env.now() - rc.started),
    });
  } catch {
    // The sink promises not to throw; if it lies, the response path still wins.
  }
}

function noCredentialResponse(rc: RequestCtx, id: string | number | null): Response {
  return jsonResponse(
    rpcError(id, ERR_GATEWAY, copy.noCredential(rc.server.displayName, rc.server.workspaceDisplayName, rc.server.authorizeUrl)),
    401,
  );
}

/**
 * Resolve and attach the workspace credential. Returns the pinned 401 answer when the server
 * needs a sign-in nobody has connected; records the usage row itself in that case.
 */
async function credentialGate(rc: RequestCtx, id: string | number | null, method: string, toolName: string | null = null): Promise<Response | null> {
  const cred = await rc.ctx.store.credentialFor(rc.session.workspaceId, rc.server.serverId, rc.session.userId);
  rc.call.auth.credential = cred;
  rc.call.auth.secret = cred?.secret ?? null;
  if (cred === null && rc.server.authMode !== "none") {
    record(rc, "no_credential", method, toolName);
    return noCredentialResponse(rc, id);
  }
  return null;
}

function bridgeCapabilities(clientVersion: Revision, verdict: UpstreamVerdict): Record<string, unknown> {
  if (sameEra(clientVersion, verdict.version)) return verdict.capabilities;
  const tools = verdict.capabilities["tools"];
  return { tools: isRecord(tools) ? tools : {} };
}

function bridgeContext(rc: RequestCtx, view: ClientView, upstreamVersion: Revision, msg: JsonRpcRequest, policy: ToolPolicy | null, verdict: UpstreamVerdict): BridgeContext {
  return {
    ctx: rc.ctx,
    workspaceId: rc.session.workspaceId,
    serverId: rc.server.serverId,
    clientVersion: view.version,
    upstreamVersion,
    requestId: msg.id,
    requestMethod: msg.method,
    policy,
    clientWantsLog: view.wantsLog,
    serverInfo: verdict.serverInfo,
    answerUpstream: (m) => {
      void sendUpstreamOneWay(rc.call, m, view.identity);
    },
    routeListChanged: (method) => routeToListenChanged(rc, method),
  };
}

function routeToListenChanged(rc: RequestCtx, method: string): void {
  routeToListenChannels(rc.call, method);
}

/** JSON-RPC bodies ride 200 for legacy clients (their 404 means "dead session"); modern clients
 * get the modern status vocabulary (404 for method-not-found, mirrored same-era otherwise). */
function statusFor(view: ClientView, message: JsonRpcMessage, upstreamStatus: number, upstreamVersion: Revision): number {
  if (BEHAVIOR[view.version].handshake === "initialize") return 200;
  if (sameEra(view.version, upstreamVersion)) return upstreamStatus;
  if ("error" in message && message.error.code === ERR_METHOD_NOT_FOUND) return 404;
  return 200;
}

interface ReplyOptions {
  policy: ToolPolicy | null;
  toolName: string | null;
  unary: boolean;
  verdict: UpstreamVerdict;
}

/** Map one upstream reply envelope for the client; records the usage row. */
async function finishReply(
  rc: RequestCtx,
  view: ClientView,
  msg: JsonRpcRequest,
  reply: Awaited<ReturnType<typeof sendUpstreamRequest>>,
  opts: ReplyOptions,
): Promise<Handled> {
  if (reply.kind === "fail") {
    record(rc, reply.outcome, msg.method, opts.toolName);
    return {
      kind: "message",
      message: upstreamFailureError(msg.id, reply.outcome),
      status: reply.outcome === "unauthorized" ? 401 : 502,
    };
  }
  if (reply.kind === "accepted") {
    // A request answered 202-no-body is an upstream protocol breach — fail closed.
    record(rc, "upstream_error", msg.method, opts.toolName);
    return { kind: "message", message: upstreamFailureError(msg.id, "upstream_error"), status: 502 };
  }
  if (reply.kind === "sse" && opts.unary) {
    const extracted = await responseFromSse(reply.body, msg.id);
    if (!extracted) {
      record(rc, "upstream_error", msg.method, opts.toolName);
      return { kind: "message", message: upstreamFailureError(msg.id, "upstream_error"), status: 502 };
    }
    reply = { kind: "json", message: extracted, status: 200 };
  }
  if (reply.kind === "json") {
    const message = reply.message;
    if (isResponse(message) && "result" in message) {
      const bridged = bridgeContext(rc, view, opts.verdict.version, msg, opts.policy, opts.verdict);
      const mapped = await mapResult(bridged, message.id, message.result);
      if (mapped.kind === "error") {
        record(rc, mapped.outcome, msg.method, opts.toolName);
        return { kind: "message", message: mapped.error, status: mapped.outcome === "upstream_error" ? 502 : 200 };
      }
      record(rc, "ok", msg.method, opts.toolName);
      return { kind: "message", message: rpcResult(message.id, mapped.result), status: 200 };
    }
    // An upstream JSON-RPC error is a completed exchange — forwarded verbatim.
    record(rc, "ok", msg.method, opts.toolName);
    return { kind: "message", message, status: statusFor(view, message, reply.status, opts.verdict.version) };
  }
  // A live stream: the outcome is decided at pipe start; the transform maps event-by-event.
  record(rc, "ok", msg.method, opts.toolName);
  const bridged = bridgeContext(rc, view, opts.verdict.version, msg, opts.policy, opts.verdict);
  const transform = sseTransform((ev) => mapSseEvent(bridged, ev));
  return { kind: "http", response: new Response(reply.body.pipeThrough(transform), { status: 200, headers: SSE_HEADERS }) };
}

/** The shared request path: policy → credential → upstream → mapped reply. Records usage. */
async function handleRpcRequest(rc: RequestCtx, view: ClientView, msg: JsonRpcRequest, unary: boolean): Promise<Handled> {
  const legacyClient = BEHAVIOR[view.version].handshake === "initialize";

  if (legacyClient && msg.method === "ping") {
    record(rc, "ok", msg.method);
    return { kind: "message", message: localPingResult(msg.id), status: 200 };
  }
  if (legacyClient && msg.method === "initialize") {
    record(rc, "ok", msg.method);
    return { kind: "message", message: rpcError(msg.id, ERR_INVALID_REQUEST, "The session is already initialized."), status: 200 };
  }

  if (msg.method === "tools/call") {
    const name = msg.params?.["name"];
    if (typeof name !== "string") {
      record(rc, "ok", msg.method);
      return { kind: "message", message: rpcError(msg.id, ERR_INVALID_PARAMS, "The tools/call request names no tool."), status: 200 };
    }
    const policy = await rc.ctx.store.toolPolicy(rc.session.workspaceId, rc.server.serverId);
    if (!isToolAllowed(policy, name)) {
      // The pinned copy; the upstream is never contacted for a non-enabled tool.
      record(rc, "denied_tool", msg.method, name);
      return { kind: "message", message: rpcError(msg.id, ERR_INVALID_PARAMS, copy.toolNotEnabled(name)), status: 200 };
    }
    const gate = await credentialGate(rc, msg.id, msg.method, name);
    if (gate) return { kind: "http", response: gate };
    const ready = await ensureUpstream(rc.call, view.identity);
    if (!ready) {
      record(rc, "upstream_error", msg.method, name);
      return { kind: "message", message: upstreamFailureError(msg.id, "upstream_error"), status: 502 };
    }
    if (
      BEHAVIOR[ready.verdict.version].handshake === "initialize" &&
      (msg.params?.["inputResponses"] !== undefined || msg.params?.["requestState"] !== undefined)
    ) {
      // A modern input_required continuation has no legacy mapping — never invent one.
      record(rc, "ok", msg.method, name);
      return { kind: "message", message: rpcError(msg.id, ERR_GATEWAY, copy.crossEraInputRequired()), status: 200 };
    }
    const reply = await sendUpstreamRequest(rc.call, msg, {
      identity: view.identity,
      inflightKey: { channel: view.channelKey, id: msg.id },
    });
    return finishReply(rc, view, msg, reply, { policy: null, toolName: name, unary, verdict: ready.verdict });
  }

  if (msg.method === "tools/list") {
    const policy = await rc.ctx.store.toolPolicy(rc.session.workspaceId, rc.server.serverId);
    const gate = await credentialGate(rc, msg.id, msg.method);
    if (gate) return { kind: "http", response: gate };
    const ready = await ensureUpstream(rc.call, view.identity);
    if (!ready) {
      record(rc, "upstream_error", msg.method);
      return { kind: "message", message: upstreamFailureError(msg.id, "upstream_error"), status: 502 };
    }
    const reply = await sendUpstreamRequest(rc.call, msg, {
      identity: view.identity,
      inflightKey: { channel: view.channelKey, id: msg.id },
    });
    return finishReply(rc, view, msg, reply, { policy, toolName: null, unary, verdict: ready.verdict });
  }

  // Everything else: full-fidelity pass-through same-era; the plain-sentence refusal cross-era.
  const gate = await credentialGate(rc, msg.id, msg.method);
  if (gate) return { kind: "http", response: gate };
  const ready = await ensureUpstream(rc.call, view.identity);
  if (!ready) {
    record(rc, "upstream_error", msg.method);
    return { kind: "message", message: upstreamFailureError(msg.id, "upstream_error"), status: 502 };
  }
  if (!sameEra(view.version, ready.verdict.version)) {
    record(rc, "ok", msg.method);
    return {
      kind: "message",
      message: rpcError(msg.id, ERR_METHOD_NOT_FOUND, copy.crossEra(msg.method)),
      status: legacyClient ? 200 : 404,
    };
  }
  const reply = await sendUpstreamRequest(rc.call, msg, {
    identity: view.identity,
    inflightKey: { channel: view.channelKey, id: msg.id },
  });
  return finishReply(rc, view, msg, reply, { policy: null, toolName: null, unary, verdict: ready.verdict });
}

// -- client-facing protocol serving -----------------------------------------------------------

async function handleLegacyInitialize(rc: RequestCtx, msg: JsonRpcRequest): Promise<Response> {
  const params = msg.params ?? {};
  const requested = params["protocolVersion"];
  // Over Streamable HTTP the gateway serves the sessionful legacy revisions; anything else
  // (unknown, modern, or the 2024-11-05 transport mismatch) is answered with our latest legacy —
  // the client disconnects if it cannot accept it, per its own lifecycle rule.
  const chosen: Revision = isRevision(requested) && BEHAVIOR[requested].sessionHeader ? requested : LATEST_LEGACY;
  const identity: ClientIdentity = {
    capabilities: isRecord(params["capabilities"]) ? params["capabilities"] : {},
    clientInfo: params["clientInfo"],
    version: chosen,
  };
  const gate = await credentialGate(rc, msg.id, msg.method);
  if (gate) return gate;
  const ready = await ensureUpstream(rc.call, identity);
  if (!ready) {
    record(rc, "upstream_error", msg.method);
    return jsonResponse(upstreamFailureError(msg.id, "upstream_error"), 502);
  }
  const session: ClientSession = {
    mcpSessionId: mintId(),
    toposSessionId: rc.session.sessionId,
    serverId: rc.server.serverId,
    clientVersion: chosen,
    clientCapabilities: identity.capabilities,
    clientInfo: identity.clientInfo,
    initialized: false,
  };
  memory.clientSessions.set(session.mcpSessionId, session);
  record(rc, "ok", msg.method);
  return jsonResponse(
    rpcResult(msg.id, {
      protocolVersion: chosen,
      capabilities: bridgeCapabilities(chosen, ready.verdict),
      serverInfo: ready.verdict.serverInfo ?? { name: rc.server.displayName },
      ...(ready.verdict.instructions !== null ? { instructions: ready.verdict.instructions } : {}),
    }),
    200,
    { "mcp-session-id": session.mcpSessionId },
  );
}

function legacyView(cs: ClientSession): ClientView {
  return {
    version: cs.clientVersion,
    identity: { capabilities: cs.clientCapabilities, clientInfo: cs.clientInfo, version: cs.clientVersion },
    channelKey: cs.mcpSessionId,
    wantsLog: true,
  };
}

async function handleLegacyNotification(rc: RequestCtx, view: ClientView, msg: JsonRpcNotification, markInitialized: () => void): Promise<Response> {
  if (msg.method === "notifications/initialized") {
    markInitialized();
    record(rc, "ok", msg.method);
    return new Response(null, { status: 202 });
  }
  if (msg.method === "notifications/cancelled") {
    const requestId = msg.params?.["requestId"];
    if (typeof requestId === "string" || typeof requestId === "number") {
      memory.cancelInflight(view.channelKey, requestId);
    }
    // Toward a legacy upstream the cancellation is also a real notification; toward a modern
    // upstream the aborted connection IS the cancellation, so nothing more is sent.
    const verdict = memory.verdict(rc.server.serverId);
    if (verdict && sameEra(view.version, verdict.version)) void sendUpstreamOneWay(rc.call, msg, view.identity);
    record(rc, "ok", msg.method);
    return new Response(null, { status: 202 });
  }
  const verdict = memory.verdict(rc.server.serverId);
  if (verdict && sameEra(view.version, verdict.version)) {
    void sendUpstreamOneWay(rc.call, msg, view.identity);
  }
  record(rc, "ok", msg.method);
  return new Response(null, { status: 202 });
}

async function handleLegacySessionPost(rc: RequestCtx, cs: ClientSession, msg: JsonRpcMessage): Promise<Response> {
  const view = legacyView(cs);
  const versionHeader = rc.greq.request.headers.get("mcp-protocol-version");
  if (versionHeader !== null && !isRevision(versionHeader)) {
    record(rc, "ok", "method" in msg ? msg.method : "response");
    return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "The MCP-Protocol-Version header names an unsupported version."), 400);
  }
  if (isNotification(msg)) {
    return handleLegacyNotification(rc, view, msg, () => {
      cs.initialized = true;
    });
  }
  if (isResponse(msg)) {
    // The client answering a server-initiated request we forwarded (same-era only).
    const verdict = memory.verdict(rc.server.serverId);
    if (verdict && sameEra(view.version, verdict.version)) {
      void sendUpstreamOneWay(rc.call, msg as unknown as Record<string, unknown>, view.identity);
    }
    record(rc, "ok", "response");
    return new Response(null, { status: 202 });
  }
  const handled = await handleRpcRequest(rc, view, msg, false);
  return handled.kind === "http" ? handled.response : jsonResponse(handled.message, handled.status);
}

// -- 2026-07-28 client serving ----------------------------------------------------------------

function modernHeaderFailure(rc: RequestCtx, msg: JsonRpcRequest): Response | null {
  const headers = rc.greq.request.headers;
  const meta = readModernMeta(msg.params);
  if (!meta) return null; // Caller guarantees presence; narrow for types.
  const headerVersion = headers.get("mcp-protocol-version");
  if (headerVersion === null || headerVersion !== meta.version) {
    record(rc, "ok", msg.method);
    return jsonResponse(
      rpcError(msg.id, ERR_HEADER_MISMATCH, "The MCP-Protocol-Version header does not match the request body."),
      400,
    );
  }
  if (meta.version !== MODERN) {
    record(rc, "ok", msg.method);
    return jsonResponse(
      rpcError(msg.id, ERR_UNSUPPORTED_VERSION, "The requested protocol version is not supported.", {
        supported: [...REVISIONS],
        requested: meta.version,
      }),
      400,
    );
  }
  if (meta.capabilities === null) {
    record(rc, "ok", msg.method);
    return jsonResponse(rpcError(msg.id, ERR_INVALID_PARAMS, "The request is missing required _meta fields."), 400);
  }
  const methodHeader = headers.get("mcp-method");
  if (methodHeader === null || methodHeader !== msg.method) {
    record(rc, "ok", msg.method);
    return jsonResponse(rpcError(msg.id, ERR_HEADER_MISMATCH, "The Mcp-Method header does not match the request body."), 400);
  }
  const namedField = msg.method === "tools/call" || msg.method === "prompts/get" ? "name" : msg.method === "resources/read" ? "uri" : null;
  if (namedField !== null) {
    const bodyValue = msg.params?.[namedField];
    const nameHeader = headers.get("mcp-name");
    const decoded = nameHeader === null ? null : decodeSentinelHeader(nameHeader);
    if (typeof bodyValue !== "string" || decoded === null || decoded !== bodyValue) {
      record(rc, "ok", msg.method);
      return jsonResponse(rpcError(msg.id, ERR_HEADER_MISMATCH, "The Mcp-Name header does not match the request body."), 400);
    }
  }
  return null;
}

async function handleModernPost(rc: RequestCtx, msg: JsonRpcRequest): Promise<Response> {
  const failure = modernHeaderFailure(rc, msg);
  if (failure) return failure;
  const meta = readModernMeta(msg.params);
  const view: ClientView = {
    version: MODERN,
    identity: { capabilities: meta?.capabilities ?? {}, clientInfo: meta?.clientInfo, version: MODERN },
    channelKey: `${rc.session.sessionId} ${rc.server.serverId}`,
    wantsLog: meta?.logLevel ?? false,
  };

  if (msg.method === "server/discover") {
    const gate = await credentialGate(rc, msg.id, msg.method);
    if (gate) return gate;
    const ready = await ensureUpstream(rc.call, view.identity);
    if (!ready) {
      record(rc, "upstream_error", msg.method);
      return jsonResponse(upstreamFailureError(msg.id, "upstream_error"), 502);
    }
    record(rc, "ok", msg.method);
    return jsonResponse(
      rpcResult(msg.id, {
        supportedVersions: [...REVISIONS],
        capabilities: bridgeCapabilities(MODERN, ready.verdict),
        ...(ready.verdict.instructions !== null ? { instructions: ready.verdict.instructions } : {}),
        resultType: "complete",
        // The gateway's view is policy- and credential-scoped: never shared-cacheable.
        ttlMs: 0,
        cacheScope: "private",
        ...(ready.verdict.serverInfo ? { _meta: { [META_SERVER_INFO]: ready.verdict.serverInfo } } : {}),
      }),
    );
  }

  if (msg.method === "subscriptions/listen") {
    const gate = await credentialGate(rc, msg.id, msg.method);
    if (gate) return gate;
    const ready = await ensureUpstream(rc.call, view.identity);
    if (!ready) {
      record(rc, "upstream_error", msg.method);
      return jsonResponse(upstreamFailureError(msg.id, "upstream_error"), 502);
    }
    const served = await serveModernListen(rc.call, { id: msg.id, ...(msg.params ? { params: msg.params } : {}) }, view.identity);
    if (!served) {
      record(rc, "upstream_error", msg.method);
      return jsonResponse(upstreamFailureError(msg.id, "upstream_error"), 502);
    }
    record(rc, "ok", msg.method);
    return served;
  }

  const handled = await handleRpcRequest(rc, view, msg, false);
  return handled.kind === "http" ? handled.response : jsonResponse(handled.message, handled.status);
}

// -- 2024-11-05 client serving (the HTTP+SSE transport) ---------------------------------------

async function handle2024Post(rc: RequestCtx, channel: LegacySseChannel, msg: JsonRpcMessage): Promise<Response> {
  const view: ClientView = {
    version: "2024-11-05",
    identity: { capabilities: channel.clientCapabilities, clientInfo: channel.clientInfo, version: "2024-11-05" },
    channelKey: channel.token,
    wantsLog: true,
  };
  if (isNotification(msg)) {
    return handleLegacyNotification(rc, view, msg, () => {
      channel.initialized = true;
    });
  }
  if (isResponse(msg)) {
    const verdict = memory.verdict(rc.server.serverId);
    if (verdict && sameEra(view.version, verdict.version)) {
      void sendUpstreamOneWay(rc.call, msg as unknown as Record<string, unknown>, view.identity);
    }
    record(rc, "ok", "response");
    return new Response(null, { status: 202 });
  }

  if (msg.method === "initialize") {
    const params = msg.params ?? {};
    channel.clientCapabilities = isRecord(params["capabilities"]) ? params["capabilities"] : {};
    channel.clientInfo = params["clientInfo"];
    const identity: ClientIdentity = { capabilities: channel.clientCapabilities, clientInfo: channel.clientInfo, version: "2024-11-05" };
    const gate = await credentialGate(rc, msg.id, msg.method);
    if (gate) return gate;
    const ready = await ensureUpstream(rc.call, identity);
    if (!ready) {
      record(rc, "upstream_error", msg.method);
      channel.writer.send({ event: "message", data: JSON.stringify(upstreamFailureError(msg.id, "upstream_error")) });
      return new Response(null, { status: 202 });
    }
    channel.detachUpstream?.();
    channel.detachUpstream = await attachUpstreamNotifications(rc.call, "2024-11-05", identity, (m) => {
      channel.writer.send({ event: "message", data: JSON.stringify(m) });
    });
    record(rc, "ok", msg.method);
    channel.writer.send({
      event: "message",
      data: JSON.stringify(
        rpcResult(msg.id, {
          protocolVersion: "2024-11-05",
          capabilities: bridgeCapabilities("2024-11-05", ready.verdict),
          serverInfo: ready.verdict.serverInfo ?? { name: rc.server.displayName },
          ...(ready.verdict.instructions !== null ? { instructions: ready.verdict.instructions } : {}),
        }),
      ),
    });
    return new Response(null, { status: 202 });
  }

  // On this transport every server message rides the stream; the POST itself is just accepted.
  const handled = await handleRpcRequest(rc, view, msg, true);
  if (handled.kind === "http") return handled.response;
  channel.writer.send({ event: "message", data: JSON.stringify(handled.message) });
  return new Response(null, { status: 202 });
}

// -- HTTP dispatch ----------------------------------------------------------------------------

async function handlePost(rc: RequestCtx): Promise<Response> {
  const url = new URL(rc.greq.request.url);
  const sseToken = url.searchParams.get("sse");

  let parsed: unknown;
  try {
    parsed = await rc.greq.request.json();
  } catch {
    record(rc, "ok", "POST");
    return jsonResponse(rpcError(null, ERR_PARSE, "The request body is not valid JSON."), 400);
  }

  if (sseToken !== null) {
    const channel = memory.legacyChannels.get(sseToken);
    if (!channel || channel.toposSessionId !== rc.session.sessionId || channel.serverId !== rc.server.serverId || channel.writer.closed) {
      record(rc, "ok", "POST");
      return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "The stream this request addresses is not open."), 404);
    }
    const msg = asJsonRpcMessage(parsed);
    if (!msg) {
      record(rc, "ok", "POST");
      return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "The request body is not a JSON-RPC message."), 400);
    }
    return handle2024Post(rc, channel, msg);
  }

  if (Array.isArray(parsed)) return handleBatch(rc, parsed);

  const msg = asJsonRpcMessage(parsed);
  if (!msg) {
    record(rc, "ok", "POST");
    return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "The request body is not a JSON-RPC message."), 400);
  }

  if ("method" in msg && readModernMeta(msg.params) !== null) {
    if (isNotification(msg)) {
      // 2026-07-28 defines no client→server HTTP notification requirements — accepted, absorbed.
      record(rc, "ok", msg.method);
      return new Response(null, { status: 202 });
    }
    return handleModernPost(rc, msg);
  }

  if (isRequest(msg) && msg.method === "initialize") {
    return handleLegacyInitialize(rc, msg);
  }

  const sessionHeader = rc.greq.request.headers.get("mcp-session-id");
  if (sessionHeader !== null) {
    const cs = memory.clientSessions.get(sessionHeader);
    if (!cs || cs.toposSessionId !== rc.session.sessionId || cs.serverId !== rc.server.serverId) {
      // Dead or foreign session: 404 tells a legacy client to re-initialize.
      record(rc, "ok", "method" in msg ? msg.method : "response");
      return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "The session this request names is not open."), 404);
    }
    return handleLegacySessionPost(rc, cs, msg);
  }

  record(rc, "ok", "method" in msg ? msg.method : "response");
  if (isResponse(msg)) {
    return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "This request carries a response outside any session."), 400);
  }
  return jsonResponse(rpcError(isRequest(msg) ? msg.id : null, ERR_INVALID_REQUEST, "The request carries no Mcp-Session-Id."), 400);
}

async function handleBatch(rc: RequestCtx, entries: unknown[]): Promise<Response> {
  const sessionHeader = rc.greq.request.headers.get("mcp-session-id");
  const cs = sessionHeader !== null ? memory.clientSessions.get(sessionHeader) : undefined;
  if (!cs || cs.toposSessionId !== rc.session.sessionId || cs.serverId !== rc.server.serverId) {
    record(rc, "ok", "POST");
    return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "A batch request needs an initialized session."), cs === undefined && sessionHeader !== null ? 404 : 400);
  }
  if (!BEHAVIOR[cs.clientVersion].batching || entries.length === 0) {
    record(rc, "ok", "POST");
    return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "JSON-RPC batching is not supported in this protocol revision."), 400);
  }
  const view = legacyView(cs);
  const replies: JsonRpcMessage[] = [];
  for (const entry of entries) {
    const msg = asJsonRpcMessage(entry);
    if (!msg) {
      record(rc, "ok", "POST");
      replies.push(rpcError(null, ERR_INVALID_REQUEST, "A batch entry is not a JSON-RPC message."));
      continue;
    }
    if (isNotification(msg)) {
      await handleLegacyNotification(rc, view, msg, () => {
        cs.initialized = true;
      });
      continue;
    }
    if (isResponse(msg)) {
      const verdict = memory.verdict(rc.server.serverId);
      if (verdict && sameEra(view.version, verdict.version)) {
        void sendUpstreamOneWay(rc.call, msg as unknown as Record<string, unknown>, view.identity);
      }
      record(rc, "ok", "response");
      continue;
    }
    if (msg.method === "initialize") {
      // 2025-03-26 forbids initialize inside a batch.
      record(rc, "ok", msg.method);
      replies.push(rpcError(msg.id, ERR_INVALID_REQUEST, "The initialize request must not be part of a batch."));
      continue;
    }
    const handled = await handleRpcRequest(rc, view, msg, true);
    if (handled.kind === "http") return handled.response; // The pinned 401 answers stop the batch.
    replies.push(handled.message);
  }
  if (replies.length === 0) return new Response(null, { status: 202 });
  return jsonResponse(replies, 200);
}

async function handleGet(rc: RequestCtx): Promise<Response> {
  const sessionHeader = rc.greq.request.headers.get("mcp-session-id");
  if (sessionHeader !== null) {
    const cs = memory.clientSessions.get(sessionHeader);
    if (!cs || cs.toposSessionId !== rc.session.sessionId || cs.serverId !== rc.server.serverId) {
      record(rc, "ok", "GET");
      return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "The session this request names is not open."), 404);
    }
    const gate = await credentialGate(rc, null, "GET");
    if (gate) return gate;
    const ready = await ensureUpstream(rc.call, legacyView(cs).identity);
    if (!ready) {
      record(rc, "upstream_error", "GET");
      return jsonResponse(upstreamFailureError(null, "upstream_error"), 502);
    }
    record(rc, "ok", "GET");
    return serveClientGetStream(rc.call, cs.clientVersion, legacyView(cs).identity);
  }
  // No session: the 2024-11-05 transport probe — serve the endpoint-event stream. (A modern
  // client never issues GET; the old transport is the only meaning left for one.)
  record(rc, "ok", "GET");
  return serve2024ClientStream(rc.greq.request.url, rc.session.sessionId, rc.server.serverId, mintId);
}

async function handleDelete(rc: RequestCtx): Promise<Response> {
  const sessionHeader = rc.greq.request.headers.get("mcp-session-id");
  if (sessionHeader === null) {
    record(rc, "ok", "DELETE");
    return new Response(null, { status: 405 });
  }
  const cs = memory.clientSessions.get(sessionHeader);
  if (!cs || cs.toposSessionId !== rc.session.sessionId || cs.serverId !== rc.server.serverId) {
    record(rc, "ok", "DELETE");
    return jsonResponse(rpcError(null, ERR_INVALID_REQUEST, "The session this request names is not open."), 404);
  }
  memory.clientSessions.delete(cs.mcpSessionId);
  // Tear down the upstream conversation too, best-effort (needs the credential to speak).
  const cred = await rc.ctx.store.credentialFor(rc.session.workspaceId, rc.server.serverId, rc.session.userId);
  rc.call.auth.credential = cred;
  rc.call.auth.secret = cred?.secret ?? null;
  await deleteUpstreamSession(rc.call);
  record(rc, "ok", "DELETE");
  return new Response(null, { status: 200 });
}

// -- the entry point --------------------------------------------------------------------------

export async function handleGatewayRequest(ctx: GatewayContext, greq: GatewayRequest): Promise<Response> {
  const started = ctx.env.now();
  const { request } = greq;

  // Origin validation (MUST, all revisions): gateway callers are headless agents; a request
  // carrying any browser Origin is a DNS-rebinding shape and is refused outright.
  if (request.headers.get("origin") !== null) {
    ctx.env.log("warn", "request with Origin header refused", { serverId: greq.serverId });
    return jsonResponse(rpcError(null, ERR_GATEWAY, copy.unauthorized()), 403);
  }

  // Pre-auth failures have no workspace to attribute a usage row to: logged, never recorded.
  if (greq.bearer === null || greq.bearer === "") {
    ctx.env.log("warn", "request without bearer refused", { serverId: greq.serverId });
    return jsonResponse(rpcError(null, ERR_GATEWAY, copy.unauthorized()), 401);
  }
  const session = await ctx.store.sessionByTokenSha256(await sha256Hex(greq.bearer));
  if (session === null) {
    ctx.env.log("warn", "unknown bearer refused", { serverId: greq.serverId });
    return jsonResponse(rpcError(null, ERR_GATEWAY, copy.unauthorized()), 401);
  }

  const rc: RequestCtx = {
    ctx,
    greq,
    session,
    server: null as unknown as ResolvedServer, // Set below before any use.
    call: null as unknown as UpstreamCall,
    started,
    usageCount: 0,
  };

  if (session.sessionId !== greq.sessionId) {
    // A valid bearer under a foreign path is attributable — and refused.
    rc.server = {
      serverId: greq.serverId,
      displayName: "",
      url: "",
      transport: "",
      authMode: null,
      secretHeader: null,
      authorizeUrl: "",
      workspaceDisplayName: "",
    };
    record(rc, "unauthorized", request.method);
    return jsonResponse(rpcError(null, ERR_GATEWAY, copy.unauthorized()), 401);
  }

  const server = await ctx.store.connectedServer(session.workspaceId, greq.serverId);
  if (server === null) {
    rc.server = {
      serverId: greq.serverId,
      displayName: "",
      url: "",
      transport: "",
      authMode: null,
      secretHeader: null,
      authorizeUrl: "",
      workspaceDisplayName: "",
    };
    record(rc, "unauthorized", request.method);
    return jsonResponse(rpcError(null, ERR_GATEWAY, copy.unknownServer()), 404);
  }
  rc.server = server;
  rc.call = { ctx, session, server, auth: { credential: null, secret: null, refreshed: false } };

  try {
    let response: Response;
    switch (request.method) {
      case "POST":
        response = await handlePost(rc);
        break;
      case "GET":
        response = await handleGet(rc);
        break;
      case "DELETE":
        response = await handleDelete(rc);
        break;
      default:
        response = new Response(null, { status: 405 });
        break;
    }
    if (rc.usageCount === 0) record(rc, "ok", request.method);
    ctx.env.log("info", "gateway request handled", {
      serverId: server.serverId,
      method: request.method,
      status: response.status,
    });
    return response;
  } catch (err) {
    if (rc.usageCount === 0) record(rc, "upstream_error", request.method);
    ctx.env.log("error", "gateway request failed", {
      serverId: server.serverId,
      method: request.method,
      error: err instanceof Error ? err.name : "unknown",
    });
    return jsonResponse(rpcError(null, ERR_GATEWAY, copy.upstreamFailed()), 500);
  }
}
