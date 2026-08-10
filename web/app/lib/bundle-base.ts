import { useParams } from "react-router";

/**
 * A bundle's KIND decides which part of the product it shows up in. A skill is a folder of
 * instructions; an MCP server is one address a team's agents call as tools. They share every
 * mechanic below the surface (one catalog, one version history, one delivery predicate) and
 * nothing above it: separate sidebar sections, separate ways in, separate addresses.
 *
 * THIS IS THAT LIST — one record per kind, and the only place any of it is written down. The
 * route table builds its per-kind mounts from it, the rail and the dashboard render their
 * sections from it, the breadcrumb registry registers each page under it, and the publish path
 * reads a new bundle's destination and gate off it. Adding a kind is adding a record here and
 * answering the questions the record asks; it is deliberately NOT a matter of finding every
 * `=== "mcp"` in the app.
 *
 * The bundle pages mount TWICE — once under each base — so a server is never addressed as a
 * skill, and each mount fences itself to its kind (a member who lands on the wrong one is sent
 * to the canonical path; everyone else already met the uniform 404 in the guard above).
 *
 * The mount a request arrived on is read off the ROUTE PARAM, not the route id: the MCP mount
 * names its param `server`, the skills mount `skill`. Route ids rename when a downstream build
 * re-roots these modules; a param name doesn't.
 *
 * Client-safe by construction: plain data and pure lookups, no server import. The web pages'
 * default DESTINATION is therefore spelled as a tag rather than the server's own `NO_CHANNEL`
 * symbol — the publish path resolves the tag.
 */

/** The URL base a bundle's pages live under. */
export type BundleBase = "skills" | "mcp";

/** The catalog tags this release defines. Mirrors the `bundle_kind_check` constraint. */
export type BundleKind = "skill" | "mcp";

/** Where THIS KIND'S WEB CREATION PAGE rests when the member picks no channel. */
export type WebNewDefault =
  /** The workspace's default channel. */
  | "default-channel"
  /** Nowhere: the catalog only. */
  | "no-channel";

/** Everything the product knows about one kind of bundle. */
export interface BundleKindEntry {
  /** The catalog tag stored on the bundle row. */
  kind: BundleKind;
  /** The URL base its pages live under. */
  base: BundleBase;
  /** What one of them is called in a sentence ("Assigning to everyone puts this <noun> …"). */
  noun: string;
  /** The sidebar's section label and the breadcrumb trail's first crumb. */
  sectionLabel: string;
  /** The route param the mount names, and the key a page reads its bundle name from. */
  paramName: "skill" | "server";
  /** This section's own way in, as an in-workspace path (no leading slash). */
  newPagePath: string;
  /** The module that page loads (relative to `routes/`). */
  newPageFile: string;
  /** What the link to that page is CALLED where both kinds' ways in stand together (the
   *  dashboard's header actions). Names the act, which is why it is not the section label. */
  newPageLabel: string;
  /** The one line the dashboard's section carries beside its heading — what a reader most needs
   *  to know about this kind once they can see a list of them. */
  indexNote: string;
  /**
   * WHERE THIS KIND'S WEB CREATION PAGE RESTS — the destination its form carries when the member
   * chooses no channel. Scoped to that page ON PURPOSE, and named for it: importing an MCP
   * server on a web form is not the same act as an agent publishing one, and only the form was
   * ever ruled on. The SESSION LANE keeps its own wire semantics (an absent channel means the
   * workspace default, for every kind) — a publish that reached the team before this record
   * existed still reaches the team, and this field must never be read there.
   */
  webNewDefaultDestination: WebNewDefault;
  /** Whether a candidate's BYTES are gated before custody (the MCP server-document gate). */
  hasContentGate: boolean;
  /** What the rail says when this section holds nothing yet. Both sections render when empty:
   *  the header carries the only `+`, and a section that hides until it has something in it
   *  hides the way to put the first thing in it. */
  railEmptyNote: string;
  /** The rail's `+` affordance, for assistive tech. */
  newActionLabel: string;
  /** What the rail's `+` DOES, by name — the shell maps names to affordances. A skill's is the
   *  publish dialog (the way in is your own agent, so the rail teaches the command); a server's
   *  is its page. Different acts, so this is a choice a record makes, not a shared default. */
  railNewAction: "publish-dialog" | "new-page";
  /** How a MIXED list marks this kind, or null for the kind a reader already assumes. A channel
   *  holds both, so the ones that are not plain skills say so on the row. */
  listChip: string | null;
  /** The rail row's icon, by NAME — the shell maps names to components, so no surface that
   *  reads these records (the route table included) drags an icon set in with them. */
  railIcon: string;
  /**
   * The prefix the route table gives this kind's SECOND mount of a shared page module. The
   * first mount takes React Router's file-derived id; a second mount of the same file needs an
   * explicit one, and the breadcrumb registry looks matches up by exactly these strings.
   * `null` means "this kind's mount is the one that keeps the derived id".
   */
  routeIdPrefix: string | null;
}

