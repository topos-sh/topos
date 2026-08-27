/**
 * Shared fakes for the core engine suites: an in-memory store/usage pair, a routing fetch, and
 * fake upstream MCP servers implementing each revision's transport quirks (the parts a gateway
 * can get wrong): 2025-03-26 sessions+batching, 2025-06-18 version-header enforcement,
 * 2026-07-28 statelessness+header validation, 2024-11-05 two-endpoint HTTP+SSE.
 */

import type {
  Credential,
  GatewayContext,
  GatewayRequest,
  GatewayStore,
  ObservedTool,
  ResolvedServer,
  SessionRef,
  ToolPolicy,
  UsageEvent,
  UsageSink,
} from "../core/ports";
import { sha256Hex } from "../core/protocol";

export class FakeUsage implements UsageSink {
  events: UsageEvent[] = [];
  record(event: UsageEvent): void {
    this.events.push(event);
  }
}

export class FakeStore implements GatewayStore {
  sessions = new Map<string, SessionRef>();
  /** Machine-token callers, keyed by `${bearerHash}|${serviceSessionId}` — a token names one
   *  runner only together with the address it was delivered for. */
  machineSessions = new Map<string, SessionRef>();
  servers = new Map<string, ResolvedServer>();
  policies = new Map<string, ToolPolicy>();
  credentials = new Map<string, Credential>();
  observed: { workspaceId: string; serverId: string; tools: ObservedTool[] }[] = [];
  rotations: { id: string; secret: string; refreshToken?: string }[] = [];
  lockAcquisitions: string[] = [];
  seenTokenHashes: string[] = [];
  private locks = new Map<string, Promise<unknown>>();

  async addBearer(bearer: string, ref: SessionRef): Promise<void> {
    this.sessions.set(await sha256Hex(bearer), ref);
  }
  /** A workspace machine token: one bearer, one live service session per runner. */
  async addMachineBearer(bearer: string, ref: SessionRef): Promise<void> {
    const hash = await sha256Hex(bearer);
    this.machineSessions.set(`${hash}|${ref.sessionId}`, { ...ref, userId: null });
  }
  setServer(workspaceId: string, server: ResolvedServer): void {
    this.servers.set(`${workspaceId}|${server.serverId}`, server);
  }
  setPolicy(workspaceId: string, serverId: string, policy: ToolPolicy): void {
    this.policies.set(`${workspaceId}|${serverId}`, policy);
  }
  setCredential(workspaceId: string, serverId: string, userId: string | null, cred: Credential): void {
    this.credentials.set(`${workspaceId}|${serverId}|${userId ?? ""}`, cred);
  }
  clearCredentials(): void {
    this.credentials.clear();
  }

  async sessionByTokenSha256(hex: string): Promise<SessionRef | null> {
    this.seenTokenHashes.push(hex);
    return this.sessions.get(hex) ?? null;
  }
  async machineSessionByTokenSha256(
    hex: string,
    serviceSessionId: string,
  ): Promise<SessionRef | null> {
    return this.machineSessions.get(`${hex}|${serviceSessionId}`) ?? null;
  }
  async connectedServer(workspaceId: string, serverId: string): Promise<ResolvedServer | null> {
    return this.servers.get(`${workspaceId}|${serverId}`) ?? null;
  }
  async toolPolicy(workspaceId: string, serverId: string): Promise<ToolPolicy> {
    return this.policies.get(`${workspaceId}|${serverId}`) ?? { mode: "all", selected: new Set() };
  }
  async credentialFor(workspaceId: string, serverId: string, userId: string | null): Promise<Credential | null> {
    return (
      this.credentials.get(`${workspaceId}|${serverId}|${userId ?? ""}`) ??
      this.credentials.get(`${workspaceId}|${serverId}|`) ??
      null
    );
  }
  async saveRotatedCredential(id: string, next: { secret: string; refreshToken?: string }): Promise<void> {
    this.rotations.push({ id, ...next });
    for (const [key, cred] of this.credentials) {
      if (cred.id === id) {
        this.credentials.set(key, { ...cred, secret: next.secret, ...(next.refreshToken ? { refreshToken: next.refreshToken } : {}) });
      }
    }
  }
  async withRefreshLock<T>(credentialId: string, fn: () => Promise<T>): Promise<T> {
    this.lockAcquisitions.push(credentialId);
    const prev = this.locks.get(credentialId) ?? Promise.resolve();
    let release!: () => void;
    const gate = new Promise<void>((resolve) => (release = resolve));
    this.locks.set(
      credentialId,
      prev.then(() => gate),
    );
    await prev;
    try {
      return await fn();
    } finally {
      release();
    }
  }
  async recordObservedTools(workspaceId: string, serverId: string, tools: readonly ObservedTool[]): Promise<void> {
    this.observed.push({ workspaceId, serverId, tools: [...tools] });
  }
}

