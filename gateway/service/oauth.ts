import { createHash, randomBytes } from "node:crypto";
import type { ResolvedServer } from "../core/ports";
import type { CredentialPayload } from "./crypto";
import type { Logger } from "./log";
import type { OauthFlowRow, StoreCredentialInput } from "./store";

/**
 * The authorize walk — RFC 9728 protected-resource metadata first, the OIDC/RFC 8414 discovery
 * chain for the authorization server, DCR where the server offers it, PKCE S256 always, and the
 * RFC 8707 `resource` parameter on both the authorize and token requests (unknown params are
 * ignored by earlier-era servers, so it rides unconditionally).
 *
 * Every outbound request here goes through the injected GUARDED fetch: a discovery document is
 * attacker-influenced input, and an endpoint it names gets no more trust than the address the
 * catalog held.
 */

const FLOW_TTL_MS = 10 * 60 * 1000;
const DISCOVERY_BODY_LIMIT = 256 * 1024;

export type BeginRefusal = "MANUAL_ONLY" | "NO_AUTH_NEEDED" | "DISCOVERY_FAILED";

/** What the ceremony needs of the store — PgStore satisfies it; the DO adapter will too. */
export interface OauthStore {
  connectedServer(workspaceId: string, serverId: string): Promise<ResolvedServer | null>;
  mintOauthFlow(row: Omit<OauthFlowRow, "id">): Promise<string>;
  takeOauthFlow(state: string): Promise<OauthFlowRow | null>;
  storeCredential(input: StoreCredentialInput): Promise<string>;
}

export interface AuthorizationPlan {
  authorizationEndpoint: string;
  tokenEndpoint: string;
  clientId: string;
  scopes: string | null;
}

interface WalkDeps {
  fetch: typeof fetch;
  log: Logger;
}

function base64url(bytes: Buffer): string {
  return bytes.toString("base64").replaceAll("+", "-").replaceAll("/", "_").replace(/=+$/, "");
}

async function readJson(response: Response): Promise<Record<string, unknown> | null> {
  try {
    const text = await response.text();
    if (text.length > DISCOVERY_BODY_LIMIT) {
      return null;
    }
    const parsed: unknown = JSON.parse(text);
    if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
      return null;
    }
    return parsed as Record<string, unknown>;
  } catch {
    return null;
  }
}

async function fetchJson(
  deps: WalkDeps,
  url: string,
  init?: RequestInit,
): Promise<Record<string, unknown> | null> {
  try {
    const response = await deps.fetch(url, init);
    if (!response.ok) {
      return null;
    }
    return await readJson(response);
  } catch {
    return null;
  }
}

/**
 * An absolute http(s) URL or nothing. Scheme ENFORCEMENT is the guarded transport's — an http
 * endpoint discovered here still refuses at dial time under the production policy (and a test
 * rig's loopback fixture may pass) — this only rules out relative paths and foreign schemes.
 */
function absoluteHttpUrl(value: unknown): string | null {
  if (typeof value !== "string") {
    return null;
  }
  try {
    const url = new URL(value);
    return url.protocol === "https:" || url.protocol === "http:" ? value : null;
  } catch {
    return null;
  }
}

/** Well-known path insertion (RFC 8414 §3 / RFC 9728): the suffix goes BETWEEN host and path. */
function wellKnown(base: URL, suffix: string): string {
  const path = base.pathname.replace(/\/$/, "");
  return `${base.origin}/.well-known/${suffix}${path === "" ? "" : path}`;
}

/** The 401 challenge's `resource_metadata="…"` parameter, when the server sends one. */
function resourceMetadataFromChallenge(header: string | null): string | null {
  if (header === null) {
    return null;
  }
  const match = header.match(/resource_metadata="([^"]+)"/);
  return match?.[1] ?? null;
}

/**
 * Find the authorization server for an MCP endpoint: the PRM well-known probes (path-scoped then
 * root), then a 401 challenge's `resource_metadata`, then — the 2025-03-26 shape — the MCP
 * server's own origin as the AS.
 */
