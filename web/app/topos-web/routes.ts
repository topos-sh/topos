import { index, layout, prefix, type RouteConfigEntry, route } from "@react-router/dev/routes";

/**
 * The product app's route table as DATA — the first of the four composition seams.
 *
 * A deployment's `app/routes.ts` is one line: `export default ossRoutes()`. A downstream
 * superset build composes `[...ossRoutes({ dir, tenancy }), ...itsOwnRoutes]`, where `dir`
 * re-roots every module file onto the checkout that holds this app's source (route `file` paths
 * are resolved relative to the consuming app's `appDirectory`). Composition is ADDITIVE-ONLY: a
 * downstream build appends routes; it never patches, forks, or shadows an entry here.
 *
 * TWO URL grammars, ONE table, chosen by `tenancy`:
 *  - `single` (the OSS default): the install IS the one workspace, so the whole signed-in surface
 *    mounts at ORIGIN-ROOTED paths (`/`, `/members`, `/skills/:skill`). There is no "workspaces"
 *    concept in a browser URL.
 *  - `multi` (a downstream superset passes it): the same page modules mount under `/:ws`, where
 *    `:ws` is the workspace NAME slug (`workspace.name` — unique, already the shareable address).
 *    The opaque `workspace.id` stays the wire/DB key but never appears in a browser URL.
 *
 * Deliberately typegen-independent: these modules type their args with the generic
 * `LoaderFunctionArgs`/`ActionFunctionArgs`, never `./+types/*` imports, so the table works
 * unchanged when consumed as a package from another app directory.
 */
export interface OssRoutesOptions {
  /** Prefix prepended to every module file path (default: this app's own directory). */
  dir?: string;
  /** How this deployment addresses workspaces — see the module doc. Default `single`. */
  tenancy?: "single" | "multi";
}

// Every top-level STATIC segment this table registers lives in `segments.ts` (a dev-free module,
// so consumers of the constant never drag this file's `@react-router/dev` import into a server
// bundle). A vitest red-test keeps the list and this table in lockstep — add a route here, update
// `OSS_TOP_LEVEL_SEGMENTS` there, or CI stays red.