export interface LogEntry {
  level: string;
  message: string;
  fields: Record<string, string | number>;
}

export interface TestCtx {
  ctx: GatewayContext;
  store: FakeStore;
  usage: FakeUsage;
  logs: LogEntry[];
}

export type FetchHandler = (req: Request) => Promise<Response> | Response;

export function makeCtx(store: FakeStore, usage: FakeUsage, routes: Record<string, FetchHandler>): TestCtx {
  const logs: LogEntry[] = [];
  let clock = 1_000;
  const fetchImpl = (async (input: RequestInfo | URL, init?: RequestInit) => {
    const req = new Request(input, init);
    for (const [prefix, handler] of Object.entries(routes)) {
      if (req.url.startsWith(prefix)) return handler(req);
    }
    return new Response("no such upstream", { status: 599 });
  }) as typeof fetch;
  return {
    store,
    usage,
    logs,
    ctx: {
      store,
      usage,
      env: {
        fetch: fetchImpl,
        now: () => (clock += 7),
        log: (level, message, fields) => logs.push({ level, message, fields: fields ?? {} }),
      },
    },
  };
}

// -- default seed -----------------------------------------------------------------------------

export const WS = "ws1";
export const USER = "user1";
export const TOPOS_SESSION = "sn_laptop";
export const SERVER_ID = "srv1";
export const BEARER = "tok-abc-standing";
export const UPSTREAM_URL = "https://up.test/mcp";

export function sessionRef(): SessionRef {
  return { sessionId: TOPOS_SESSION, workspaceId: WS, userId: USER, displayName: "robert's laptop" };
}

export function resolvedServer(overrides: Partial<ResolvedServer> = {}): ResolvedServer {
  return {
    serverId: SERVER_ID,
    displayName: "Linear",
    url: UPSTREAM_URL,
    transport: "streamable-http",
    authMode: "oauth",
    secretHeader: null,
    authorizeUrl: "https://topos.example/w/acme/mcp/srv1",
    workspaceDisplayName: "Acme",
    ...overrides,
  };
}

export function oauthCredential(overrides: Partial<Credential> = {}): Credential {
  return {
    id: "cred_1",
    kind: "oauth",
    secret: "up-secret-1",
    refreshToken: "refresh-1",
    tokenEndpoint: "https://auth.test/token",
    clientId: "client-1",
    ...overrides,
  };
}

export function gwRequest(opts: {
  bearer?: string | null;
  sessionId?: string;
  serverId?: string;
  method?: string;
  body?: unknown;
  headers?: Record<string, string>;
  url?: string;
}): GatewayRequest {
  const sessionId = opts.sessionId ?? TOPOS_SESSION;
  const serverId = opts.serverId ?? SERVER_ID;
  const url = opts.url ?? `https://gw.test/${sessionId}/${serverId}`;
  const method = opts.method ?? "POST";
  const request = new Request(url, {
    method,
    headers: opts.headers ?? {},
    ...(opts.body !== undefined ? { body: JSON.stringify(opts.body) } : {}),
  });
  return { sessionId, serverId, request, bearer: opts.bearer === undefined ? BEARER : opts.bearer };
}

// -- SSE utilities ----------------------------------------------------------------------------

