import { serverEnv } from "@/env.server";

/**
 * The ONE vault transport. The vault is PURE BYTE CUSTODY now — versions, pointers, objects —
 * and its whole HTTP surface is the internal custody lane this module speaks:
 *
 * - a runtime allowlist refuses any (method, path template) outside ALLOWED_ROUTES BEFORE a
 *   request is made — even a future typed addition fails closed;
 * - authentication is the shared internal bearer alone. There is NO acting-identity header:
 *   the vault is identity-free by design — authorization happened HERE (the app's guards +
 *   seat rows decide every request), and the vault records only pass-through display strings
 *   the request body carries;
 * - there is no other credential anywhere: this tier holds no signing key and computes no
 *   digest — content addressing is the vault's, policy is the database's.
 *
 * Every vault read is per-request fresh; nothing caches across requests except the
 * version-content LRU (immutable, content-addressed — see version-cache.server.ts).
 */
export const ALLOWED_ROUTES = [
  // Commit-only ingest (a proposal's candidate; no pointer move).
  "POST /internal/v1/workspaces/{ws}/bundles/{bundle}/versions",
  // Commit + CAS pointer move in one op (genesis passes a null expected generation).
  "POST /internal/v1/workspaces/{ws}/bundles/{bundle}/publish",
  // CAS-move the pointer onto an EXISTING version (review approve).
  "POST /internal/v1/workspaces/{ws}/bundles/{bundle}/pointer",
  // The forward revert: a new commit carrying the good version's tree, then the CAS move.
  "POST /internal/v1/workspaces/{ws}/bundles/{bundle}/revert",
  "GET /internal/v1/workspaces/{ws}/bundles/{bundle}/current",
  "GET /internal/v1/workspaces/{ws}/bundles/{bundle}/versions/{version_id}",
  "GET /internal/v1/workspaces/{ws}/bundles/{bundle}/log",
  // One content-addressed object's raw bytes (the browse pages + the device bundle read).
  "GET /internal/v1/workspaces/{ws}/bundles/{bundle}/objects/{object_id}",
  // Byte-purge one version (tombstone; the hash stays) / drop a whole bundle's custody.
  "POST /internal/v1/workspaces/{ws}/bundles/{bundle}/versions/{version_id}/purge",
  "DELETE /internal/v1/workspaces/{ws}/bundles/{bundle}",
] as const;

export type AllowedRoute = (typeof ALLOWED_ROUTES)[number];

const allowed = new Set<string>(ALLOWED_ROUTES);

export function isAllowedRoute(method: string, template: string): boolean {
  return allowed.has(`${method.toUpperCase()} ${template}`);
}

/** Fill a `{param}` template; every value is URL-encoded (ids, hex hashes). */
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
  method: "GET" | "POST" | "DELETE";
  template: string;
  params?: Record<string, string>;
  body?: unknown;
  headers?: Record<string, string>;
}

/**
 * The transport. Refuses off-list routes; injects the internal bearer; never throws on HTTP
 * status (callers map status + body to typed results).
 */
export async function vaultFetch(req: VaultRequest): Promise<Response> {
  if (!isAllowedRoute(req.method, req.template)) {
    // Templates are curly-brace forms, never concrete URLs — safe to name.
    throw new Error(`vault client: route not allowlisted: ${req.method} ${req.template}`);
  }
  const env = serverEnv();
  const path = fillTemplate(req.template, req.params ?? {});
  const headers = new Headers(req.headers);
  headers.set("authorization", `Bearer ${env.PLANE_INTERNAL_TOKEN}`);
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
