import { serverEnv } from "@/env.server";

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
