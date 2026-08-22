/**
 * The per-revision protocol behavior table and the JSON-RPC primitives the engine shares.
 *
 * Five MCP revisions exist; the engine never branches on an "era" label directly — every branch
 * keys off a derived fact in BEHAVIOR so a future revision is one table row, not a code sweep.
 * The facts here mirror the published transport/lifecycle rules per revision:
 *
 * - 2024-11-05  HTTP+SSE two-endpoint transport (GET stream + `endpoint` event); initialize
 *               handshake; no session header; no version header; server may send requests.
 * - 2025-03-26  Streamable HTTP; server-minted `Mcp-Session-Id`; JSON-RPC batching legal;
 *               no version header requirement.
 * - 2025-06-18  Same, batching removed; `MCP-Protocol-Version` header required after initialize
 *               (absent ⇒ assume 2025-03-26).
 * - 2025-11-25  Same as 2025-06-18 (additive deltas only: polling, icons, tasks).
 * - 2026-07-28  Stateless: no initialize, no sessions, POST-only; `_meta` carries version +
 *               capabilities on every request; `Mcp-Method`/`Mcp-Name` headers validated against
 *               the body; server must not send JSON-RPC requests; change notifications only on
 *               an opted-in `subscriptions/listen` stream.
 */

export const REVISIONS = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"] as const;
export type Revision = (typeof REVISIONS)[number];

export const MODERN: Revision = "2026-07-28";
/** The version the engine offers a legacy peer when the peer's ask is unusable. */
export const LATEST_LEGACY: Revision = "2025-11-25";

export function isRevision(value: unknown): value is Revision {
  return typeof value === "string" && (REVISIONS as readonly string[]).includes(value);
}

export interface RevisionBehavior {
  /** Two-endpoint HTTP+SSE (2024-11-05) vs single-endpoint Streamable HTTP. */
  transport: "http+sse" | "streamable-http";
  /** Server mints `Mcp-Session-Id` on InitializeResult; client echoes it on every request. */
  sessionHeader: boolean;
  /** JSON-RPC batch arrays legal in POST bodies (2025-03-26 only). */
  batching: boolean;
  /** `MCP-Protocol-Version` HTTP header expected on post-initialize requests. */
  versionHeader: boolean;
  /** `initialize` handshake vs per-request `_meta` (stateless). */
  handshake: "initialize" | "stateless";
  /** Server may send JSON-RPC requests to the client (sampling/elicitation/roots). */
  serverRequests: boolean;
  /** `ping` exists (removed in 2026-07-28 — the gateway answers it, never forwards it). */
  ping: boolean;
  /** Where unsolicited change notifications ride toward this peer. */
  notifications: "sse-stream" | "get-stream" | "subscriptions-listen";
  /** `Last-Event-ID` resumability defined. */
  resumable: boolean;
  /** Results must carry `resultType` (+ `ttlMs`/`cacheScope` on list/read/discover). */
  resultMeta: boolean;
  /** `Mcp-Method`/`Mcp-Name` headers required and validated against the body. */
  methodHeaders: boolean;
}

export const BEHAVIOR: Record<Revision, RevisionBehavior> = {
  "2024-11-05": {
    transport: "http+sse",
    sessionHeader: false,
    batching: false,
    versionHeader: false,
    handshake: "initialize",
    serverRequests: true,
    ping: true,
    notifications: "sse-stream",
    resumable: false,
    resultMeta: false,
    methodHeaders: false,
  },
  "2025-03-26": {
    transport: "streamable-http",
    sessionHeader: true,
    batching: true,
    versionHeader: false,
    handshake: "initialize",
    serverRequests: true,
    ping: true,
    notifications: "get-stream",
    resumable: true,
    resultMeta: false,
    methodHeaders: false,
  },
  "2025-06-18": {
    transport: "streamable-http",
    sessionHeader: true,
    batching: false,
    versionHeader: true,
    handshake: "initialize",
    serverRequests: true,
    ping: true,
    notifications: "get-stream",
    resumable: true,
    resultMeta: false,
    methodHeaders: false,
  },
  "2025-11-25": {
    transport: "streamable-http",
    sessionHeader: true,
    batching: false,
    versionHeader: true,
    handshake: "initialize",
    serverRequests: true,
    ping: true,
    notifications: "get-stream",
    resumable: true,
    resultMeta: false,
    methodHeaders: false,
  },
  "2026-07-28": {
    transport: "streamable-http",
    sessionHeader: false,
    batching: false,
    versionHeader: true,
    handshake: "stateless",
    serverRequests: false,
    ping: false,
    notifications: "subscriptions-listen",
    resumable: false,
    resultMeta: true,
    methodHeaders: true,
  },
};

