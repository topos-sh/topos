import { useParams } from "react-router";

/**
 * A bundle's KIND decides which part of the product it shows up in. A skill is a folder of
 * instructions; an MCP server is one address a team's agents call as tools. They share every
 * mechanic below the surface (one catalog, one version history, one delivery predicate) and
 * nothing above it: separate sidebar sections, separate ways in, separate addresses.
 *
 * This is the ONE place that mapping is written. The bundle pages mount TWICE — once under each
 * base — so a server is never addressed as a skill, and each mount fences itself to its kind
 * (a member who lands on the wrong one is sent to the canonical path; everyone else already met
 * the uniform 404 in the guard above).
 *
 * The mount a request arrived on is read off the ROUTE PARAM, not the route id: the MCP mount
 * names its param `server`, the skills mount `skill`. Route ids rename when a downstream build
 * re-roots these modules; a param name doesn't.
 */

/** The URL base a bundle's pages live under. */
export type BundleBase = "skills" | "mcp";

/** The catalog `kind` → the base that addresses it. Anything not `mcp` is a skill. */
export function baseForKind(kind: string | null | undefined): BundleBase {
  return kind === "mcp" ? "mcp" : "skills";
}

/** The section a base belongs to — the sidebar's label and the breadcrumb trail's first crumb. */
export function sectionLabel(base: BundleBase): string {
  return base === "mcp" ? "MCP servers" : "Skills";
}

/** What one of them is called in a sentence ("Assigning to everyone puts this <noun> …"). */
export function bundleNoun(base: BundleBase): string {
  return base === "mcp" ? "MCP server" : "skill";
}

/** Which mount this request came in on. */
export function baseOf(params: { skill?: string; server?: string }): BundleBase {
  return params.server !== undefined ? "mcp" : "skills";
}

/** The bundle NAME the URL carries, whichever mount it arrived on. */
export function bundleNameOf(params: { skill?: string; server?: string }): string {
  return (params.server ?? params.skill) as string;
}

/** A bundle page's in-workspace path (no leading slash — `wsHref`/`wsPathServer` root it).
 *  `tail` is the sub-page, leading slash included (`/history`), or empty for the face. */
export function bundlePath(base: BundleBase, name: string, tail = ""): string {
  return `${base}/${name}${tail}`;
}

/** The hook a page or component INSIDE the bundle route tree uses to build its own links —
 *  it reads the mount off the route params, so one component serves both bases. */
export function useBundleBase(): BundleBase {
  return baseOf(useParams());
}
