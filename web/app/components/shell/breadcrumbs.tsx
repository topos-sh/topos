import { Link, type Params, useMatches } from "react-router";
import type { SidebarWorkspace } from "@/lib/shell/chrome.server";
import { wsHref } from "@/lib/ws-path";

/**
 * The global breadcrumb trail — rendered UNDER the page title on every signed-in page (it used to
 * live in the top header bar; it moved below the title so a page's own heading leads and the trail
 * sits beneath it as context). It is SELF-SUFFICIENT: no props feed it the chrome. Two things drive
 * it: React Router's `useMatches()` (the active route branch) and the ONE central REGISTRY below
 * (route module id → a crumb builder). This app centralizes chrome knowledge — the nav registry,
 * the sidebar's link building — rather than scattering `handle` exports across route files, so ALL
 * breadcrumb knowledge lives here, in one place, and a route the registry doesn't know contributes
 * nothing.
 *
 * The trail is: the workspace itself (its display name → its dashboard, from the layout chrome,
 * only when a workspace is in scope) followed by the DEEPEST match the registry has an entry for.
 * Builders are handed that match's loader `data` and `params` and must be DEFENSIVE — the same
 * module can load a teaser variant with no page data, so a builder returns `null`/`[]` when its
 * shape is missing and the trail degrades to root-only rather than crashing.
 *
 * The chrome (the active workspace + the tenancy grammar) is read off whichever LAYOUT loader
 * carries it, found by loader-data SHAPE rather than a hardcoded route id — the downstream superset
 * re-roots these same modules under a different directory (which renames route ids), so shape
 * detection is the composition-safe read. When no match carries chrome (an anonymous face, a page
 * outside both signed-in shells) the trail renders nothing.
 *
 * Presentation only. It reads no `:ws` param (this component renders inside a page's title block,
 * so it can't rely on seeing the workspace segment cleanly) — links build the sidebar's way, from
 * the loader-supplied workspace address + tenancy through `wsHref`.
 */

/** One trail segment. `sub` is the workspace-relative path (no leading slash) fed to `wsHref`;
 *  omit it for a segment that isn't a link (e.g. the unlinked "Skills" header). The empty string
 *  links to the workspace root. The LAST crumb is always rendered unlinked (the current page). */
interface Crumb {
  label: string;
  sub?: string;
}

/** A registry entry: derive a route's crumb tail from its match. Returns the segments AFTER the
 *  workspace root, or null when the loader shape isn't the page variant (→ root-only). */
type CrumbBuilder = (match: { data: unknown; params: Params }) => Crumb[] | null;

/** The skill crumb's label: the catalog display name when the loader carried one, else the URL
 *  name — mirrors how `SkillHeader` titles a skill (`displayName ?? name`). */
function skillLabel(data: unknown, urlName: string): string {
  const display = (data as { displayName?: unknown } | null)?.displayName;
  return typeof display === "string" && display.length > 0 ? display : urlName;
}

/** A channel tab's trail: Channels → #name → <tab>. Channel names carry no separate display, so
 *  the URL segment IS the name; a missing segment (shouldn't happen on a loaded page) → root-only. */
function channelTab(params: Params, tab: string): Crumb[] | null {
  const ch = params.channel;
  if (ch === undefined) {
    return null;
  }
  return [
    { label: "Channels", sub: "channels" },
    { label: `#${ch}`, sub: `channels/${ch}` },
    { label: tab },
  ];
}

/** A skill tab's trail: Skills (unlinked — there is no skills index page) → <name> → <tab>. */
function skillTab(data: unknown, params: Params, tab: string): Crumb[] | null {
  const name = params.skill;
  if (name === undefined) {
    return null;
  }
  return [
    { label: "Skills" },
    { label: skillLabel(data, name), sub: `skills/${name}` },
    { label: tab },
  ];
}

/** The file-view tail: the manifest path the loader resolved (the one truth — never the raw URL),
 *  falling back to the splat, else nothing (→ the version short-id becomes the current crumb). */
function fileTail(data: unknown, params: Params): string | undefined {
  const segments = (data as { displaySegments?: unknown } | null)?.displaySegments;
  if (
    Array.isArray(segments) &&
    segments.length > 0 &&
    segments.every((s) => typeof s === "string")
  ) {
    return segments.join("/");
  }
  const splat = params["*"];
  return splat !== undefined && splat.length > 0 ? splat : undefined;
}