async function discoverAuthorizationServer(
  deps: WalkDeps,
  serverUrl: URL,
): Promise<{ issuer: string; scopes: string | null; legacyOrigin: boolean }> {
  const prmUrls =
    serverUrl.pathname === "/" || serverUrl.pathname === ""
      ? [wellKnown(serverUrl, "oauth-protected-resource")]
      : [
          wellKnown(serverUrl, "oauth-protected-resource"),
          `${serverUrl.origin}/.well-known/oauth-protected-resource`,
        ];
  for (const url of prmUrls) {
    const prm = await fetchJson(deps, url);
    const issuer = extractIssuerFromPrm(prm);
    if (issuer !== null) {
      return { ...issuer, legacyOrigin: false };
    }
  }
  // No PRM well-known: ask the endpoint itself and read the challenge (2025-06-18's one MUST).
  try {
    const probe = await deps.fetch(serverUrl.toString(), { method: "GET" });
    // The response body is upstream data nobody here reads; cancel it so the connection frees.
    await probe.body?.cancel();
    if (probe.status === 401) {
      const metadataUrl = resourceMetadataFromChallenge(probe.headers.get("www-authenticate"));
      if (metadataUrl !== null) {
        const issuer = extractIssuerFromPrm(await fetchJson(deps, metadataUrl));
        if (issuer !== null) {
          return { ...issuer, legacyOrigin: false };
        }
      }
    }
  } catch {
    // The endpoint not answering a bare GET proves nothing — the legacy fallback still applies.
  }
  return { issuer: serverUrl.origin, scopes: null, legacyOrigin: true };
}

function extractIssuerFromPrm(
  prm: Record<string, unknown> | null,
): { issuer: string; scopes: string | null } | null {
  if (prm === null || !Array.isArray(prm.authorization_servers)) {
    return null;
  }
  const issuer = absoluteHttpUrl(prm.authorization_servers[0]);
  if (issuer === null) {
    return null;
  }
  const scopes = Array.isArray(prm.scopes_supported)
    ? prm.scopes_supported.filter((s): s is string => typeof s === "string").join(" ")
    : null;
  return { issuer, scopes: scopes === "" ? null : scopes };
}

/** RFC 8414 then OIDC discovery, both well-known families, in the spec's priority order. */
async function fetchAsMetadata(
  deps: WalkDeps,
  issuer: string,
): Promise<Record<string, unknown> | null> {
  const base = new URL(issuer);
  const hasPath = base.pathname !== "/" && base.pathname !== "";
  const candidates = [
    wellKnown(base, "oauth-authorization-server"),
    wellKnown(base, "openid-configuration"),
    ...(hasPath
      ? [`${base.origin}${base.pathname.replace(/\/$/, "")}/.well-known/openid-configuration`]
      : []),
  ];
  for (const url of candidates) {
    const metadata = await fetchJson(deps, url);
    if (metadata !== null && absoluteHttpUrl(metadata.authorization_endpoint) !== null) {
      // 2026-07-28 iss validation: metadata that names an issuer must name the one asked for.
      if (typeof metadata.issuer === "string" && metadata.issuer.replace(/\/$/, "") !== issuer.replace(/\/$/, "")) {
        continue;
      }
      return metadata;
    }
  }
  return null;
}

/** Dynamic client registration (RFC 7591) — a public client, redirect-URI-exact. */
async function registerClient(
  deps: WalkDeps,
  registrationEndpoint: string,
  redirectUri: string,
): Promise<string | null> {
  const response = await fetchJson(deps, registrationEndpoint, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      client_name: "Topos gateway",
      redirect_uris: [redirectUri],
      grant_types: ["authorization_code", "refresh_token"],
      response_types: ["code"],
      token_endpoint_auth_method: "none",
      application_type: "web",
    }),
  });
  const clientId = response?.client_id;
  return typeof clientId === "string" && clientId.length > 0 ? clientId : null;
}

