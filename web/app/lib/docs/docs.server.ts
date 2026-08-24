import { cardFace } from "@/lib/card.server";
import { DOCS_NAV, DOCS_ORDER, DOCS_PAGES } from "./content.generated.server";
import type { DocsNeighbour, DocsPage, DocsPageView, DocsSidebarGroup } from "./model";

/**
 * The documentation lookups every docs route reads. Everything here is a pure read over the
 * committed generated module — no filesystem, no database, no request-time compilation — so a
 * docs page costs the same as any static asset and the running image needs no `docs/` directory.
 *
 * A docs page has ONE body in two dresses, and BOTH ways of asking for the plain one work:
 * `/docs/<id>.md` is the twin an agent can link to and paste around, and a fetch of the PAGE
 * path itself with any non-HTML `Accept` gets the same markdown (`docsNegotiatedMarkdown`) —
 * `curl https://topos.sh/docs/quickstart` reads the documentation, not the app's protocol card.
 * A browser (an `Accept` naming `text/html`) is untouched and renders the page.
 */

/** The docs mount — origin-rooted in BOTH tenancy modes (it describes the deployment). */
export const DOCS_BASE = "/docs";

/** The page `/docs` itself renders. */
export const DOCS_INDEX_ID = "index";

/** The browser path of a page id — the index lives at the docs root, not at `/docs/index`. */
export function docsPath(id: string): string {
  return id === DOCS_INDEX_ID ? DOCS_BASE : `${DOCS_BASE}/${id}`;
}

/** The plain-markdown twin of a page: its own path plus `.md`. */
export function docsMarkdownPath(id: string): string {
  return `${docsPath(id)}.md`;
}

/** The page a `/docs/*` splat names, or null when nothing does. An empty splat is the index. */
export function docsPageFor(splat: string | undefined): DocsPage | null {
  const id = splat === undefined || splat === "" ? DOCS_INDEX_ID : splat.replace(/\/+$/, "");
  return DOCS_PAGES[id] ?? null;
}

/** The sidebar, group by group, in nav order. */
export function docsSidebar(): DocsSidebarGroup[] {
  return DOCS_NAV.map((group) => ({
    group: group.group,
    pages: group.pages.map((id) => ({
      id,
      label: DOCS_PAGES[id]?.sidebarLabel ?? id,
      path: docsPath(id),
    })),
  }));
}

/** The pages either side of `id` in reading order — what prev/next link to. */
export function docsNeighbours(id: string): {
  previous: DocsNeighbour | null;
  next: DocsNeighbour | null;
} {
  const at = DOCS_ORDER.indexOf(id);
  const neighbour = (index: number): DocsNeighbour | null => {
    const other = DOCS_ORDER[index];
    const page = other === undefined ? undefined : DOCS_PAGES[other];
    return page === undefined ? null : { title: page.title, path: docsPath(page.id) };
  };
  return at === -1
    ? { previous: null, next: null }
    : { previous: neighbour(at - 1), next: neighbour(at + 1) };
}

/** Assemble the view one docs page renders from. */
export function docsPageView(page: DocsPage): DocsPageView {
  const { previous, next } = docsNeighbours(page.id);
  return {
    id: page.id,
    title: page.title,
    description: page.description,
    html: page.html,
    headings: page.headings,
    sidebar: docsSidebar(),
    previous,
    next,
    markdownPath: docsMarkdownPath(page.id),
  };
}

/**
 * `/docs/llms.txt` — the docs index in the llms.txt convention: one line per page, title,
 * one-sentence description, absolute URL. `origin` comes from the request, so a self-hosted
 * install indexes its own address rather than someone else's.
 */
export function docsLlmsTxt(origin: string): string {
  const base = origin.replace(/\/+$/, "");
  const lines = [
    "# Topos documentation",
    "",
    "> Topos keeps a team's agent skills — bundles of instructions, scripts, and reference docs —",
    "> current on every machine: publish once, every subscribed agent picks the update up at its",
    "> next session start.",
    "",
    "Every link below is the page's plain-markdown twin. Drop the `.md` for the rendered page.",
    "",
  ];
  for (const group of DOCS_NAV) {
    lines.push(`## ${group.group}`, "");
    for (const id of group.pages) {
      const page = DOCS_PAGES[id];
      if (page !== undefined) {
        // The MARKDOWN twin, deliberately: this index exists for machines, and the `.md` path
        // says what it serves in the URL — one address whose answer never depends on a header.
        lines.push(`- [${page.title}](${base}${docsMarkdownPath(id)}): ${page.description}`);
      }
    }
    lines.push("");
  }
  return `${lines.join("\n").trimEnd()}\n`;
}

/** The plain-markdown body of a page, or null when the id names nothing. */
export function docsMarkdown(id: string): string | null {
  return DOCS_PAGES[id]?.markdown ?? null;
}

/** The headers a markdown docs body is served with; `vary` is added where Accept chose it. */
const MARKDOWN_HEADERS = {
  "content-type": "text/markdown; charset=utf-8",
  "cache-control": "public, max-age=300",
} as const;

/** One page's markdown as a response — what `/docs/<id>.md` answers with. */
export function docsMarkdownResponse(markdown: string): Response {
  return new Response(markdown, { headers: MARKDOWN_HEADERS });
}

/**
 * The page a docs PAGE path names (`/docs`, `/docs/`, `/docs/<id>`, `/docs/<a>/<b>`), or null
 * when the path is not one — including the `.md` twins and `/docs/llms.txt`, which are their own
 * resource routes and never come through here.
 */
function docsPageAtPath(pathname: string): DocsPage | null {
  if (pathname !== DOCS_BASE && !pathname.startsWith(`${DOCS_BASE}/`)) {
    return null;
  }
  return docsPageFor(pathname === DOCS_BASE ? "" : pathname.slice(`${DOCS_BASE}/`.length));
}

/**
 * The markdown answer for a NON-BROWSER fetch of a docs page path, or null to leave the request
 * alone.
 *
 * `curl` sends a bare wildcard Accept, so before this the documentation answered a terminal with
 * the app's protocol card — the right answer for a workspace address, the wrong one for a public
 * page whose whole point is being readable. A path that names no page is left alone too, so the
 * miss keeps whatever answer it already had.
 */
export function docsNegotiatedMarkdown(request: Request): Response | null {
  if (cardFace(request) === "html") {
    return null;
  }
  const page = docsPageAtPath(new URL(request.url).pathname);
  if (page === null) {
    return null;
  }
  const response = docsMarkdownResponse(page.markdown);
  // The same URL now has two bodies, chosen by Accept — caches must key on it.
  response.headers.set("vary", "accept");
  return response;
}
