/**
 * The reserved-name ground truth — a DEV-FREE module (no `@react-router/dev` import), so a
 * downstream server bundle can consume these constants without dragging the route-table DSL in.
 *
 * Two lists, one rule: a workspace NAME slug must never shadow a path the product (or its
 * operator) may need at the top level. In MULTI tenancy every top-level static segment shadows a
 * `/:ws` slug, so the workspace creator refuses every name below — indistinguishably from a taken
 * name (same message, same shape; the list is never enumerable through the form).
 */

/**
 * Every top-level STATIC path segment the OSS route table registers in MULTI tenancy —
 * alphabetical and exhaustive. These are exactly the segments that shadow a `/:ws` workspace
 * slug (single tenancy mounts more statics at the origin root, but no slug exists there to
 * shadow). A vitest red-test derives the real multi-mode segment set from `ossRoutes()` and
 * fails when this list and the table drift — add a route, update this list, or CI stays red.
 */
export const OSS_TOP_LEVEL_SEGMENTS: readonly string[] = [
  "account",
  "agent",
  "api",
  "app",
  "claim",
  "healthz",
  "install",
  "install.sh",
  "login",
  "new",
  "recovery",
  "verify",
];

/**
 * Words the product may plausibly need at the top level later — reserved NOW so no workspace
 * squats an address a future page needs. Curated; extend judiciously, and treat the list as
 * append-only (a removal frees a real name).
 */
export const FUTURE_RESERVED_SEGMENTS: readonly string[] = [
  "about",
  "assets",
  "blog",
  "cli",
  "cloud",
  "demo",
  "dev",
  "docs",
  "download",
  "help",
  "internal",
  "legal",
  "mail",
  "oss",
  "pricing",
  "privacy",
  "security",
  "static",
  "status",
  "support",
  "terms",
  // Also the CLI's built-in skill's reserved name — a workspace named `topos` would collide with
  // the product name everywhere a bare `topos` token is resolved.
  "topos",
  "webhooks",
  "www",
];

/**
 * The ONE reserved-name check every surface that writes `workspace.name` consults (creation
 * today; any future rename must call it too). `extra` is the composition's own additions — a
 * superset build passes its private top-level segments so no workspace name can occlude them.
 */
export function isReservedWorkspaceName(name: string, extra: readonly string[] = []): boolean {
  return (
    OSS_TOP_LEVEL_SEGMENTS.includes(name) ||
    FUTURE_RESERVED_SEGMENTS.includes(name) ||
    extra.includes(name)
  );
}