/** The full walk: PRM → AS metadata → client. Null = DISCOVERY_FAILED (the reason is logged). */
export async function planAuthorization(
  deps: WalkDeps,
  serverUrl: string,
  redirectUri: string,
): Promise<AuthorizationPlan | null> {
  let parsed: URL;
  try {
    parsed = new URL(serverUrl);
  } catch {
    return null;
  }
  const { issuer, scopes, legacyOrigin } = await discoverAuthorizationServer(deps, parsed);
  const metadata = await fetchAsMetadata(deps, issuer);

  let authorizationEndpoint: string | null;
  let tokenEndpoint: string | null;
  let registrationEndpoint: string | null;
  if (metadata !== null) {
    authorizationEndpoint = absoluteHttpUrl(metadata.authorization_endpoint);
    tokenEndpoint = absoluteHttpUrl(metadata.token_endpoint);
    registrationEndpoint = absoluteHttpUrl(metadata.registration_endpoint);
    // PKCE: where the AS declares its methods, S256 must be among them; silence passes (the
    // 2025-03-26 era predates the field, and the challenge itself still rides).
    if (
      Array.isArray(metadata.code_challenge_methods_supported) &&
      !metadata.code_challenge_methods_supported.includes("S256")
    ) {
      deps.log("warn", "authorization server refuses S256; refusing the walk");
      return null;
    }
  } else if (legacyOrigin) {
    // 2025-03-26 default endpoints at the server's origin.
    authorizationEndpoint = `${issuer}/authorize`;
    tokenEndpoint = `${issuer}/token`;
    registrationEndpoint = `${issuer}/register`;
  } else {
    deps.log("warn", "authorization server metadata unreachable");
    return null;
  }
  if (authorizationEndpoint === null || tokenEndpoint === null) {
    deps.log("warn", "authorization server metadata names no usable endpoints");
    return null;
  }
  if (registrationEndpoint === null) {
    // No DCR and no pre-registration channel this increment (CIMD is a later addition).
    deps.log("warn", "authorization server offers no client registration");
    return null;
  }
  const clientId = await registerClient(deps, registrationEndpoint, redirectUri);
  if (clientId === null) {
    deps.log("warn", "dynamic client registration refused");
    return null;
  }
  return { authorizationEndpoint, tokenEndpoint, clientId, scopes };
}

export interface BeginAuthorizeInput {
  workspaceId: string;
  serverId: string;
  userId: string | null;
  returnTo: string;
}

export interface OauthConfig {
  gatewayPublicUrl: string;
  toposPublicUrl: string;
}

export function redirectUriFor(config: OauthConfig): string {
  return `${config.gatewayPublicUrl.replace(/\/$/, "")}/oauth/callback`;
}

/** ReturnTo stays on the web app's own origin — checked at mint AND again at redemption. */
export function returnToAllowed(returnTo: string, config: OauthConfig): boolean {
  try {
    return new URL(returnTo).origin === new URL(config.toposPublicUrl).origin;
  } catch {
    return false;
  }
}

export type BeginOutcome =
  | { ok: true; authorizeUrl: string }
  | { ok: false; code: BeginRefusal }
  | { ok: false; code: "NOT_CONNECTED" | "BAD_RETURN_TO" };

export async function beginAuthorize(
  input: BeginAuthorizeInput,
  store: OauthStore,
  config: OauthConfig,
  deps: WalkDeps,
): Promise<BeginOutcome> {
  const server = await store.connectedServer(input.workspaceId, input.serverId);
  if (server === null) {
    return { ok: false, code: "NOT_CONNECTED" };
  }
  if (server.authMode === "manual") {
    return { ok: false, code: "MANUAL_ONLY" };
  }
  if (server.authMode === "none") {
    return { ok: false, code: "NO_AUTH_NEEDED" };
  }
  // A caller-built returnTo off the web's own origin is a caller bug, refused before any walk.
  if (!returnToAllowed(input.returnTo, config)) {
    return { ok: false, code: "BAD_RETURN_TO" };
  }
  const redirectUri = redirectUriFor(config);
  const plan = await planAuthorization(deps, server.url, redirectUri);
  if (plan === null) {
    return { ok: false, code: "DISCOVERY_FAILED" };
  }
  const state = base64url(randomBytes(32));
  const codeVerifier = base64url(randomBytes(48));
  const challenge = base64url(createHash("sha256").update(codeVerifier).digest());
  // RFC 8707: the audience is the MCP server's canonical URI, repeated at the token request.
  const resource = server.url;
  await store.mintOauthFlow({
    state,
    workspaceId: input.workspaceId,
    serverId: input.serverId,
    userId: input.userId,
    codeVerifier,
    clientId: plan.clientId,
    tokenEndpoint: plan.tokenEndpoint,
    resource,
    returnTo: input.returnTo,
    expiresAt: new Date(Date.now() + FLOW_TTL_MS),
  });
  const authorize = new URL(plan.authorizationEndpoint);
  authorize.searchParams.set("response_type", "code");
  authorize.searchParams.set("client_id", plan.clientId);
  authorize.searchParams.set("redirect_uri", redirectUri);
  authorize.searchParams.set("state", state);
  authorize.searchParams.set("code_challenge", challenge);
  authorize.searchParams.set("code_challenge_method", "S256");
  authorize.searchParams.set("resource", resource);
  if (plan.scopes !== null) {
    authorize.searchParams.set("scope", plan.scopes);
  }
  return { ok: true, authorizeUrl: authorize.toString() };
}