export function ossRoutes(options: OssRoutesOptions = {}): RouteConfigEntry[] {
  const dir = options.dir ?? "";
  const tenancy = options.tenancy ?? "single";
  const file = (p: string) => `${dir}routes/${p}`;

  // The three shareable FACES (workspace root · a skill · a channel): resource address and
  // canonical page are ONE route. They mount under face-shell.tsx (no login bounce — anonymous is
  // a valid state that renders the constant teaser). In single mode the workspace root is the
  // origin index; in multi it is `/:ws`.
  const faceChildren: RouteConfigEntry[] = [
    tenancy === "multi"
      ? route(":ws", file("workspace-dashboard.tsx"))
      : index(file("workspace-dashboard.tsx")),
    route(faceSub(tenancy, "skills/:skill"), file("skill-current.tsx")),
    route(faceSub(tenancy, "channels/:channel"), file("channel-detail.tsx")),
  ];

  // The member-only signed-in surface: every child mounts under shell.tsx (the login-bounce
  // layout). Same modules in both modes; only the path prefix differs.
  const memberChildren: RouteConfigEntry[] = [
    // The person-scoped device list is top-level in BOTH modes (a device is a possession of ONE
    // user, not a workspace resource).
    route("account/devices", file("your-devices.tsx")),
    // Self-serve workspace creation + onboarding — MULTI ONLY, top-level like account/devices (a
    // person, not a workspace, is its subject). Single-tenant mints its one workspace at boot, so
    // there is nothing to create and `/new` falls through to the house 404.
    ...(tenancy === "multi" ? [route("new", file("workspace-new.tsx"))] : []),
    ...memberWorkspaceChildren(tenancy, file),
  ];

  return [
    // ── Public, sessionless ──────────────────────────────────────────────────────────────────
    // The origin index: in single mode the workspace root is a FACE (mounted below); in multi it
    // is the marketing landing page (never a claim band).
    ...(tenancy === "multi" ? [index(file("landing.tsx"))] : []),
    route("login", file("login.tsx")),
    route("recovery", file("recovery.tsx")),
    // The first-boot claim is single-tenant only (multi mints no boot workspace). In multi,
    // `claim` is a reserved top-level segment that answers the house 404, so the `:ws` face can't
    // swallow it and it discloses nothing.
    tenancy === "multi" ? route("claim", file("reserved.tsx")) : route("claim", file("claim.tsx")),
    // The tokened invitation page — GET-safe viewing, explicit accept/decline POSTs. Origin-
    // rooted in single tenancy; nested under the workspace slug in multi (the static `invite`
    // segment outranks the face routes' params, so no face ever swallows it).
    tenancy === "multi"
      ? route(":ws/invite/:token", file("invite-redeem.tsx"))
      : route("invite/:token", file("invite-redeem.tsx")),
    // The ONE approve ceremony: a signed-in human confirms a device flow by its user code.
    route("verify", file("verify.tsx")),
    route("healthz", file("healthz.ts")),
    route("install", file("install.ts")),
    // The `.sh`-suffixed alias shells expect — same loader, byte-identical bytes and headers;
    // `/install` stays the canonical name.
    route("install.sh", file("install-sh.ts")),
    // The agent-onboarding document: what an agent told "set up Topos for us" fetches and follows.
    route("agent", file("agent.ts")),
    // The machine-discovery lane: llms.txt (the site guide convention) + the agent-skills
    // discovery index, whose ONE entry is the repo's downloadable `topos` skill. The skill's
    // three files serve under the SAME well-known base so relative sibling references resolve;
    // `.well-known/skills/` is the earlier index spelling, aliased byte-identically. All four
    // are deployment-scoped resource routes — origin-rooted in BOTH tenancy modes.
    route("llms.txt", file("llms-txt.ts")),
    route(".well-known/agent-skills/index.json", file("agent-skills-index.ts")),
    route(".well-known/agent-skills/topos/:file", file("agent-skills-file.ts")),
    route(".well-known/skills/index.json", file("agent-skills-index-legacy.ts")),
    route("api/auth/*", file("api.auth.ts")),
    // THE DEVICE LANE — `/api/v1` is the product's one public API, TERMINATING here since the
    // identity unification. `:ws` here is the opaque workspace ID (the wire/DB key), unchanged in
    // both tenancy modes. Static segments outrank the splat, which answers the uniform wire 404.
    route("api/v1/device/authorize", file("api.v1.device-authorize.ts")),
    route("api/v1/device/token", file("api.v1.device-token.ts")),
    // The already-enrolled device's invite-URL accept — person-scoped (seat-less by design).
    route("api/v1/invitations/accept", file("api.v1.invitation-accept.ts")),
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
    route("api/memberships", file("api.memberships.ts")),
    // The door into the product (a bare `/app`), then the two signed-in layouts.
    route("app", file("app-entry.tsx")),
    layout(file("face-shell.tsx"), faceChildren),
    layout(file("shell.tsx"), memberChildren),
    // Any unmatched path: the same constant card for a non-browser fetcher (served from the entry
    // before routing), the house 404 for a browser — path SHAPE decides the response.
    route("*", file("catch-all.tsx")),
  ];
}

/** Nest an in-workspace path under `/:ws` in multi mode; keep it origin-rooted in single. */
function faceSub(tenancy: "single" | "multi", sub: string): string {
  return tenancy === "multi" ? `:ws/${sub}` : sub;
}

/** The member-only pages, mounted origin-rooted (single) or under `/:ws` (multi). */
function memberWorkspaceChildren(
  tenancy: "single" | "multi",
  file: (p: string) => string,
): RouteConfigEntry[] {
  const children: RouteConfigEntry[] = [
    route("members", file("workspace-members.tsx")),
    route("archive", file("workspace-archive.tsx")),
    route("settings", file("workspace-settings.tsx")),
    // The workspace's device view (staleness + blind spots) — a tab of the Settings page.
    route("settings/devices", file("fleet.tsx")),
    // The whole-catalog export (a zip stream) — a resource route the Settings page links to.
    // Loader-only, so a document GET returns its Response directly; owner-gated in its loader.
    route("settings/export", file("workspace-export.ts")),
    // The channel index + the create form (Rails-style /channels/new); the channel FACE (the
    // Skills tab) lives under face-shell, its Members/History/Settings section tabs here.
    route("channels", file("channels-index.tsx")),
    route("channels/new", file("channel-new.tsx")),
    route("channels/:channel/members", file("channel-members.tsx")),
    route("channels/:channel/history", file("channel-history.tsx")),
    route("channels/:channel/settings", file("channel-settings.tsx")),
    // The skill subpages (the skill FACE itself is under face-shell). Member-only.
    route("skills/:skill/history", file("skill-history.tsx")),
    route("skills/:skill/proposals", file("skill-proposals.tsx")),
    route("skills/:skill/proposals/:versionId", file("proposal-review.tsx")),
    route("skills/:skill/settings", file("skill-settings.tsx")),
    route("skills/:skill/versions/:versionId", file("version-files.tsx")),
    route("skills/:skill/versions/:versionId/files/*", file("file-view.tsx")),
  ];
  return tenancy === "multi" ? prefix(":ws", children) : children;
}