/**
 * THE registry — route module id (React Router derives it from the route file: `routes/<name>`) →
 * its crumb tail. This is the single place breadcrumb knowledge is recorded; adding a page's trail
 * is one line here, and an unlisted route simply renders root-only.
 */
const REGISTRY: Record<string, CrumbBuilder> = {
  // The workspace root itself — the trail is just the root crumb (added by the component).
  "routes/workspace-dashboard": () => [],

  // Channels.
  "routes/channels-index": () => [{ label: "Channels" }],
  "routes/channel-new": () => [{ label: "Channels", sub: "channels" }, { label: "New channel" }],
  "routes/channel-detail": ({ params }) => {
    const ch = params.channel;
    return ch === undefined ? null : [{ label: "Channels", sub: "channels" }, { label: `#${ch}` }];
  },
  "routes/channel-history": ({ params }) => channelTab(params, "History"),
  "routes/channel-settings": ({ params }) => channelTab(params, "Settings"),

  // Skills. There is no skills index page, so the "Skills" root is an unlinked segment.
  "routes/skill-current": ({ data, params }) => {
    const name = params.skill;
    return name === undefined ? null : [{ label: "Skills" }, { label: skillLabel(data, name) }];
  },
  "routes/skill-history": ({ data, params }) => skillTab(data, params, "History"),
  "routes/skill-proposals": ({ data, params }) => skillTab(data, params, "Proposals"),
  "routes/skill-settings": ({ data, params }) => skillTab(data, params, "Settings"),
  "routes/proposal-review": ({ data, params }) => {
    const name = params.skill;
    const versionId = params.versionId;
    if (name === undefined || versionId === undefined) {
      return null;
    }
    return [
      { label: "Skills" },
      { label: skillLabel(data, name), sub: `skills/${name}` },
      { label: "Proposals", sub: `skills/${name}/proposals` },
      { label: versionId.slice(0, 12) },
    ];
  },
  "routes/version-files": ({ data, params }) => {
    const name = params.skill;
    const versionId = params.versionId;
    if (name === undefined || versionId === undefined) {
      return null;
    }
    return [
      { label: "Skills" },
      { label: skillLabel(data, name), sub: `skills/${name}` },
      { label: versionId.slice(0, 12) },
    ];
  },
  "routes/file-view": ({ data, params }) => {
    const name = params.skill;
    const versionId = params.versionId;
    if (name === undefined || versionId === undefined) {
      return null;
    }
    const crumbs: Crumb[] = [
      { label: "Skills" },
      { label: skillLabel(data, name), sub: `skills/${name}` },
      { label: versionId.slice(0, 12), sub: `skills/${name}/versions/${versionId}` },
    ];
    const tail = fileTail(data, params);
    if (tail !== undefined) {
      crumbs.push({ label: tail });
    }
    return crumbs;
  },

  // Workspace nav.
  "routes/workspace-members": () => [{ label: "Members" }],
  "routes/workspace-settings": () => [{ label: "Settings" }],
  "routes/sessions": () => [{ label: "Settings", sub: "settings" }, { label: "Sessions" }],
  "routes/workspace-archive": () => [{ label: "Settings", sub: "settings" }, { label: "Archive" }],

  // Account-scoped (top-level in both tenancies) — mirrors the page's own title.
  "routes/your-sessions": () => [{ label: "Your sessions" }],

  // Self-serve workspace creation (multi tenancy only, off-workspace — no root crumb precedes it).
  "routes/workspace-new": () => [{ label: "New workspace" }],
};

/** The shared chrome the two signed-in layouts load — the active workspace + the tenancy grammar
 *  the trail's links build under. */
