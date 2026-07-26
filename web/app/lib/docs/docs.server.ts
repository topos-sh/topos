import { DOCS_NAV, DOCS_ORDER, DOCS_PAGES } from "./content.generated.server";
import type { DocsNeighbour, DocsPage, DocsPageView, DocsSidebarGroup } from "./model";

/**
 * The documentation lookups every docs route reads. Everything here is a pure read over the
 * committed generated module — no filesystem, no database, no request-time compilation — so a
 * docs page costs the same as any static asset and the running image needs no `docs/` directory.
 *
 * Path shape decides the face, the way it does everywhere else in this app: `/docs/<id>` renders
 * the page, `/docs/<id>.md` serves the same page as plain markdown for an agent that fetched the
 * URL, and `/docs/llms.txt` indexes the set. No content negotiation, no guessing.
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
        // The MARKDOWN twin, deliberately: this index exists for machines, and a non-browser
        // fetch of the page path gets the app's constant protocol card, not the documentation.
        // The `.md` routes are resource routes, so they answer with the prose itself.
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
