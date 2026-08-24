import { matchRoutes, type RouteObject } from "react-router";
import { describe, expect, it } from "vitest";
import {
  DOCS_BASE,
  docsLlmsTxt,
  docsMarkdown,
  docsMarkdownPath,
  docsNegotiatedMarkdown,
  docsNeighbours,
  docsPageFor,
  docsPath,
  docsSidebar,
} from "@/lib/docs/docs.server";
import { ossRoutes } from "@/topos-web/routes";

/**
 * The docs SURFACES: the three faces a docs path can have (page, markdown twin, index) and the
 * route table that decides between them by PATH SHAPE.
 *
 * These run against the COMMITTED content module — the same bytes a deployment serves — so they
 * assert structure and mechanism, never a sentence of documentation copy (which moves for reasons
 * that have nothing to do with this renderer).
 */

/** The docs entries of the real route table, as a matchable tree. */
function docsRouteTable(tenancy: "single" | "multi"): RouteObject[] {
  const flatten = (entries: ReturnType<typeof ossRoutes>): RouteObject[] =>
    entries.map((entry) => ({
      id: entry.id ?? entry.file,
      path: entry.path,
      index: entry.index,
      children: entry.children ? flatten(entry.children) : undefined,
    })) as RouteObject[];
  return flatten(ossRoutes({ tenancy }));
}

function matchId(path: string, tenancy: "single" | "multi" = "multi"): string | undefined {
  const matches = matchRoutes(docsRouteTable(tenancy), path);
  return matches?.[matches.length - 1]?.route.id;
}

describe("the docs route table", () => {
  it("mounts the docs origin-rooted in BOTH tenancy modes", () => {
    for (const tenancy of ["single", "multi"] as const) {
      expect(matchId("/docs", tenancy)).toBe("docs-index");
      expect(matchId("/docs/quickstart", tenancy)).toBe("docs-page");
    }
  });

  it("routes a page, its markdown twin, and the index by path SHAPE alone", () => {
    expect(matchId("/docs")).toBe("docs-index");
    expect(matchId("/docs.md")).toBe("docs-index-md");
    expect(matchId("/docs/quickstart")).toBe("docs-page");
    expect(matchId("/docs/quickstart.md")).toBe("docs-md-1");
    expect(matchId("/docs/motions/publish")).toBe("docs-page");
    expect(matchId("/docs/motions/publish.md")).toBe("docs-md-2");
    expect(matchId("/docs/a/b/c.md")).toBe("docs-md-3");
    // The llms.txt index takes no explicit id — one route, one module, so the file path is it.
    expect(matchId("/docs/llms.txt")).toBe("routes/docs-llms-txt.ts");
  });

  it("keeps the static docs segment ahead of the :ws workspace face in multi tenancy", () => {
    // `/docs` must never resolve as a workspace slug — that is what reserving the segment buys.
    expect(matchId("/docs", "multi")).toBe("docs-index");
    expect(matchId("/somebody-else", "multi")).not.toBe("docs-index");
  });
});

describe("page lookup", () => {
  it("treats the empty splat as the index page", () => {
    expect(docsPageFor(undefined)?.id).toBe("index");
    expect(docsPageFor("")?.id).toBe("index");
  });

  it("answers null for a path that names nothing — the caller renders the house 404", () => {
    expect(docsPageFor("nope")).toBeNull();
    expect(docsPageFor("deep/nope")).toBeNull();
    expect(docsMarkdown("nope")).toBeNull();
  });

  it("puts the index at the docs root, and every other page under it", () => {
    expect(docsPath("index")).toBe(DOCS_BASE);
    expect(docsPath("quickstart")).toBe("/docs/quickstart");
    expect(docsMarkdownPath("index")).toBe("/docs.md");
    expect(docsMarkdownPath("quickstart")).toBe("/docs/quickstart.md");
  });
});

describe("the sidebar and prev/next", () => {
  it("builds the sidebar from the nav, every entry pointing at a real page", () => {
    const sidebar = docsSidebar();
    expect(sidebar.length).toBeGreaterThan(0);
    for (const group of sidebar) {
      expect(group.group).not.toBe("");
      for (const page of group.pages) {
        expect(docsPageFor(page.id)).not.toBeNull();
        expect(page.path).toBe(docsPath(page.id));
        expect(page.label).not.toBe("");
      }
    }
  });

  it("walks reading order: the first page has no previous, the last no next", () => {
    const order = docsSidebar().flatMap((group) => group.pages.map((page) => page.id));
    const first = order[0] as string;
    const last = order[order.length - 1] as string;
    expect(docsNeighbours(first).previous).toBeNull();
    expect(docsNeighbours(first).next?.path).toBe(docsPath(order[1] as string));
    expect(docsNeighbours(last).next).toBeNull();
  });

  it("has no neighbours for a page that is not in the nav", () => {
    expect(docsNeighbours("nope")).toEqual({ previous: null, next: null });
  });
});