export function pushStream(): {
  stream: ReadableStream<Uint8Array>;
  event: (name: string | null, data: string) => void;
  raw: (text: string) => void;
  close: () => void;
  cancelled: () => boolean;
} {
  const encoder = new TextEncoder();
  let controller: ReadableStreamDefaultController<Uint8Array> | null = null;
  let wasCancelled = false;
  const stream = new ReadableStream<Uint8Array>({
    start(c) {
      controller = c;
    },
    cancel() {
      wasCancelled = true;
    },
  });
  return {
    stream,
    cancelled: () => wasCancelled,
    event(name, data) {
      const head = name === null ? "" : `event: ${name}\n`;
      try {
        controller?.enqueue(encoder.encode(`${head}data: ${data}\n\n`));
      } catch {
        // Closed by the consumer — pushes after teardown are the test's own business.
      }
    },
    raw(text) {
      try {
        controller?.enqueue(encoder.encode(text));
      } catch {
        // Same.
      }
    },
    close() {
      try {
        controller?.close();
      } catch {
        // Already closed.
      }
    },
  };
}

export interface ParsedEvent {
  event: string | null;
  data: string;
}

/** Read SSE events off a Response body one at a time (no buffering of the whole stream). */
export function eventReader(body: ReadableStream<Uint8Array>): {
  next: () => Promise<ParsedEvent | null>;
  cancel: () => Promise<void>;
} {
  const reader = body.getReader();
  const decoder = new TextDecoder();
  let buffer = "";
  const pending: ParsedEvent[] = [];
  const drain = () => {
    for (;;) {
      const idx = buffer.indexOf("\n\n");
      if (idx === -1) return;
      const block = buffer.slice(0, idx);
      buffer = buffer.slice(idx + 2);
      let event: string | null = null;
      const data: string[] = [];
      for (const line of block.split("\n")) {
        if (line.startsWith("event:")) event = line.slice(6).trim();
        else if (line.startsWith("data:")) data.push(line.slice(5).replace(/^ /, ""));
      }
      if (event !== null || data.length > 0) pending.push({ event, data: data.join("\n") });
    }
  };
  return {
    async next() {
      for (;;) {
        if (pending.length > 0) return pending.shift() ?? null;
        const { done, value } = await reader.read();
        if (done) return null;
        buffer += decoder.decode(value, { stream: true });
        drain();
      }
    },
    async cancel() {
      await reader.cancel().catch(() => {});
    },
  };
}

// -- fake upstream servers --------------------------------------------------------------------

export interface CapturedCall {
  method: string;
  httpMethod: string;
  headers: Record<string, string>;
  body: unknown;
}

export interface ToolSpec {
  name: string;
  description?: string;
}

const DEFAULT_TOOLS: ToolSpec[] = [
  { name: "issue_create", description: "Create an issue" },
  { name: "issue_delete", description: "Delete an issue" },
  { name: "issue_list", description: "List issues" },
];

function captureHeaders(req: Request): Record<string, string> {
  const out: Record<string, string> = {};
  req.headers.forEach((value, key) => {
    out[key] = value;
  });
  return out;
}

interface AuthOpts {
  secret?: string | null;
  secretHeader?: string | null;
}

function authorized(req: Request, opts: AuthOpts): boolean {
  if (opts.secret === undefined || opts.secret === null) return true;
  if (opts.secretHeader) return req.headers.get(opts.secretHeader) === opts.secret;
  return req.headers.get("authorization") === `Bearer ${opts.secret}`;
}

function json(body: unknown, status = 200, headers: Record<string, string> = {}): Response {
  return new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", ...headers } });
}

function toolsResult(tools: ToolSpec[]): Record<string, unknown> {
  return { tools: tools.map((t) => ({ name: t.name, description: t.description ?? "", inputSchema: { type: "object" } })) };
}

function callResult(params: Record<string, unknown> | undefined): Record<string, unknown> {
  return { content: [{ type: "text", text: JSON.stringify({ name: params?.["name"], arguments: params?.["arguments"] ?? {} }) }], isError: false };
}

export interface LegacyFakeOpts extends AuthOpts {
  tools?: ToolSpec[];
  /** Answer tools/* requests over an SSE response stream instead of a JSON body. */
  sseReplies?: boolean;
  /** Refuse the GET notification stream with 405. */
  noGetStream?: boolean;
  serverInfo?: Record<string, unknown>;
}

export interface LegacyFake {
  handler: FetchHandler;
  calls: CapturedCall[];
  initCount: number;
  toolCallCount: number;
  acceptedPosts: unknown[];
  /** Push onto every open GET stream. */
  notify: (data: string) => void;
  openGetStreams: number;
  /** Kill every server-side session (the client's next call meets a 404). */
  dropSessions: () => void;
}

