import { index, layout, prefix, type RouteConfigEntry, route } from "@react-router/dev/routes";
import { BUNDLE_KINDS } from "../lib/bundle-base";

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

  // The four shareable FACES (workspace root · a skill · an MCP server · a channel): resource
  // address and canonical page are ONE route. They mount under face-shell.tsx (no login bounce —
  // anonymous is a valid state that renders the constant teaser). In single mode the workspace
  // root is the origin index; in multi it is `/:ws`.
  //
  // A skill and an MCP server are the SAME page module under TWO bases: the kind decides which
  // one addresses a given bundle, and each mount fences itself (app/lib/bundle-base.server.ts).
  // The mount is told apart by its PARAM NAME (`:skill` vs `:server`), never by route id — a
  // downstream build re-roots these modules, which renames ids. Both mounts are BUILT from the
  // per-kind records (app/lib/bundle-base.ts), so a kind cannot be half-addressable.
  const faceChildren: RouteConfigEntry[] = [
    tenancy === "multi"
      ? route(":ws", file("workspace-dashboard.tsx"))
      : index(file("workspace-dashboard.tsx")),
    ...bundleMounts(BUNDLE_FACE, file, (sub) => faceSub(tenancy, sub)),
    route(faceSub(tenancy, "channels/:channel"), file("channel-detail.tsx")),
  ];

  // The member-only signed-in surface: every child mounts under shell.tsx (the login-bounce
  // layout). Same modules in both modes; only the path prefix differs.
  const memberChildren: RouteConfigEntry[] = [
    // The person-scoped session list is top-level in BOTH modes (a session belongs to ONE
    // user, not to the workspace page tree).
    route("account/sessions", file("your-sessions.tsx")),
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
    // The ONE approve ceremony: a signed-in human confirms a login flow by its user code.
    route("verify", file("verify.tsx")),
    route("healthz", file("healthz.ts")),
    route("install", file("install.ts")),
    // The `.sh`-suffixed alias shells expect — same loader, byte-identical bytes and headers;
    // `/install` stays the canonical name.
    route("install.sh", file("install-sh.ts")),
    // The OTHER install path: the self-host deployment file, so standing up a server is a curl
    // and a `docker compose up` rather than a clone and a build.
    route("compose.yml", file("compose-yml.ts")),
    // Its companion: the database provisioning script that compose file mounts.
    route("compose-init-db.sh", file("compose-init-db.ts")),
    // The agent-onboarding document: what an agent told "set up Topos for us" fetches and follows.
    route("agent", file("agent.ts")),
    // THE DOCUMENTATION, rendered by this app from the repo's own MDX (compiled into
    // app/lib/docs/ at build time). Origin-rooted in BOTH tenancy modes — docs describe the
    // DEPLOYMENT, not a workspace — and the static `docs` segment outranks the `:ws` face, which
    // is why it is registered in `segments.ts` (no workspace may take the name).
    //
    // Two faces, decided by PATH SHAPE, never by content negotiation: the page (`/docs`,
    // `/docs/<page>`) and its plain-markdown twin for an agent (`<the same path>.md`). The `.md`
    // twins are dynamic segments with a literal suffix, one route per nesting depth — the depth
    // ceiling the page-id contract enforces (see scripts/docs/compile.mjs).
    route("docs", file("docs-page.tsx"), { id: "docs-index" }),
    route("docs/llms.txt", file("docs-llms-txt.ts")),
    route("docs.md", file("docs-markdown.ts"), { id: "docs-index-md" }),
    route("docs/:page.md", file("docs-markdown.ts"), { id: "docs-md-1" }),
    route("docs/:section/:page.md", file("docs-markdown.ts"), { id: "docs-md-2" }),
    route("docs/:section/:subsection/:page.md", file("docs-markdown.ts"), { id: "docs-md-3" }),
    route("docs/*", file("docs-page.tsx"), { id: "docs-page" }),
    // The machine-discovery lane: llms.txt (the site guide convention) + the agent-skills
    // discovery index, whose ONE entry is the repo's downloadable `topos` skill. The skill's
    // files serve under the SAME well-known base so relative sibling references resolve;
    // `.well-known/skills/` is the earlier index spelling, aliased byte-identically. All four
    // are deployment-scoped resource routes — origin-rooted in BOTH tenancy modes.
    route("llms.txt", file("llms-txt.ts")),
    route(".well-known/agent-skills/index.json", file("agent-skills-index.ts")),
    route(".well-known/agent-skills/topos/:file", file("agent-skills-file.ts")),
    route(".well-known/skills/index.json", file("agent-skills-index-legacy.ts")),
    route("api/auth/*", file("api.auth.ts")),
    // THE SESSION LANE — `/api/v1` is the product's one public API, TERMINATING here since the
    // identity unification. `:ws` here is the opaque workspace ID (the wire/DB key), unchanged in
    // both tenancy modes. Static segments outrank the splat, which answers the uniform wire 404.
    route("api/v1/login/authorize", file("api.v1.login-authorize.ts")),
    route("api/v1/login/token", file("api.v1.login-token.ts")),
    // The lane-side second connect: a further workspace's session over an existing credential.
    route("api/v1/login/connect", file("api.v1.login-connect.ts")),
    // The session self-revoke (`topos logout`): the presented credential names its OWN session.
    route("api/v1/session", file("api.v1.session.ts")),
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
      route("proposals", file("api.v1.ws-proposals.ts")),
      route("skills", file("api.v1.skills-index.ts")),
      // No route writes a person's feed: what the server says someone should have is decided
      // here (a curator's assignment, or their own click), never by a machine they logged in
      // from. The retired `profile*` paths fall through to the splat's uniform 404.
      route("channels/:channel/protection", file("api.v1.channel-protection.ts")),
      route("skills/:skill/protection", file("api.v1.skill-protection.ts")),
      route("skills/:skill/current", file("api.v1.skill-current.ts")),
      route("skills/:skill/log", file("api.v1.skill-log.ts")),
      route("skills/:skill/proposals", file("api.v1.skill-proposals.ts")),
      route("skills/:skill/versions/:versionId", file("api.v1.skill-version.ts")),
      route("skills/:skill/bundles/:objectId", file("api.v1.skill-object.ts")),
    ]),
    route("api/v1/*", file("api.v1.$.ts")),
    route("api/memberships", file("api.memberships.ts")),
    // THE WORKSPACE'S MCP REGISTRY — the official read API (`v0.1`) served over this
    // workspace's `kind: 'mcp'` catalog, so an agent already pointed at a registry can be
    // pointed here instead. Workspace-scoped in BOTH grammars (origin-rooted in single, under
    // the `/:ws` slug in multi) and member-gated by either door, cookie or bearer. ONE splat
    // module owns the three shapes: the list, a name's versions, and its latest — the server
    // NAME carries a percent-encoded slash, so the module parses the raw path rather than a
    // decoded param.
    route(faceSub(tenancy, "registry/v0.1/*"), file("mcp-registry.ts")),
    // THE PUBLIC CATALOG FEED — the same read API over this DEPLOYMENT's global MCP catalog, for
    // another install to sync from. Deployment-scoped, so origin-rooted in both grammars, and
    // served only where the deployment turned it on.
    route("mcp-catalog/v0.1/*", file("mcp-catalog-feed.ts")),
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