describe("/docs/llms.txt", () => {
  const index = docsLlmsTxt("https://topos.example.com");

  it("links every page's MARKDOWN twin, ABSOLUTE, on the deployment's own origin", () => {
    // This index is read by machines, and the `.md` path says what it serves in the URL — one
    // address whose answer never depends on a header.
    for (const group of docsSidebar()) {
      for (const page of group.pages) {
        expect(index).toContain(`](https://topos.example.com${docsMarkdownPath(page.id)})`);
        expect(index).not.toContain(`](https://topos.example.com${page.path})`);
      }
    }
  });

  it("carries each page's title and one-line description", () => {
    for (const group of docsSidebar()) {
      for (const entry of group.pages) {
        const page = docsPageFor(entry.id);
        expect(index).toContain(`[${page?.title}]`);
        expect(index).toContain(`: ${page?.description}`);
      }
    }
  });

  it("groups the index the way the sidebar groups it", () => {
    for (const group of docsSidebar()) {
      expect(index).toContain(`## ${group.group}`);
    }
  });

  it("tells a reader how to get any page as markdown", () => {
    expect(index).toContain(".md");
  });

  it("normalizes a trailing slash on the origin rather than doubling it", () => {
    expect(docsLlmsTxt("https://topos.example.com/")).toBe(index);
  });
});

describe("the markdown twin", () => {
  it("serves every page in the nav", () => {
    for (const group of docsSidebar()) {
      for (const page of group.pages) {
        expect(docsMarkdown(page.id)).toMatch(/^# /);
      }
    }
  });

  it("hands an agent prose, not JSX", () => {
    for (const group of docsSidebar()) {
      for (const page of group.pages) {
        const markdown = docsMarkdown(page.id) ?? "";
        expect(markdown).not.toMatch(/<\/?(Note|Warning|Tip|Steps|Step|Tabs|Tab|Card|CardGrid)/);
        expect(markdown).not.toContain("GENERATED-CLI-REFERENCE");
      }
    }
  });
});

describe("a non-browser fetch of a docs PAGE path", () => {
  const fetchDocs = (path: string, accept?: string): Response | null =>
    docsNegotiatedMarkdown(
      new Request(`https://topos.example.com${path}`, {
        headers: accept === undefined ? {} : { accept },
      }),
    );

  it("serves the page's markdown to curl's bare Accept (the D14 soft-card fix)", async () => {
    const response = fetchDocs("/docs/quickstart", "*/*");
    expect(response).not.toBeNull();
    expect(response?.headers.get("content-type")).toBe("text/markdown; charset=utf-8");
    expect(await (response as Response).text()).toBe(docsMarkdown("quickstart"));
  });

  it("serves the same bytes as the page's own .md twin, for every page in the nav", async () => {
    for (const group of docsSidebar()) {
      for (const page of group.pages) {
        const response = fetchDocs(page.path, "*/*");
        expect(response, page.path).not.toBeNull();
        expect(await (response as Response).text()).toBe(docsMarkdown(page.id));
      }
    }
  });

  it("answers the docs ROOT too, with or without the trailing slash", async () => {
    for (const path of [DOCS_BASE, `${DOCS_BASE}/`]) {
      const response = fetchDocs(path, "*/*");
      expect(response, path).not.toBeNull();
      expect(await (response as Response).text()).toBe(docsMarkdown("index"));
    }
  });

  it("varies on accept — the same URL now has two bodies", () => {
    expect(fetchDocs("/docs/quickstart", "*/*")?.headers.get("vary")).toBe("accept");
  });

  it("leaves a BROWSER alone — the page renders exactly as before", () => {
    expect(fetchDocs("/docs/quickstart", "text/html")).toBeNull();
    expect(fetchDocs("/docs/quickstart", "text/html,application/xhtml+xml,*/*;q=0.8")).toBeNull();
  });

  it("leaves everything that is not a docs page alone — the card still answers those", () => {
    for (const path of ["/", "/northwind", "/northwind/skills/x", "/docsomething", "/login"]) {
      expect(fetchDocs(path, "*/*"), path).toBeNull();
    }
  });

  it("leaves a docs path that names NO page alone — the miss keeps its own answer", () => {
    expect(fetchDocs("/docs/no-such-page", "*/*")).toBeNull();
  });

  it("never intercepts the .md twins or the llms.txt index — those are their own routes", () => {
    expect(fetchDocs("/docs/quickstart.md", "*/*")).toBeNull();
    expect(fetchDocs("/docs/llms.txt", "*/*")).toBeNull();
  });
});
