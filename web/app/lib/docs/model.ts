/**
 * The docs content model — the shape the generator emits and the routes read.
 *
 * The documentation lives at the repo root in `docs/` as MDX. It is compiled ONCE, by
 * `bun run gen:docs`, into a committed module (`content.generated.server.ts`) that the app
 * imports like any other module. Nothing reads the `docs/` directory at request time, so the
 * runtime image needs no documentation files and a downstream build inherits the docs by
 * compiling this app's source, unchanged.
 */

/** One entry in a page's on-page table of contents (its `h2`/`h3` outline, in document order). */
export interface DocsHeading {
  depth: 2 | 3;
  /** The heading's anchor id — stable across builds, derived from its text. */
  id: string;
  text: string;
}

/** One compiled documentation page. */
export interface DocsPage {
  /** The page's path under `/docs`, e.g. `index` or `motions/publish`. */
  id: string;
  /** Frontmatter `title` — the page's h1 and its document title. */
  title: string;
  /** Frontmatter `description` — the meta description and the page's line in the docs index. */
  description: string;
  /** Frontmatter `sidebar_label`, falling back to the title. */
  sidebarLabel: string;
  /** The rendered body: finished markup, highlighted at generate time. */
  html: string;
  /** The same page as plain markdown — what `/docs/<id>.md` serves. */
  markdown: string;
  headings: DocsHeading[];
}

/** One sidebar group, straight from `docs/nav.json`. */
export interface DocsNavGroup {
  group: string;
  /** Page ids, in the order they appear in the sidebar and in prev/next. */
  pages: string[];
}

/** One sidebar link: the label the nav shows and the path it points at. */
export interface DocsSidebarEntry {
  id: string;
  label: string;
  path: string;
}

export interface DocsSidebarGroup {
  group: string;
  pages: DocsSidebarEntry[];
}

/** A prev/next destination. */
export interface DocsNeighbour {
  title: string;
  path: string;
}

/**
 * What a docs route hands its component: the page, plus the chrome around it. Lives here rather
 * than beside the loader so the page component can type its props without importing a server
 * module.
 */
export interface DocsPageView {
  id: string;
  title: string;
  description: string;
  html: string;
  headings: DocsHeading[];
  sidebar: DocsSidebarGroup[];
  previous: DocsNeighbour | null;
  next: DocsNeighbour | null;
  /** Where this page's plain-markdown twin lives — the docs footer points an agent at it. */
  markdownPath: string;
}