/** One page of a bundle: the path tail under `<base>/:<param>`, the module, and the name its
 *  second mount's route id is built from. */
interface BundlePage {
  /** The id suffix a second mount takes (`mcp-history`) — also how the breadcrumb registry
   *  spells its double registration. */
  page: string;
  /** Appended to `<base>/:<param>`; empty for the face itself. */
  tail: string;
  file: string;
}

/** The shareable FACE — the bundle's canonical page, mounted under face-shell.tsx. */
const BUNDLE_FACE: readonly BundlePage[] = [
  { page: "current", tail: "", file: "skill-current.tsx" },
];

/** The member-only sub-pages, one mount per kind. */
const BUNDLE_SUBPAGES: readonly BundlePage[] = [
  { page: "history", tail: "/history", file: "skill-history.tsx" },
  { page: "proposals", tail: "/proposals", file: "skill-proposals.tsx" },
  { page: "proposal-review", tail: "/proposals/:versionId", file: "proposal-review.tsx" },
  { page: "settings", tail: "/settings", file: "skill-settings.tsx" },
  { page: "versions", tail: "/versions/:versionId", file: "version-files.tsx" },
  { page: "file-view", tail: "/versions/:versionId/files/*", file: "file-view.tsx" },
];

/**
 * Mount each page once PER KIND: `<base>/:<param><tail>`, in the registry's order (skills, then
 * MCP). The first kind's mount keeps React Router's file-derived id; every further mount of the
 * same module needs an explicit one, which the kind's record supplies — those strings are chrome
 * keys (the breadcrumb registry looks a match up by id), so they are named data, not incidental.
 *
 * `at` roots each path for the tree it is going into: the faces carry their own `:ws` segment in
 * multi tenancy, while the member children are prefixed wholesale further down.
 */
