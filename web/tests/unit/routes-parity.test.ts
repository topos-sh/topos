import type { RouteConfigEntry } from "@react-router/dev/routes";
import { matchRoutes, type RouteObject } from "react-router";
import { describe, expect, it } from "vitest";
import { ossRoutes } from "@/topos-web/routes";

/**
 * ROUTE PARITY — the table's addressable surface, pinned.
 *
 * The bundle pages mount once per kind, and those mounts are BUILT rather than typed out one by
 * one. A generator is only as good as the paths it reproduces: every URL that resolved before
 * must resolve to the same module afterwards, or a bookmark, an invitation's first destination
 * and a channel row all break at once. So this file pins two things that a refactor of the
 * table cannot quietly change:
 *
 *  1. the FULL flattened surface — every path the table registers, in both tenancy grammars,
 *     with the module each one loads (the snapshots below were taken from the hand-written
 *     table and are the parity baseline, not a description of the generator's output), and
 *  2. a concrete URL → module list driven through React Router's own `matchRoutes`, so the
 *     ranking a real request goes through is what is asserted, not the shape of the config.
 *
 * Paths and FILES are the contract here, never route ids: a downstream superset build re-roots
 * these modules under its own directory, which renames every generated id. The one exception is
 * the ids the table sets EXPLICITLY — those are chrome keys (the breadcrumb registry looks a
 * match up by id), so they are pinned by the snapshot too, spelled as given.
 */

/** One registered route, flattened: the path a request matches, and what answers it. */
interface FlatRoute {
  /** Absolute path as registered (layout/index nodes contribute their parent's). */
  path: string;
  /** The module file, with the `routes/` prefix stripped — the stable half of the pair. */
  file: string;
  /** The EXPLICIT id the table gave this mount, when it gave one. */
  id?: string;
}

/**
 * Walk the config into the flat list of addressable entries. Index routes (no path) are
 * reported at their parent's path with a trailing marker so an index and a bare layout can
 * never collapse into one line; layouts contribute nothing themselves.
 */
function flatten(entries: RouteConfigEntry[], parent = ""): FlatRoute[] {
  const out: FlatRoute[] = [];
  for (const entry of entries) {
    const hasPath = typeof entry.path === "string" && entry.path.length > 0;
    const here = hasPath ? `${parent}/${entry.path as string}`.replace(/\/+/g, "/") : parent;
    const isLayoutOnly = !hasPath && entry.children !== undefined;
    if (!isLayoutOnly) {
      out.push({
        path: hasPath ? here : `${here === "" ? "/" : here} (index)`,
        file: entry.file.replace(/^routes\//, ""),
        ...(entry.id === undefined ? {} : { id: entry.id }),
      });
    }
    if (entry.children !== undefined) {
      out.push(...flatten(entry.children, here));
    }
  }
  return out;
}

/** The flattened surface as one sorted, greppable line per route. */
function surface(tenancy: "single" | "multi"): string[] {
  return flatten(ossRoutes({ tenancy }))
    .map((r) => `${r.path}  →  ${r.file}${r.id === undefined ? "" : `  [id=${r.id}]`}`)
    .sort();
}

/**
 * The config as React Router route objects, so `matchRoutes` ranks them the way a request does.
 * Each object gets a UNIQUE synthetic id (a mount may share its file with another mount, and
 * React Router refuses duplicate ids), and `files` records what each id actually loads — the
 * answer the assertions want is the module, never the id.
 */
function routeObjects(
  entries: RouteConfigEntry[],
  files: Map<string, string>,
  trail = "",
): RouteObject[] {
  return entries.map((entry, index) => {
    const id = `${trail}${index}`;
    files.set(id, entry.file.replace(/^routes\//, ""));
    const children =
      entry.children === undefined ? undefined : routeObjects(entry.children, files, `${id}.`);
    const base = { id, ...(children === undefined ? {} : { children }) };
    return (
      typeof entry.path === "string" && entry.path.length > 0
        ? { ...base, path: entry.path }
        : { ...base, index: children === undefined ? true : undefined }
    ) as RouteObject;
  });
}

/** Which MODULE answers `url` — the leaf match's file, the thing a reader actually lands on. */
function moduleFor(url: string, tenancy: "single" | "multi"): string | undefined {
  const files = new Map<string, string>();
  const matches = matchRoutes(routeObjects(ossRoutes({ tenancy }), files), url);
  const leaf = matches?.[matches.length - 1]?.route.id;
  return leaf === undefined ? undefined : files.get(leaf);
}

describe("the route table's addressable surface", () => {
  it("registers exactly these paths in SINGLE tenancy", () => {
    expect(surface("single")).toMatchSnapshot();
  });

  it("registers exactly these paths in MULTI tenancy", () => {
    expect(surface("multi")).toMatchSnapshot();
  });
});

/**
 * The bundle pages under BOTH bases. Each pair is the same module reached two ways — the skills
 * base and the MCP base — which is exactly what the per-kind mounts exist to provide.
 */
const BUNDLE_URLS: [skills: string, mcp: string, module: string][] = [
  ["/skills/alpha", "/mcp/alpha", "skill-current.tsx"],
  ["/skills/alpha/history", "/mcp/alpha/history", "skill-history.tsx"],
  ["/skills/alpha/proposals", "/mcp/alpha/proposals", "skill-proposals.tsx"],
  ["/skills/alpha/proposals/v1", "/mcp/alpha/proposals/v1", "proposal-review.tsx"],
  ["/skills/alpha/settings", "/mcp/alpha/settings", "skill-settings.tsx"],
  ["/skills/alpha/versions/v1", "/mcp/alpha/versions/v1", "version-files.tsx"],
  [
    "/skills/alpha/versions/v1/files/a/b.md",
    "/mcp/alpha/versions/v1/files/a/b.md",
    "file-view.tsx",
  ],
];

describe("every bundle URL resolves to its page module, under both bases", () => {
  for (const [skillUrl, mcpUrl, module] of BUNDLE_URLS) {
    it(`single: ${skillUrl} and ${mcpUrl} → ${module}`, () => {
      expect(moduleFor(skillUrl, "single")).toBe(module);
      expect(moduleFor(mcpUrl, "single")).toBe(module);
    });
    it(`multi: /acme${skillUrl} and /acme${mcpUrl} → ${module}`, () => {
      expect(moduleFor(`/acme${skillUrl}`, "multi")).toBe(module);
      expect(moduleFor(`/acme${mcpUrl}`, "multi")).toBe(module);
    });
  }
});

describe("the static ways in still outrank the bundle name they look like", () => {
  // `skills/import` and `mcp/new` are catalog PAGES, not bundles called `import`/`new`. They are
  // registered from the same per-kind record that builds the mounts, so the ranking that keeps
  // them reachable is worth pinning: a generator that emitted `:skill` first would swallow them.
  it("single", () => {
    expect(moduleFor("/skills/import", "single")).toBe("skill-import.tsx");
    expect(moduleFor("/mcp/new", "single")).toBe("mcp-new.tsx");
  });
  it("multi", () => {
    expect(moduleFor("/acme/skills/import", "multi")).toBe("skill-import.tsx");
    expect(moduleFor("/acme/mcp/new", "multi")).toBe("mcp-new.tsx");
  });
});
