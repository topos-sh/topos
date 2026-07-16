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
 * Every top-level STATIC path segment the OSS route table can register in either tenancy mode,
 * alphabetical and exhaustive. A vitest red-test derives the real segment set from
 * `ossRoutes()` (both modes) and fails when this list and the table drift — add a route, update
 * this list, or CI stays red.
 */
export const OSS_TOP_LEVEL_SEGMENTS: readonly string[] = [
  "account",
  "api",
  "app",
  "claim",
  "healthz",
  "install",
  "login",
  "new",
  "recovery",
  "verify",
];

/**
 * Words the product may plausibly need at the top level later (founder-approved future-reserve
 * list, 2026-07-15) — reserved NOW so no workspace squats an address a future page needs.
 * Extend judiciously; removals free real names, so treat the list as append-only.
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
