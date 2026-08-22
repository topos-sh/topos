/**
 * The portable core's ports — the ONE contract between the session engine and whatever hosts it.
 *
 * The engine (engine.ts and friends) is pure: no filesystem, no sockets, no process, no timers of
 * its own, ZERO runtime dependencies. Everything it needs arrives through these interfaces, so the
 * identical engine runs inside the container service (adapters over Postgres and direct fetch) and
 * inside an edge isolate (adapters over seeded state and a platform fetch). Web-standard
 * Request/Response are the only I/O types — both targets speak them natively.
 *
 * Secrets: the store returns credentials ALREADY DECRYPTED, held in memory for the duration of one
 * request. The engine never sees key material, never persists a secret, and never logs one.
 *
 * SSRF: the injected `fetch` is the GUARDED one — the container adapter enforces the private-range
 * policy there (resolve-then-connect), so the engine calls it without ceremony. An edge deployment
 * substitutes the platform fetch.
 */

export type AuthMode = "none" | "oauth" | "manual" | null;

/** The caller a bearer token proved: one enrolled installation in one workspace, as one person. */
export interface SessionRef {
  sessionId: string;
  workspaceId: string;
  userId: string;
  /** The installation's display label — rides into usage rows, never into upstream requests. */
  displayName: string;
}

/** One workspace's connected server, resolved to the revision it currently receives. */
export interface ResolvedServer {
  serverId: string;
  displayName: string;
  /** The upstream MCP endpoint (https, extracted from the resolved revision's document). */
  url: string;
  transport: string;
  authMode: AuthMode;
  /**
   * Header NAME a `manual` secret rides in when the server's document declares one
   * (e.g. `X-API-Key`); null = standard `Authorization: Bearer`.
   */
  secretHeader: string | null;
  /** The web page where a person connects a sign-in — quoted in the no-credential answer. */
  authorizeUrl: string;
  workspaceDisplayName: string;
}

export interface ToolPolicy {
  mode: "all" | "selected";
  /** Meaningful only under `selected`: the enabled tool names. */
  selected: ReadonlySet<string>;
}

/** A decrypted credential, alive for one request. */
export interface Credential {
  id: string;
  kind: "oauth" | "manual";
  /** The bearer/API secret to attach upstream. */
  secret: string;
  /** OAuth refresh material, absent for `manual`. */
  refreshToken?: string;
  tokenEndpoint?: string;
  clientId?: string;
}

export interface ObservedTool {
  name: string;
  description?: string;
}

export type UsageOutcome = "ok" | "denied_tool" | "no_credential" | "unauthorized" | "upstream_error";

export interface UsageEvent {
  workspaceId: string;
  serverId: string;
  sessionId: string;
  userId: string;
  /** Null for non-tool methods (initialize, lists, notifications). */
  toolName: string | null;
  method: string;
  outcome: UsageOutcome;
  durationMs: number;
}

/**
 * Everything the engine may ask its host. The container answers from Postgres per call (which is
 * what makes revocation a row change); an edge deployment answers from seeded state.
 */
export interface GatewayStore {
  /** Hex sha256 of the presented bearer → the active session it names, or null. */
  sessionByTokenSha256(hex: string): Promise<SessionRef | null>;
  /** Null when the workspace has no live connection to this server. */
  connectedServer(workspaceId: string, serverId: string): Promise<ResolvedServer | null>;
  /** No stored policy row means `{ mode: "all" }`. */
  toolPolicy(workspaceId: string, serverId: string): Promise<ToolPolicy>;
  /** Resolution is the store's: the person's own credential first, the workspace one second. */
  credentialFor(workspaceId: string, serverId: string, userId: string): Promise<Credential | null>;
  /** Persist a rotated token (re-encrypted by the adapter); id is the credential's. */
  saveRotatedCredential(id: string, next: { secret: string; refreshToken?: string }): Promise<void>;
  /**
   * Run `fn` holding this credential's refresh serialization (row lock in the container; the
   * host's single-threaded execution at the edge makes this a plain call there).
   */
  withRefreshLock<T>(credentialId: string, fn: () => Promise<T>): Promise<T>;
  /** Upsert the tool list a live tools/list response advertised. Fire-and-forget semantics. */
  recordObservedTools(workspaceId: string, serverId: string, tools: readonly ObservedTool[]): Promise<void>;
}

export interface UsageSink {
  /** Must never throw and never block the response path. */
  record(event: UsageEvent): void;
}

export interface CoreEnv {
  /** The GUARDED fetch (SSRF policy already applied by the host). */
  fetch: typeof fetch;
  /** Milliseconds since epoch — injected so the edge host controls time. */
  now(): number;
  /** Structured, secret-free logging. `fields` values must never carry tokens or bodies. */
  log(level: "info" | "warn" | "error", message: string, fields?: Record<string, string | number>): void;
}

export interface GatewayContext {
  store: GatewayStore;
  usage: UsageSink;
  env: CoreEnv;
}

/**
 * The one entry point a deployment calls per proxied HTTP request. `bearer` is the RAW token from
 * the Authorization header (the engine hashes it; hosts never pre-verify). Implemented in
 * engine.ts.
 */
export interface GatewayRequest {
  sessionId: string;
  serverId: string;
  request: Request;
  bearer: string | null;
}

/**
 * HOST CONTRACT for `request`: it may arrive with its body unread, and the engine may answer
 * without reading it (every pre-auth refusal does). The engine cancels the body on those paths
 * rather than leaving a stream dangling — some hosts route a request across an internal hop that
 * objects to an unread stream once a response is sent. A host that buffers the body before
 * calling is equally fine; neither side needs to know which the other did.
 */