/** A 2025-03-26 or 2025-06-18/11-25 Streamable HTTP server, per the `version` argument. */
export function legacyFake(version: "2025-03-26" | "2025-06-18" | "2025-11-25", opts: LegacyFakeOpts = {}): LegacyFake {
  const tools = opts.tools ?? DEFAULT_TOOLS;
  const requiresVersionHeader = version !== "2025-03-26";
  const sessions = new Set<string>();
  const getStreams: ReturnType<typeof pushStream>[] = [];
  let counter = 0;
  const fake: LegacyFake = {
    calls: [],
    initCount: 0,
    toolCallCount: 0,
    acceptedPosts: [],
    openGetStreams: 0,
    notify(data) {
      for (const s of getStreams) s.event("message", data);
    },
    dropSessions() {
      sessions.clear();
    },
    handler: async (req) => {
      if (!authorized(req, opts)) return new Response(null, { status: 401 });
      if (req.method === "GET") {
        if (opts.noGetStream) return new Response(null, { status: 405 });
        const sid = req.headers.get("mcp-session-id");
        if (sid === null || !sessions.has(sid)) return new Response(null, { status: 400 });
        const push = pushStream();
        getStreams.push(push);
        fake.openGetStreams += 1;
        return new Response(push.stream, { status: 200, headers: { "content-type": "text/event-stream" } });
      }
      if (req.method === "DELETE") {
        const sid = req.headers.get("mcp-session-id");
        if (sid !== null) sessions.delete(sid);
        return new Response(null, { status: 200 });
      }
      if (req.method !== "POST") return new Response(null, { status: 405 });
      const parsed: unknown = await req.json().catch(() => null);
      if (parsed === null) return new Response(null, { status: 400 });

      const headers = captureHeaders(req);
      const versionHeader = req.headers.get("mcp-protocol-version");
      if (versionHeader !== null && versionHeader !== version && versionHeader !== "2025-03-26") {
        // ≥2025-06-18 rule: an invalid/unsupported version header is a plain 400.
        return new Response(null, { status: 400 });
      }

      const handleOne = (msg: Record<string, unknown>): Record<string, unknown> | null => {
        const method = msg["method"];
        const id = msg["id"];
        if (typeof method !== "string") {
          fake.acceptedPosts.push(msg);
          return null; // A client response — accepted.
        }
        fake.calls.push({ method, httpMethod: "POST", headers, body: msg });
        if (id === undefined || id === null) return null; // Notification.
        const params = msg["params"] as Record<string, unknown> | undefined;
        if (method === "initialize") {
          fake.initCount += 1;
          const sid = `up-sess-${(counter += 1)}`;
          sessions.add(sid);
          // Session header is attached by the outer POST handling below via a marker.
          return {
            jsonrpc: "2.0",
            id,
            result: {
              protocolVersion: version,
              capabilities: { tools: { listChanged: true }, resources: {} },
              serverInfo: opts.serverInfo ?? { name: `upstream-${version}`, version: "9.9" },
              instructions: "be kind to the API",
            },
            _newSession: sid,
          };
        }
        const sid = req.headers.get("mcp-session-id");
        if (sid === null) return { jsonrpc: "2.0", id, error: { code: -32000, message: "no session" }, _status: 400 };
        if (!sessions.has(sid)) return { jsonrpc: "2.0", id, error: { code: -32000, message: "dead session" }, _status: 404 };
        if (requiresVersionHeader && versionHeader === null) {
          // These revisions assume 2025-03-26 on a missing header — tolerated, recorded above.
        }
        if (method === "tools/list") return { jsonrpc: "2.0", id, result: toolsResult(tools) };
        if (method === "tools/call") {
          fake.toolCallCount += 1;
          return { jsonrpc: "2.0", id, result: callResult(params) };
        }
        if (method === "resources/list") return { jsonrpc: "2.0", id, result: { resources: [{ uri: "res://a", name: "a" }] } };
        if (method === "sampling/echo-request") return { jsonrpc: "2.0", id, result: {} };
        return { jsonrpc: "2.0", id, error: { code: -32601, message: `Method not found: ${method}` } };
      };

      if (Array.isArray(parsed)) {
        if (version !== "2025-03-26") return new Response(null, { status: 400 });
        const replies = parsed
          .map((entry) => handleOne(entry as Record<string, unknown>))
          .filter((r): r is Record<string, unknown> => r !== null)
          .map(({ _newSession, _status, ...rest }) => rest);
        return replies.length === 0 ? new Response(null, { status: 202 }) : json(replies);
      }
      const reply = handleOne(parsed as Record<string, unknown>);
      if (reply === null) return new Response(null, { status: 202 });
      const { _newSession, _status, ...body } = reply;
      const extra: Record<string, string> = typeof _newSession === "string" ? { "mcp-session-id": _newSession } : {};
      if (typeof _status === "number") return json(body, _status, extra);
      if (opts.sseReplies && (body["result"] !== undefined || body["error"] !== undefined)) {
        const push = pushStream();
        push.event("message", JSON.stringify(body));
        push.close();
        return new Response(push.stream, { status: 200, headers: { "content-type": "text/event-stream", ...extra } });
      }
      return json(body, 200, { ...extra, "x-upstream-internal": "do-not-leak" });
    },
  };
  return fake;
}

