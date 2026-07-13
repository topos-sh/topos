import { serverEnv } from "@/env.server";
import { internalError, uniformNotFound } from "@/lib/api/wire.server";

/**
 * The ONE vault transport. Everything this tier asks the vault goes through `vaultFetch`:
 * - a runtime allowlist refuses any (method, path template) outside ALLOWED_ROUTES BEFORE a
 *   request is made — even a future typed addition fails closed;
 * - the internal lane gets exactly two headers, injected here: the shared internal bearer and
 *   the session-verified acting principal. Public reads (the verification context, the claim
 *   passthrough) ride bare, exactly like an unmodified client;
 * - there is NO other credential anywhere: this tier holds no device credential and no key —
 *   the vault re-verifies the acting principal's roster rows in-transaction on every call.
 *
 * Every vault read is `no-store` fresh: nothing here caches across requests except the
 * version-content LRU (immutable, content-addressed — see version-cache.server.ts).
 */
export const ALLOWED_ROUTES = [
  "GET /v1/enroll/verify/{user_code}",
  "GET /i/{token}",
  "GET /internal/v1/workspaces/{ws}/skills/{skill}/current",
  "GET /internal/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}",
  "GET /internal/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}",
  "GET /internal/v1/workspaces/{ws}/skills/{skill}/proposals",
  "GET /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}",
  "POST /internal/v1/workspaces",
  "POST /internal/v1/device-sessions/{user_code}/approve",
  "POST /internal/v1/device-sessions/{user_code}/approve-standup",
  "POST /internal/v1/workspaces/{ws}/roster/remove",
  "POST /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}/approve",
  "POST /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}/reject",
  "POST /internal/v1/workspaces/{ws}/skills/{skill}/reverts",
  // The skill lifecycle ceremonies — {skill} is the immutable skill_id on every one (the page
  // resolves the catalog name in its own loader; keying the wire on the id makes a concurrent
  // rename a harmless miss, never a wrong-target act).
  "POST /internal/v1/workspaces/{ws}/skills/{skill}/archive",
  "POST /internal/v1/workspaces/{ws}/skills/{skill}/unarchive",
  "POST /internal/v1/workspaces/{ws}/skills/{skill}/delete",
  "POST /internal/v1/workspaces/{ws}/skills/{skill}/purge",
  "POST /internal/v1/workspaces/{ws}/skills/{skill}/rename",
] as const;

export type AllowedRoute = (typeof ALLOWED_ROUTES)[number];

const allowed = new Set<string>(ALLOWED_ROUTES);

export function isAllowedRoute(method: string, template: string): boolean {
  return allowed.has(`${method.toUpperCase()} ${template}`);
}

/** Fill a `{param}` template; every value is URL-encoded (emails, hex ids, tokens). */
export function fillTemplate(template: string, params: Record<string, string>): string {
  return template.replace(/\{(\w+)\}/g, (_, key: string) => {
    const value = params[key];
    if (value === undefined) {
      throw new Error(`vault client: missing path param: ${key}`);
    }
    return encodeURIComponent(value);
  });
}

export interface VaultRequest {
  method: "GET" | "POST";
  template: string;
  params?: Record<string, string>;
  /** The session-verified acting principal — REQUIRED on the internal lane. */
  actingEmail?: string;
  body?: unknown;
  headers?: Record<string, string>;
}

/**
 * The transport. Refuses off-list routes; injects the internal-lane headers; never throws on
 * HTTP status (callers map status + body to typed results).
 */