function bundleMounts(
  pages: readonly BundlePage[],
  file: (p: string) => string,
  at: (sub: string) => string,
): RouteConfigEntry[] {
  return BUNDLE_KINDS.flatMap((kind) =>
    pages.map((page) => {
      const path = at(`${kind.base}/:${kind.paramName}${page.tail}`);
      return kind.routeIdPrefix === null
        ? route(path, file(page.file))
        : route(path, file(page.file), { id: `${kind.routeIdPrefix}-${page.page}` });
    }),
  );
}

/** The member-only pages, mounted origin-rooted (single) or under `/:ws` (multi). */
function memberWorkspaceChildren(
  tenancy: "single" | "multi",
  file: (p: string) => string,
): RouteConfigEntry[] {
  const children: RouteConfigEntry[] = [
    // The person's own feed ("Your skills"): what is assigned to them, and the switches that
    // add one to their own feed or turn one off.
    route("profile", file("profile.tsx")),
    // The disclosure page: what this workspace can and cannot read from a member's machines,
    // limits first, with that member's own reported rows as the proof.
    route("visibility", file("visibility.tsx")),
    // EACH SECTION'S OWN WAY IN, one per kind, from the same records that build the mounts
    // below — so a kind can never gain pages without gaining the door that fills them. Skills:
    // add-from-GitHub (server-side fetch → preview → publish WITH upstream provenance). MCP:
    // pick from the built-in list, or name / fetch / paste a document → publish as a
    // `kind: 'mcp'` bundle whose one file is the server document. Neither door offers the other
    // kind. Each path's STATIC first pair of segments outranks its kind's `:<param>` mount.
    ...BUNDLE_KINDS.map((kind) => route(kind.newPagePath, file(kind.newPageFile))),
    route("members", file("workspace-members.tsx")),
    route("settings/archive", file("workspace-archive.tsx")),
    route("settings", file("workspace-settings.tsx")),
    // The workspace's sessions view (approve/remove + applied state) — a Settings tab.
    route("settings/sessions", file("sessions.tsx")),
    // The whole-catalog export (a zip stream) — a resource route the Settings page links to.
    // Loader-only, so a document GET returns its Response directly; owner-gated in its loader.
    route("settings/export", file("workspace-export.ts")),
    // The channel index + the create form (Rails-style /channels/new); the channel FACE (the
    // Skills tab) lives under face-shell, its Members/History/Settings section tabs here.
    route("channels", file("channels-index.tsx")),
    route("channels/new", file("channel-new.tsx")),
    route("channels/:channel/history", file("channel-history.tsx")),
    route("channels/:channel/settings", file("channel-settings.tsx")),
    // The bundle sub-pages (the FACE itself is under face-shell), one mount per kind: ONE module
    // each, kind-fenced, so a server's tabs stay inside the MCP section instead of walking the
    // reader back into Skills. Member-only.
    ...bundleMounts(BUNDLE_SUBPAGES, file, (sub) => sub),
  ];
  return tenancy === "multi" ? prefix(":ws", children) : children;
}