export interface ModernFakeOpts extends AuthOpts {
  tools?: ToolSpec[];
  /** tools/call answers `input_required` with an opaque requestState. */
  inputRequired?: boolean;
  serverInfo?: Record<string, unknown>;
}

export interface ModernFake {
  handler: FetchHandler;
  calls: CapturedCall[];
  discoverCount: number;
  toolCallCount: number;
  listenStreams: ReturnType<typeof pushStream>[];
  notifyListen: (data: string) => void;
}

/** A 2026-07-28 stateless server: header/body validation, no sessions, subscriptions/listen. */
export function modernFake(opts: ModernFakeOpts = {}): ModernFake {
  const tools = opts.tools ?? DEFAULT_TOOLS;
  const serverInfo = opts.serverInfo ?? { name: "upstream-modern", version: "3.0" };
  const fake: ModernFake = {
    calls: [],
    discoverCount: 0,
    toolCallCount: 0,
    listenStreams: [],
    notifyListen(data) {
      for (const s of fake.listenStreams) s.event("message", data);
    },
    handler: async (req) => {
      if (req.method !== "POST") return new Response(null, { status: 405 });
      if (!authorized(req, opts)) return new Response(null, { status: 401 });
      const parsed: unknown = await req.json().catch(() => null);
      if (parsed === null || Array.isArray(parsed)) {
        return json({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "invalid" } }, 400);
      }
      const msg = parsed as Record<string, unknown>;
      const method = msg["method"];
      const id = msg["id"];
      const params = msg["params"] as Record<string, unknown> | undefined;
      const headers = captureHeaders(req);
      if (typeof method !== "string") {
        return json({ jsonrpc: "2.0", id: null, error: { code: -32600, message: "responses are not accepted" } }, 400);
      }
      fake.calls.push({ method, httpMethod: "POST", headers, body: msg });
      if (id === undefined || id === null) return new Response(null, { status: 202 });
      const meta = (params?.["_meta"] ?? null) as Record<string, unknown> | null;
      const bodyVersion = meta?.["io.modelcontextprotocol/protocolVersion"];
      const headerVersion = req.headers.get("mcp-protocol-version");
      if (typeof bodyVersion !== "string" || meta?.["io.modelcontextprotocol/clientCapabilities"] === undefined) {
        return json({ jsonrpc: "2.0", id, error: { code: -32602, message: "missing required _meta" } }, 400);
      }
      if (headerVersion !== bodyVersion) {
        return json({ jsonrpc: "2.0", id, error: { code: -32020, message: "header/body mismatch" } }, 400);
      }
      if (bodyVersion !== "2026-07-28") {
        return json(
          { jsonrpc: "2.0", id, error: { code: -32022, message: "unsupported version", data: { supported: ["2026-07-28"], requested: bodyVersion } } },
          400,
        );
      }
      if (req.headers.get("mcp-method") !== method) {
        return json({ jsonrpc: "2.0", id, error: { code: -32020, message: "Mcp-Method mismatch" } }, 400);
      }
      if (method === "server/discover") {
        fake.discoverCount += 1;
        return json({
          jsonrpc: "2.0",
          id,
          result: {
            supportedVersions: ["2026-07-28"],
            capabilities: { tools: { listChanged: true } },
            instructions: "modern instructions",
            resultType: "complete",
            ttlMs: 60_000,
            cacheScope: "private",
            _meta: { "io.modelcontextprotocol/serverInfo": serverInfo },
          },
        });
      }
      if (method === "tools/list") {
        return json({
          jsonrpc: "2.0",
          id,
          result: { ...toolsResult(tools), resultType: "complete", ttlMs: 30_000, cacheScope: "public" },
        });
      }
      if (method === "tools/call") {
        if (req.headers.get("mcp-name") !== params?.["name"]) {
          return json({ jsonrpc: "2.0", id, error: { code: -32020, message: "Mcp-Name mismatch" } }, 400);
        }
        fake.toolCallCount += 1;
        if (opts.inputRequired) {
          return json({
            jsonrpc: "2.0",
            id,
            result: { resultType: "input_required", requestState: "opaque-xyz", inputRequests: {} },
          });
        }
        return json({ jsonrpc: "2.0", id, result: { ...callResult(params), resultType: "complete" } });
      }
      if (method === "resources/read") {
        return json({
          jsonrpc: "2.0",
          id,
          result: { contents: [{ uri: params?.["uri"], text: "resource body" }], resultType: "complete", ttlMs: 0, cacheScope: "private" },
        });
      }
      if (method === "subscriptions/listen") {
        const push = pushStream();
        fake.listenStreams.push(push);
        push.event(
          "message",
          JSON.stringify({
            jsonrpc: "2.0",
            method: "notifications/subscriptions/acknowledged",
            params: { notifications: params?.["notifications"] ?? {}, _meta: { "io.modelcontextprotocol/subscriptionId": id } },
          }),
        );
        return new Response(push.stream, { status: 200, headers: { "content-type": "text/event-stream" } });
      }
      return json({ jsonrpc: "2.0", id, error: { code: -32601, message: `Method not found: ${method}` } }, 404);
    },
  };
  return fake;
}

