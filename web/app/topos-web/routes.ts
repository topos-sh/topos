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
    // The first-boot claim (the printed one-time link) + the mail-less recovery hatch.
    route("claim", file("claim.tsx")),
    route("recovery", file("recovery.tsx")),
    // The ONE approve ceremony: a signed-in human confirms a device flow by its user code.
    route("verify", file("verify.tsx")),
    route("healthz", file("healthz.ts")),
    route("install", file("install.ts")),
    route("api/auth/*", file("api.auth.ts")),
    // THE DEVICE LANE — `/api/v1` is the product's one public API, TERMINATING here since the
    // identity unification: the row ops run over this app's own schema, and the custody ops
    // (publish/propose/revert/review + the byte reads) are app-authorized orchestration over
    // the vault's internal custody lane. Static segments outrank the splat, which answers the
    // uniform wire 404 for everything unlisted.
    route("api/v1/device/authorize", file("api.v1.device-authorize.ts")),
    route("api/v1/device/token", file("api.v1.device-token.ts")),
    route("api/v1/publish", file("api.v1.publish.ts")),
    route("api/v1/proposals", file("api.v1.propose.ts")),
    route("api/v1/reverts", file("api.v1.reverts.ts")),
    route("api/v1/reviews", file("api.v1.reviews.ts")),
    ...prefix("api/v1/workspaces/:ws", [
      route("me", file("api.v1.me.ts")),
      route("channels", file("api.v1.channels.ts")),
      route("delivery", file("api.v1.delivery.ts")),
      route("report", file("api.v1.report.ts")),
      route("notices/ack", file("api.v1.notices-ack.ts")),
      route("invitations", file("api.v1.invitations.ts")),
      route("devices", file("api.v1.devices.ts")),
      route("proposals", file("api.v1.ws-proposals.ts")),
      route("skills", file("api.v1.skills-index.ts")),
      route("follows/:skill", file("api.v1.follows.ts")),
      route("exclusions/:skill", file("api.v1.exclusions.ts")),
      route("channels/:channel/membership", file("api.v1.channel-membership.ts")),
      route("channels/:channel/skills/:skill", file("api.v1.curation.ts")),
      route("channels/:channel/protection", file("api.v1.channel-protection.ts")),
      route("skills/:skill/reach", file("api.v1.skill-reach.ts")),
      route("skills/:skill/protection", file("api.v1.skill-protection.ts")),
      route("skills/:skill/current", file("api.v1.skill-current.ts")),
      route("skills/:skill/log", file("api.v1.skill-log.ts")),
      route("skills/:skill/proposals", file("api.v1.skill-proposals.ts")),
      route("skills/:skill/versions/:versionId", file("api.v1.skill-version.ts")),
      route("skills/:skill/bundles/:objectId", file("api.v1.skill-object.ts")),
    ]),
    route("api/v1/*", file("api.v1.$.ts")),
    // Signed-in surface: one shell layout carries the session middleware + chrome.
    route("api/memberships", file("api.memberships.ts")),
    route("app", file("app-entry.tsx")),
    layout(file("shell.tsx"), [
      route("settings/devices", file("your-devices.tsx")),
      ...prefix("workspaces", [
        index(file("workspaces-index.tsx")),
        ...prefix(":ws", [
          index(file("workspace-dashboard.tsx")),
          route("settings", file("workspace-settings.tsx")),
          route("members", file("workspace-members.tsx")),
          route("archive", file("workspace-archive.tsx")),
          route("fleet", file("fleet.tsx")),
          ...prefix("channels", [
            index(file("channels-index.tsx")),
            route(":channel", file("channel-detail.tsx")),
            route(":channel/history", file("channel-history.tsx")),
          ]),
          ...prefix("skills/:skill", [
            index(file("skill-current.tsx")),
            route("history", file("skill-history.tsx")),
            route("proposals", file("skill-proposals.tsx")),
            route("proposals/:versionId", file("proposal-review.tsx")),
            route("settings", file("skill-settings.tsx")),
            route("versions/:versionId", file("version-files.tsx")),
            route("versions/:versionId/files/*", file("file-view.tsx")),
          ]),
        ]),
      ]),
    ]),
    // Historical URL shapes kept honest: permanent redirects to the resource routes.
    route("create", file("redirect-create.ts")),
    route("link", file("redirect-link.ts")),
    // RESOURCE ADDRESSES — `<origin>/<workspace>[...]` is what sharing and joining speak. The
    // browser face is a page; every other fetcher gets the CONSTANT protocol card (no path
    // echo, no existence oracle). Static routes above always outrank these dynamic segments.
    route(":ws", file("resource-workspace.tsx")),
    route(":ws/channels/:name", file("resource-channel.tsx")),
    route(":ws/skills/:name", file("resource-skill.tsx")),
    // Any unmatched path: the same constant card for a non-browser fetcher, the house 404 for
    // a browser — path SHAPE decides the response, never existence.
    route("*", file("catch-all.tsx")),
  ];
}
