import { index, layout, prefix, type RouteConfigEntry, route } from "@react-router/dev/routes";

/**
 * The product app's route table as DATA — the first of the four composition seams.
 *
 * A deployment's `app/routes.ts` is one line: `export default ossRoutes()`. A downstream
 * superset build composes `[...ossRoutes({ dir }), ...itsOwnRoutes]`, where `dir` re-roots
 * every module file onto the checkout that holds this app's source (route `file` paths are
 * resolved relative to the consuming app's `appDirectory`). Composition is ADDITIVE-ONLY:
 * a downstream build appends routes; it never patches, forks, or shadows an entry here.
 *
 * Deliberately typegen-independent: these modules type their args with the generic
 * `LoaderFunctionArgs`/`ActionFunctionArgs`, never `./+types/*` imports, so the table works
 * unchanged when consumed as a package from another app directory.
 */
export interface OssRoutesOptions {
  /** Prefix prepended to every module file path (default: this app's own directory). */
  dir?: string;
}

export function ossRoutes(options: OssRoutesOptions = {}): RouteConfigEntry[] {
  const dir = options.dir ?? "";
  const file = (p: string) => `${dir}routes/${p}`;
  return [
    // Public, sessionless.
    index(file("landing.tsx")),
    route("login", file("login.tsx")),
    route("verify/:userCode", file("verify.tsx")),
    route("healthz", file("healthz.ts")),
    route("install", file("install.ts")),
    route("i/:token", file("claim-link.ts")),
    route("api/auth/*", file("api.auth.ts")),
    // Signed-in surface: one shell layout carries the session middleware + chrome.
    route("api/memberships", file("api.memberships.ts")),
    route("app", file("app-entry.tsx")),
    layout(file("shell.tsx"), [
      ...prefix("workspaces", [
        index(file("workspaces-index.tsx")),
        route("new", file("workspaces-new.tsx")),
        ...prefix(":ws", [
          index(file("workspace-dashboard.tsx")),
          route("settings", file("workspace-settings.tsx")),
          ...prefix("skills/:skill", [
            index(file("skill-current.tsx")),
            route("history", file("skill-history.tsx")),
            route("proposals", file("skill-proposals.tsx")),
            route("proposals/:versionId", file("proposal-review.tsx")),
            route("versions/:versionId", file("version-files.tsx")),
            route("versions/:versionId/files/*", file("file-view.tsx")),
          ]),
        ]),
      ]),
    ]),
    // Historical URL shapes kept honest: permanent redirects to the resource routes.
    route("create", file("redirect-create.ts")),
    route("link", file("redirect-link.ts")),
  ];
}