export interface Fake2411 {
  handler: FetchHandler;
  calls: CapturedCall[];
  initCount: number;
  toolCallCount: number;
  notify: (data: string) => void;
  /** Push a server-initiated request onto the stream (same-era passthrough tests). */
  pushRaw: (data: string) => void;
}

/** A 2024-11-05 HTTP+SSE server: GET opens the stream, `endpoint` names the POST URL. */
export function fake2411(base: string, opts: AuthOpts & { tools?: ToolSpec[] } = {}): Fake2411 {
  const tools = opts.tools ?? DEFAULT_TOOLS;
  const streams: ReturnType<typeof pushStream>[] = [];
  const fake: Fake2411 = {
    calls: [],
    initCount: 0,
    toolCallCount: 0,
    notify(data) {
      for (const s of streams) s.event("message", data);
    },
    pushRaw(data) {
      for (const s of streams) s.event("message", data);
    },
    handler: async (req) => {
      if (!authorized(req, opts)) return new Response(null, { status: 401 });
      const url = new URL(req.url);
      if (req.method === "GET" && url.pathname.endsWith("/messages") === false) {
        const push = pushStream();
        streams.push(push);
        push.event("endpoint", "/messages"); // Relative — exercises the resolver + origin pin.
        return new Response(push.stream, { status: 200, headers: { "content-type": "text/event-stream" } });
      }
      if (req.method === "POST" && url.pathname.endsWith("/messages")) {
        const msg = (await req.json().catch(() => null)) as Record<string, unknown> | null;
        if (msg === null) return new Response(null, { status: 400 });
        const method = msg["method"];
        const id = msg["id"];
        if (typeof method !== "string") return new Response(null, { status: 202 }); // A client response.
        fake.calls.push({ method, httpMethod: "POST", headers: captureHeaders(req), body: msg });
        if (id === undefined || id === null) return new Response(null, { status: 202 });
        const params = msg["params"] as Record<string, unknown> | undefined;
        const answer = (result: Record<string, unknown>) => {
          for (const s of streams) s.event("message", JSON.stringify({ jsonrpc: "2.0", id, result }));
        };
        if (method === "initialize") {
          fake.initCount += 1;
          answer({
            protocolVersion: "2024-11-05",
            capabilities: { tools: {} },
            serverInfo: { name: "upstream-2411", version: "0.1" },
          });
        } else if (method === "tools/list") {
          answer(toolsResult(tools));
        } else if (method === "tools/call") {
          fake.toolCallCount += 1;
          answer(callResult(params));
        } else {
          for (const s of streams) {
            s.event("message", JSON.stringify({ jsonrpc: "2.0", id, error: { code: -32601, message: "no such method" } }));
          }
        }
        return new Response(null, { status: 202 });
      }
      return new Response(null, { status: 405 }); // POST to the base URL: the transport-miss signal.
    },
  };
  return fake;
}