export type CallbackOutcome =
  | {
      ok: true;
      returnTo: string;
      /**
       * The connection a credential just landed on, or null when the walk ended without one. The
       * caller uses it to probe the server's tools before the person is sent back to the page that
       * renders them; this module does no probing of its own — its business is the ceremony.
       */
      stored: { workspaceId: string; serverId: string; userId: string | null } | null;
    }
  | { ok: false; status: 400 };

/**
 * Redeem the callback: consume the state row (single-use, before any network), exchange the code
 * with PKCE + resource, validate, encrypt, store. The person lands back on the flow's returnTo
 * whatever the sign-in outcome — the server page shows the standing truth either way.
 */
export async function completeCallback(
  params: URLSearchParams,
  store: OauthStore,
  config: OauthConfig,
  deps: WalkDeps,
): Promise<CallbackOutcome> {
  const state = params.get("state");
  if (state === null || state === "") {
    return { ok: false, status: 400 };
  }
  const flow = await store.takeOauthFlow(state);
  if (flow === null || flow.expiresAt.getTime() < Date.now()) {
    // Unknown, replayed, or expired — one constant answer, no oracle.
    return { ok: false, status: 400 };
  }
  if (!returnToAllowed(flow.returnTo, config)) {
    return { ok: false, status: 400 };
  }
  const code = params.get("code");
  if (code === null || code === "" || params.get("error") !== null) {
    // The AS refused (or answered without a code). The ceremony is over; the page tells the truth.
    return { ok: true, returnTo: flow.returnTo, stored: null };
  }
  const body = new URLSearchParams({
    grant_type: "authorization_code",
    code,
    redirect_uri: redirectUriFor(config),
    client_id: flow.clientId,
    code_verifier: flow.codeVerifier,
    resource: flow.resource,
  });
  let token: Record<string, unknown> | null = null;
  try {
    const response = await deps.fetch(flow.tokenEndpoint, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded", accept: "application/json" },
      body: body.toString(),
    });
    if (response.ok) {
      token = await readJson(response);
    }
  } catch {
    token = null;
  }
  const accessToken = token?.access_token;
  const tokenType = token?.token_type;
  if (
    token === null ||
    typeof accessToken !== "string" ||
    accessToken === "" ||
    (typeof tokenType === "string" && tokenType.toLowerCase() !== "bearer")
  ) {
    deps.log("warn", "token exchange failed", { serverId: flow.serverId });
    return { ok: true, returnTo: flow.returnTo, stored: null };
  }
  const payload: CredentialPayload = {
    secret: accessToken,
    ...(typeof token.refresh_token === "string" && token.refresh_token !== ""
      ? { refreshToken: token.refresh_token }
      : {}),
    tokenEndpoint: flow.tokenEndpoint,
    clientId: flow.clientId,
  };
  await store.storeCredential({
    workspaceId: flow.workspaceId,
    serverId: flow.serverId,
    userId: flow.userId,
    authKind: "oauth",
    payload,
    createdByDisplay: flow.userId ?? "workspace",
  });
  return {
    ok: true,
    returnTo: flow.returnTo,
    stored: { workspaceId: flow.workspaceId, serverId: flow.serverId, userId: flow.userId },
  };
}