/**
 * The kinds, in the order every surface presents them: skills first, then MCP servers. The rail,
 * the dashboard and the route table all read their order from here, so the three can never
 * disagree about which section leads.
 */
export const BUNDLE_KINDS: readonly BundleKindEntry[] = [
  {
    kind: "skill",
    base: "skills",
    noun: "skill",
    sectionLabel: "Skills",
    paramName: "skill",
    newPagePath: "skills/import",
    newPageFile: "skill-import.tsx",
    newPageLabel: "Add from GitHub",
    indexNote: "Published skills appear here automatically.",
    webNewDefaultDestination: "default-channel",
    hasContentGate: false,
    railEmptyNote: "No skills yet.",
    newActionLabel: "Publish a skill from your agent",
    railNewAction: "publish-dialog",
    listChip: null,
    railIcon: "package",
    routeIdPrefix: null,
  },
  {
    kind: "mcp",
    base: "mcp",
    noun: "MCP server",
    sectionLabel: "MCP servers",
    paramName: "server",
    newPagePath: "mcp/new",
    newPageFile: "mcp-new.tsx",
    newPageLabel: "Add an MCP server",
    indexNote: "Delivered as a tool endpoint in each member's agent configs.",
    webNewDefaultDestination: "no-channel",
    hasContentGate: true,
    railEmptyNote: "No MCP servers yet.",
    newActionLabel: "Add an MCP server",
    railNewAction: "new-page",
    listChip: "MCP server",
    railIcon: "plug",
    routeIdPrefix: "mcp",
  },
];

/** The skills record — the fallback every "anything not `mcp`" read resolves to. */
const SKILL_ENTRY = BUNDLE_KINDS[0] as BundleKindEntry;

/**
 * The record for a catalog `kind`. ANYTHING NOT A DEFINED KIND IS A SKILL: the column carries a
 * default and older rows predate the tag, so an unknown value reads as the plain case rather
 * than crashing a page. (The catalog constraint is what keeps a genuinely unknown tag out.)
 */
export function kindEntry(kind: string | null | undefined): BundleKindEntry {
  return BUNDLE_KINDS.find((entry) => entry.kind === kind) ?? SKILL_ENTRY;
}

/** The record for a URL base. */
export function baseEntry(base: BundleBase): BundleKindEntry {
  return BUNDLE_KINDS.find((entry) => entry.base === base) ?? SKILL_ENTRY;
}

/** The catalog `kind` → the base that addresses it. Anything not `mcp` is a skill. */
export function baseForKind(kind: string | null | undefined): BundleBase {
  return kindEntry(kind).base;
}

/** The section a base belongs to — the sidebar's label and the breadcrumb trail's first crumb. */
export function sectionLabel(base: BundleBase): string {
  return baseEntry(base).sectionLabel;
}

/**
 * A mixed catalog list split into the sections it reads as — the registry's order, which is the
 * order the rail and the dashboard use. Any surface that offers BOTH kinds at once (a picker over
 * the whole catalog) groups them through this, so the two never interleave under one heading.
 *
 * An EMPTY section is dropped, not rendered empty: a workspace with no MCP servers should see a
 * plain list of skills, not a heading standing over nothing.
 */
export function groupByBase<T extends { kind: string }>(
  rows: readonly T[],
): { base: BundleBase; label: string; rows: T[] }[] {
  return BUNDLE_KINDS.map((entry) => ({
    base: entry.base,
    label: entry.sectionLabel,
    rows: rows.filter((row) => baseForKind(row.kind) === entry.base),
  })).filter((group) => group.rows.length > 0);
}

/** What one of them is called in a sentence ("Assigning to everyone puts this <noun> …"). */
export function bundleNoun(base: BundleBase): string {
  return baseEntry(base).noun;
}

/** Which mount this request came in on — decided by which kind's param the URL carries. */
export function baseOf(params: { skill?: string; server?: string }): BundleBase {
  const entry = BUNDLE_KINDS.find((k) => params[k.paramName] !== undefined);
  return (entry ?? SKILL_ENTRY).base;
}

/** The bundle NAME the URL carries, whichever mount it arrived on. */
export function bundleNameOf(params: { skill?: string; server?: string }): string {
  for (const entry of BUNDLE_KINDS) {
    const value = params[entry.paramName];
    if (value !== undefined) {
      return value;
    }
  }
  return undefined as unknown as string;
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