/** Both sides share a handshake model — the gate for pass-through of non-tool features. */
export function sameEra(a: Revision, b: Revision): boolean {
  return BEHAVIOR[a].handshake === BEHAVIOR[b].handshake;
}

// ---------------------------------------------------------------------------------------------
// JSON-RPC message shapes. Parsed values stay `unknown`-first; every narrowing is explicit so a
// malformed body fails closed instead of flowing on as `any`.

export type JsonRpcId = string | number;

export interface JsonRpcRequest {
  jsonrpc: "2.0";
  id: JsonRpcId;
  method: string;
  params?: Record<string, unknown>;
}

export interface JsonRpcNotification {
  jsonrpc: "2.0";
  /** Structurally bars a JsonRpcRequest, so type-guard narrowing keeps the union apart. */
  id?: undefined;
  method: string;
  params?: Record<string, unknown>;
}

export interface JsonRpcSuccess {
  jsonrpc: "2.0";
  id: JsonRpcId;
  result: Record<string, unknown>;
}

export interface JsonRpcFailure {
  jsonrpc: "2.0";
  id: JsonRpcId | null;
  error: { code: number; message: string; data?: unknown };
}

export type JsonRpcMessage = JsonRpcRequest | JsonRpcNotification | JsonRpcSuccess | JsonRpcFailure;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function isId(value: unknown): value is JsonRpcId {
  return typeof value === "string" || typeof value === "number";
}

/** Narrow a parsed JSON value to one JSON-RPC message; null = malformed (fail closed). */
export function asJsonRpcMessage(value: unknown): JsonRpcMessage | null {
  if (!isRecord(value) || value["jsonrpc"] !== "2.0") return null;
  const method = value["method"];
  const id = value["id"];
  if (typeof method === "string") {
    const params = value["params"];
    if (params !== undefined && !isRecord(params)) return null;
    if (id === undefined || id === null) {
      return { jsonrpc: "2.0", method, ...(params !== undefined ? { params } : {}) };
    }
    if (!isId(id)) return null;
    return { jsonrpc: "2.0", id, method, ...(params !== undefined ? { params } : {}) };
  }
  if (isRecord(value["error"])) {
    const err = value["error"];
    if (typeof err["code"] !== "number" || typeof err["message"] !== "string") return null;
    if (id !== null && !isId(id)) return null;
    return {
      jsonrpc: "2.0",
      id: id === null ? null : (id as JsonRpcId),
      error: { code: err["code"], message: err["message"], ...(err["data"] !== undefined ? { data: err["data"] } : {}) },
    };
  }
  if (isRecord(value["result"]) && isId(id)) {
    return { jsonrpc: "2.0", id, result: value["result"] };
  }
  return null;
}

export function isRequest(msg: JsonRpcMessage): msg is JsonRpcRequest {
  return "method" in msg && "id" in msg;
}

export function isNotification(msg: JsonRpcMessage): msg is JsonRpcNotification {
  return "method" in msg && !("id" in msg);
}

export function isResponse(msg: JsonRpcMessage): msg is JsonRpcSuccess | JsonRpcFailure {
  return !("method" in msg);
}

// JSON-RPC error codes. −32020..−32099 is reserved by the 2026-07-28 revision; the gateway's own
// errors use −32000 (the implementation-defined band that revision leaves open).
export const ERR_PARSE = -32700;
export const ERR_INVALID_REQUEST = -32600;
export const ERR_METHOD_NOT_FOUND = -32601;
export const ERR_INVALID_PARAMS = -32602;
export const ERR_HEADER_MISMATCH = -32020;
export const ERR_MISSING_CAPABILITY = -32021;
export const ERR_UNSUPPORTED_VERSION = -32022;
export const ERR_GATEWAY = -32000;

