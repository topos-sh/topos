import { serverEnv } from "@/env.server";

/**
 * The ONE gateway transport — the custody lane's twin, and deliberately built the same way:
 *
 * - a runtime allowlist refuses any (method, path template) outside ALLOWED_ROUTES BEFORE a
 *   request is made — a future typed addition fails closed until it is listed here;
 * - authentication is the shared internal bearer alone. There is no acting-identity header: the
 *   gateway is told WHICH workspace, server and person a call is about in the body, and this tier
 *   has already decided whether the caller may ask (guards + seat rows), exactly as with the vault;
 * - the lane is OPTIONAL. A deployment that runs no gateway sets neither env var, `gatewayLane()`
 *   answers null, and every surface above degrades to not existing rather than to failing.
 *
 * THE SECRET RIDES ONE ROUTE. `credentials/manual` is the only request body that ever carries a
 * person's secret; it is written nowhere, logged nowhere, and never echoed back — the answer is an
 * id. Nothing in this module stringifies a request body for any purpose but sending it.
 */
export const ALLOWED_ROUTES = [
  // Start the authorize walk: the gateway does discovery + client registration and mints the
  // short-lived flow row, then hands back the upstream URL for this tier to send the person to.
  "POST /internal/v1/authorize/begin",
  // Store a pasted secret for a `manual`-tier server. The ONE route a secret travels.
  "POST /internal/v1/credentials/manual",
  // Revoke: the row goes, and the next call through the gateway has no credential to attach.
  "DELETE /internal/v1/credentials/{credentialId}",
  // Ask the server for its tool list again. The gateway holds the sign-in, so it is the only tier
  // that can ask; what comes back is rows in its own schema, which this tier then reads.
  "POST /internal/v1/tools/refresh",
] as const;

const allowed = new Set<string>(ALLOWED_ROUTES);

export function isAllowedRoute(method: string, template: string): boolean {
  return allowed.has(`${method.toUpperCase()} ${template}`);
}

/** Fill a `{param}` template; every value is URL-encoded (ids). */
export function fillTemplate(template: string, params: Record<string, string>): string {
  return template.replace(/\{(\w+)\}/g, (_, key: string) => {
    const value = params[key];
    if (value === undefined) {
      throw new Error(`gateway client: missing path param: ${key}`);
    }
    return encodeURIComponent(value);
  });
}

/** The internal lane's two halves, present together or not at all (env.server.ts pairs them). */
export interface GatewayLane {
  baseUrl: string;
  token: string;
}

/**
 * The lane, or null where this deployment runs no gateway. EVERY gateway-facing surface asks this
 * first: a null answer means the sections are not rendered and no gateway table is read (the
 * `gateway` schema does not exist on such an install, so a read would be an error, not an empty
 * list).
 */
export function gatewayLane(): GatewayLane | null {
  const env = serverEnv();
  if (env.GATEWAY_INTERNAL_URL === undefined || env.GATEWAY_INTERNAL_TOKEN === undefined) {
    return null;
  }
  return { baseUrl: env.GATEWAY_INTERNAL_URL, token: env.GATEWAY_INTERNAL_TOKEN };
}

export interface GatewayRequest {
  method: "POST" | "DELETE";
  template: string;
  params?: Record<string, string>;
  body?: unknown;
}

/**
 * The transport. Refuses off-list routes and an unconfigured lane; injects the internal bearer;
 * never throws on HTTP status (callers map status + body to typed results).
 */
export async function gatewayFetch(req: GatewayRequest): Promise<Response> {
  if (!isAllowedRoute(req.method, req.template)) {
    // Templates are curly-brace forms, never concrete URLs — safe to name.
    throw new Error(`gateway client: route not allowlisted: ${req.method} ${req.template}`);
  }
  const lane = gatewayLane();
  if (lane === null) {
    throw new Error("gateway client: no gateway is configured for this deployment");
  }
  const path = fillTemplate(req.template, req.params ?? {});
  const headers = new Headers();
  headers.set("authorization", `Bearer ${lane.token}`);
  let body: string | undefined;
  if (req.body !== undefined) {
    headers.set("content-type", "application/json");
    body = JSON.stringify(req.body);
  }
  return await fetch(`${lane.baseUrl}${path}`, { method: req.method, headers, body });
}