export async function vaultFetch(req: VaultRequest): Promise<Response> {
  if (!isAllowedRoute(req.method, req.template)) {
    // Templates are curly-brace forms, never concrete URLs — safe to name.
    throw new Error(`vault client: route not allowlisted: ${req.method} ${req.template}`);
  }
  const env = serverEnv();
  const path = fillTemplate(req.template, req.params ?? {});
  const headers = new Headers(req.headers);
  if (req.template.startsWith("/internal/")) {
    if (!req.actingEmail) {
      throw new Error("vault client: internal lane requires an acting principal");
    }
    headers.set("authorization", `Bearer ${env.PLANE_INTERNAL_TOKEN}`);
    headers.set("x-topos-acting-email", req.actingEmail);
  }
  let body: string | undefined;
  if (req.body !== undefined) {
    headers.set("content-type", "application/json");
    body = JSON.stringify(req.body);
  }
  return await fetch(`${env.PLANE_INTERNAL_URL}${path}`, {
    method: req.method,
    headers,
    body,
  });
}

/**
 * The DOOR's pass-through half. The byte/pointer and enrollment/governance ops the vault must
 * decide itself — publish/propose/revert/review (receipt + replay-before-revoked ordering),
 * the pointer/object reads, credential minting, roster/revoke — cross this tier VERBATIM: the
 * device's own `Authorization` bearer rides through untouched and the vault's in-transaction
 * resolve stays the sole authority (this tier deliberately does NOT pre-authenticate a
 * forwarded request — a pre-auth check here could only drift from the vault's ordering).
 *
 * What this tier DOES enforce at the door:
 *  - the target is pinned under `/v1/` — the raw (still-encoded) path is checked segment by
 *    segment, so no traversal or encoding trick can reach the vault's `/internal/v1` lane
 *    through the public door;
 *  - only protocol headers cross (allowlist below) — in particular `x-topos-acting-email`
 *    and any cookie die here, so nothing session-shaped can be smuggled into the vault;
 *  - response headers cross by the same discipline (the wire's caching/ETag family, nothing
 *    hop-by-hop).
 */
const FORWARD_REQUEST_HEADERS = [
  "authorization",
  "accept",
  "content-type",
  "content-length",
  "if-none-match",
  "topos-known-version-id",
  "user-agent",
] as const;

const FORWARD_RESPONSE_HEADERS = [
  "cache-control",
  "content-type",
  "etag",
  "retry-after",
  "vary",
  "x-robots-tag",
] as const;

export async function forwardDeviceLane(request: Request): Promise<Response> {
  const url = new URL(request.url);
  const rawPath = url.pathname;
  if (rawPath !== "/api/v1" && !rawPath.startsWith("/api/v1/")) {
    return uniformNotFound();
  }
  // Traversal guard on the RAW segments: any dot-dot (plain or percent-encoded, any case) or
  // backslash refuses before a URL is built. The suffix below keeps the raw encoding, so what
  // the vault's router sees is byte-for-byte what the client sent past `/api`.
  for (const segment of rawPath.split("/")) {
    const lowered = segment.toLowerCase();
    if (
      segment === ".." ||
      segment.includes("\\") ||
      lowered.includes("%2e%2e") ||
      lowered.includes("%5c") ||
      lowered === ".%2e" ||
      lowered === "%2e."
    ) {
      return uniformNotFound();
    }
  }
  const suffix = rawPath.slice("/api".length);
  const headers = new Headers();
  for (const name of FORWARD_REQUEST_HEADERS) {
    const value = request.headers.get(name);
    if (value !== null) {
      headers.set(name, value);
    }
  }
  const method = request.method.toUpperCase();
  const init: RequestInit & { duplex?: "half" } = { method, headers };
  if (method !== "GET" && method !== "HEAD" && request.body !== null) {
    // Streamed through — publishes carry up to the vault's own write cap; nothing buffers here.
    init.body = request.body;
    init.duplex = "half";
  }
  let upstream: Response;
  try {
    upstream = await fetch(`${serverEnv().PLANE_INTERNAL_URL}${suffix}${url.search}`, init);
  } catch (error) {
    console.error("device-lane forward failed:", error);
    return internalError();
  }
  const out = new Headers();
  for (const name of FORWARD_RESPONSE_HEADERS) {
    const value = upstream.headers.get(name);
    if (value !== null) {
      out.set(name, value);
    }
  }
  return new Response(upstream.body, { status: upstream.status, headers: out });
}