/** Agent-facing copy. The two workspace strings are pinned by the product — ship verbatim. */
export const copy = {
  toolNotEnabled: (name: string) => `The tool ${name} is not enabled for this workspace.`,
  noCredential: (displayName: string, workspace: string, webUrl: string) =>
    `No sign-in is connected for ${displayName} in ${workspace}. Ask a member to connect one at ${webUrl}.`,
  crossEra: (method: string) => `The method ${method} is not supported across protocol revisions.`,
  crossEraInputRequired: () => `The server asked for additional input, which is not supported across protocol revisions.`,
  unauthorized: () => `Unauthorized.`,
  unknownServer: () => `Unknown server for this workspace.`,
  upstreamFailed: () => `The upstream server did not answer the request.`,
  upstreamAuthFailed: () => `The upstream server rejected the gateway's sign-in.`,
};

export function rpcError(id: JsonRpcId | null, code: number, message: string, data?: unknown): JsonRpcFailure {
  return { jsonrpc: "2.0", id, error: { code, message, ...(data !== undefined ? { data } : {}) } };
}

export function rpcResult(id: JsonRpcId, result: Record<string, unknown>): JsonRpcSuccess {
  return { jsonrpc: "2.0", id, result };
}

export function jsonResponse(body: JsonRpcMessage | JsonRpcMessage[], status = 200, headers?: Record<string, string>): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: { "content-type": "application/json", ...headers },
  });
}

// ---------------------------------------------------------------------------------------------
// 2026-07-28 per-request `_meta` keys.

export const META_VERSION = "io.modelcontextprotocol/protocolVersion";
export const META_CAPABILITIES = "io.modelcontextprotocol/clientCapabilities";
export const META_CLIENT_INFO = "io.modelcontextprotocol/clientInfo";
export const META_SERVER_INFO = "io.modelcontextprotocol/serverInfo";
export const META_SUBSCRIPTION = "io.modelcontextprotocol/subscriptionId";
export const META_LOG_LEVEL = "io.modelcontextprotocol/logLevel";

export interface ModernMeta {
  version: string;
  /** Present-and-shaped check; the value itself is relayed opaquely. */
  capabilities: Record<string, unknown> | null;
  clientInfo: unknown;
  logLevel: boolean;
}

/** Null when the request carries no modern version claim (i.e. it is not a modern request). */
export function readModernMeta(params: Record<string, unknown> | undefined): ModernMeta | null {
  if (!params) return null;
  const meta = params["_meta"];
  if (!isRecord(meta)) return null;
  const version = meta[META_VERSION];
  if (typeof version !== "string") return null;
  const caps = meta[META_CAPABILITIES];
  return {
    version,
    capabilities: isRecord(caps) ? caps : null,
    clientInfo: meta[META_CLIENT_INFO],
    logLevel: meta[META_LOG_LEVEL] !== undefined,
  };
}

/**
 * Decode a 2026-07-28 header value that may use the non-ASCII sentinel `=?base64?<b64>?=`.
 * Values outside the sentinel are used as-is (header values are case-sensitive).
 */
export function decodeSentinelHeader(value: string): string | null {
  const m = /^=\?base64\?([A-Za-z0-9+/=]*)\?=$/.exec(value);
  if (!m) return value;
  try {
    const b64 = m[1] ?? "";
    const bytes = atob(b64);
    const buf = new Uint8Array(bytes.length);
    for (let i = 0; i < bytes.length; i++) buf[i] = bytes.charCodeAt(i);
    return new TextDecoder("utf-8", { fatal: true }).decode(buf);
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------------------------
// Small crypto helpers. WebCrypto + getRandomValues exist in Bun, Node ≥20, and edge isolates —
// the only platform surface the portable core touches beyond fetch/streams.

export async function sha256Hex(input: string): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(input));
  return Array.from(new Uint8Array(digest), (b) => b.toString(16).padStart(2, "0")).join("");
}

/** Cryptographically random, visible-ASCII (hex) — the legacy session-id requirement. */
export function mintId(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
}

let gatewayRequestCounter = 0;
/** Ids for the gateway's own upstream requests (initialize, discover, listen). */
export function nextGatewayId(): string {
  gatewayRequestCounter += 1;
  return `gw:${gatewayRequestCounter}`;
}