/** A refresh token endpoint; counts grants, rotates to `nextSecret`. */
export function tokenEndpoint(nextSecret: string, opts: { fail?: boolean } = {}) {
  const state = { grants: 0, bodies: [] as string[] };
  const handler: FetchHandler = async (req) => {
    const body = await req.text();
    state.grants += 1;
    state.bodies.push(body);
    if (opts.fail) return new Response("denied", { status: 400 });
    return json({ access_token: nextSecret, refresh_token: "refresh-2", token_type: "bearer" });
  };
  return { handler, state };
}

// -- client drivers (exercise the engine exactly as each era's client would) ------------------

import { handleGatewayRequest } from "../core/engine";

export async function rpc(t: TestCtx, body: unknown, headers: Record<string, string> = {}, opts: Parameters<typeof gwRequest>[0] = {}): Promise<Response> {
  return handleGatewayRequest(t.ctx, gwRequest({ ...opts, body, headers: { ...(opts.headers ?? {}), ...headers } }));
}

let clientId = 0;
export function nextId(): string {
  return `c:${(clientId += 1)}`;
}

/** Run a legacy Streamable HTTP client's initialize + initialized; returns the session id. */
export async function initLegacySession(t: TestCtx, version: string): Promise<string> {
  const resp = await rpc(t, {
    jsonrpc: "2.0",
    id: nextId(),
    method: "initialize",
    params: { protocolVersion: version, capabilities: { sampling: {} }, clientInfo: { name: "test-client", version: "1" } },
  });
  if (resp.status !== 200) throw new Error(`initialize failed: ${resp.status} ${await resp.text()}`);
  const sid = resp.headers.get("mcp-session-id");
  if (sid === null) throw new Error("no session header minted");
  const note = await rpc(t, { jsonrpc: "2.0", method: "notifications/initialized" }, { "mcp-session-id": sid });
  if (note.status !== 202) throw new Error(`initialized notification failed: ${note.status}`);
  return sid;
}

export function modernHeaders(method: string, name?: string): Record<string, string> {
  return {
    "mcp-protocol-version": "2026-07-28",
    "mcp-method": method,
    ...(name !== undefined ? { "mcp-name": name } : {}),
  };
}

export function modernBody(method: string, params: Record<string, unknown> = {}, id: string | number = nextId()): Record<string, unknown> {
  return {
    jsonrpc: "2.0",
    id,
    method,
    params: {
      ...params,
      _meta: {
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientCapabilities": { elicitation: {} },
        ...(params["_meta"] as Record<string, unknown> | undefined),
      },
    },
  };
}

export interface Client2411 {
  endpoint: string;
  post: (body: unknown) => Promise<Response>;
  next: () => Promise<ParsedEvent | null>;
  nextMessage: () => Promise<Record<string, unknown>>;
  cancel: () => Promise<void>;
}

/** Open the 2024-11-05 HTTP+SSE client transport against the engine. */
export async function open2411Client(t: TestCtx): Promise<Client2411> {
  const resp = await handleGatewayRequest(t.ctx, gwRequest({ method: "GET" }));
  if (resp.status !== 200 || !resp.body) throw new Error(`GET stream failed: ${resp.status}`);
  const reader = eventReader(resp.body);
  const first = await reader.next();
  if (!first || first.event !== "endpoint") throw new Error("no endpoint event");
  const endpoint = first.data;
  return {
    endpoint,
    post: (body) => handleGatewayRequest(t.ctx, gwRequest({ url: endpoint, body })),
    next: () => reader.next(),
    nextMessage: async () => {
      const ev = await reader.next();
      if (!ev) throw new Error("stream ended");
      return JSON.parse(ev.data) as Record<string, unknown>;
    },
    cancel: () => reader.cancel(),
  };
}

export async function bodyJson(resp: Response): Promise<Record<string, unknown>> {
  return (await resp.json()) as Record<string, unknown>;
}