interface Chrome {
  workspace: SidebarWorkspace | null;
  tenancy: "single" | "multi";
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

/** The `ChromeData` shape guard: a `tenancy` of the known grammar plus a `workspace` key (null when
 *  off-workspace, else the sidebar workspace). Defensive — an unexpected shape degrades to null
 *  rather than throwing. */
function chromeShape(data: unknown): Chrome | null {
  if (!isRecord(data)) {
    return null;
  }
  const { tenancy } = data;
  if ((tenancy !== "single" && tenancy !== "multi") || !("workspace" in data)) {
    return null;
  }
  return { workspace: workspaceShape(data.workspace), tenancy };
}

/** The trail reads only `displayName` + `address` off the workspace; validate those and degrade a
 *  malformed row to "no workspace" (a root-less trail) rather than crashing the header. */
function workspaceShape(data: unknown): SidebarWorkspace | null {
  if (!isRecord(data)) {
    return null;
  }
  return typeof data.displayName === "string" && typeof data.address === "string"
    ? (data as unknown as SidebarWorkspace)
    : null;
}

/**
 * Pull the chrome out of a layout match's loader data, in EITHER shape a signed-in layout returns:
 * `shell.tsx` hands back `ChromeData` directly (a `{ tenancy, workspace, … }` object), while
 * `face-shell.tsx` wraps it as `{ signedIn: true, chrome: ChromeData }` (or `{ signedIn: false }`
 * for an anonymous face — no chrome). Route ids are deliberately NOT matched here: the downstream
 * superset build re-roots these same modules under a different directory, which renames the ids;
 * the loader-data SHAPE is the composition-safe signal. Returns null for any match that isn't a
 * chrome-carrying layout (a page loader, the anonymous face, a sessionless route).
 */
function chromeFromLoaderData(data: unknown): Chrome | null {
  const direct = chromeShape(data);
  if (direct !== null) {
    return direct;
  }
  if (isRecord(data) && data.signedIn === true) {
    return chromeShape(data.chrome);
  }
  return null;
}

/**
 * The breadcrumb bar. Renders nothing when there's no chrome (an anonymous face, a page outside the
 * signed-in shells) or no trail to show (an off-workspace page whose route the registry doesn't
 * cover). Single line: long names ellipsize (flexbox shrinks the longest segment first) rather than
 * wrapping. `className` lets a call site add the small top margin that seats it under the title —
 * kept on the `<nav>` (not a wrapper) so a null render leaves no phantom spacing.
 */
export function Breadcrumbs({ className = "" }: { className?: string }) {
  const matches = useMatches();

  // The chrome (the active workspace + tenancy) rides a LAYOUT loader — found by SHAPE, not route
  // id, so the superset's re-rooted module ids don't matter. No chrome ⇒ no trail (an anonymous
  // face renders no chrome; a page outside both shells has none to find).
  let chrome: Chrome | null = null;
  for (let i = matches.length - 1; i >= 0; i--) {
    const match = matches[i];
    const found = match === undefined ? null : chromeFromLoaderData(match.loaderData);
    if (found !== null) {
      chrome = found;
      break;
    }
  }
  if (chrome === null) {
    return null;
  }
  const { workspace, tenancy } = chrome;

  // The DEEPEST match the registry knows drives the tail; everything above it (layouts, the door)
  // is skipped. A `null` build (a teaser/empty loader variant) degrades to root-only.
  let tail: Crumb[] = [];
  for (let i = matches.length - 1; i >= 0; i--) {
    const match = matches[i];
    const build = match === undefined ? undefined : REGISTRY[match.id];
    if (build !== undefined && match !== undefined) {
      tail = build({ data: match.loaderData, params: match.params }) ?? [];
      break;
    }
  }

  const crumbs: Crumb[] =
    workspace !== null ? [{ label: workspace.displayName, sub: "" }, ...tail] : tail;
  if (crumbs.length === 0) {
    return null;
  }

  // Links build from the loader-supplied address + tenancy (the sidebar's rule), never useParams:
  // the trail derives its scope from the chrome, not a child route's `:ws` segment.
  const wsSegment = workspace !== null && tenancy === "multi" ? workspace.address : null;

  return (
    <nav aria-label="Breadcrumb" className={`min-w-0 ${className}`.trimEnd()}>
      <ol className="flex min-w-0 items-center gap-1.5 font-mono text-dim text-xs">
        {crumbs.map((crumb, index) => {
          const isLast = index === crumbs.length - 1;
          const href =
            !isLast && crumb.sub !== undefined ? wsHref(wsSegment, crumb.sub) : undefined;
          return (
            <li
              // The trail is a fixed positional list (no stable id per segment) — index-keyed by design.
              // biome-ignore lint/suspicious/noArrayIndexKey: positional, stable within one render
              key={index}
              className="flex min-w-0 items-center gap-1.5"
            >
              {index > 0 && (
                <span aria-hidden="true" className="shrink-0 text-faint">
                  /
                </span>
              )}
              {href !== undefined ? (
                <Link
                  to={href}
                  className="min-w-0 truncate text-dim transition-colors hover:text-ink focus-visible:rounded-sm focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
                >
                  {crumb.label}
                </Link>
              ) : (
                <span
                  className={`min-w-0 truncate ${isLast ? "text-ink" : "text-dim"}`}
                  aria-current={isLast ? "page" : undefined}
                >
                  {crumb.label}
                </span>
              )}
            </li>
          );
        })}
      </ol>
    </nav>
  );
}
