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
    // THE DEVICE LANE — `/api/v1` is the product's one public API, served here since the door
    // cutover. Row ops run in this tier through the guarded `topos_*` functions under the
    // scoped role (each route authenticates the workspace credential via `requireDeviceActor`);
    // everything else — byte/pointer ops, enrollment, governance — falls through the static
    // routes below onto the `api/v1/*` splat, which forwards VERBATIM to the vault on the
    // internal network. Static segments outrank the splat, so each listed route wins its path.
    // The REVIEW INBOX (`workspaces/:ws/proposals`) and the SKILL LOG (`…/skills/:skill/log`) are
    // deliberately NOT served here: both decorate their rows with git commit messages — byte
    // custody this tier does not hold — so they ride the splat to the vault like every byte op.
    ...prefix("api/v1/workspaces/:ws", [
      route("me", file("api.v1.me.ts")),
      route("channels", file("api.v1.channels.ts")),
      route("delivery", file("api.v1.delivery.ts")),
      route("report", file("api.v1.report.ts")),
      route("notices/ack", file("api.v1.notices-ack.ts")),
      route("invitations", file("api.v1.invitations.ts")),
      route("follows/:skill", file("api.v1.follows.ts")),
      route("exclusions/:skill", file("api.v1.exclusions.ts")),
      route("channels/:channel/membership", file("api.v1.channel-membership.ts")),
      route("channels/:channel/skills/:skill", file("api.v1.curation.ts")),
      route("channels/:channel/protection", file("api.v1.channel-protection.ts")),
      route("skills/:skill/reach", file("api.v1.skill-reach.ts")),
      route("skills/:skill/protection", file("api.v1.skill-protection.ts")),
    ]),
    // The passcode START is served here since the mail unification — the one enrollment step
    // with a mail side effect: mint over the vault's internal lane, mail through the app's ONE
    // seam, answer the constant ack (the confirm keeps riding the splat to the vault, which
    // pins this start's wire with a contract stub).
    route("api/v1/enroll/passcode", file("api.v1.enroll.passcode.ts")),
    route("api/v1/*", file("api.v1.$.ts")),
    // The claim resource ALSO lives under the API base — `{api_base_url}/i/<token>` — the same
    // passthrough the origin-root `/i/` link serves, mounted twice. TIER PARITY: on the vault the
    // API base IS the root, so `{base}/i/` always resolves there; this mount keeps that true when
    // the app is the serving tier, and a claim link rooted at either base enrolls identically.
    route("api/i/:token", file("claim-link.ts"), { id: "routes/claim-link-api" }),
    // Signed-in surface: one shell layout carries the session middleware + chrome.
    route("api/memberships", file("api.memberships.ts")),
    route("app", file("app-entry.tsx")),
    layout(file("shell.tsx"), [
      route("settings/devices", file("your-devices.tsx")),
      ...prefix("workspaces", [
        index(file("workspaces-index.tsx")),
        route("new", file("workspaces-new.tsx")),
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
