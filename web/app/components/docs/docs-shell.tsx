import type { MouseEvent } from "react";
import { Link, NavLink } from "react-router";
import type { DocsPageView } from "@/lib/docs/model";

/**
 * The documentation page, in the Klein system (see ../../../DESIGN.md): the print ground, ink
 * running text, Martian Mono for the wordmark, headings and micro-labels, and Klein blue placed
 * as an object — the sidebar's active marker, the anchor affordances, the one nav button. No new
 * palette, no second visual language: the same tokens the rest of the app is built from.
 *
 * Three columns on a wide screen — the nav.json sidebar, the article, the on-page contents — and
 * one column on a narrow one, where the sidebar leads and the contents list is dropped (the
 * article's own headings carry the reader instead).
 *
 * The article body is markup this repo GENERATED at build time from its own MDX (see
 * app/lib/docs/model.ts): no raw HTML can reach it — remark-rehype runs without
 * `allowDangerousHtml` and the component set is a closed list — and shiki has already coloured
 * every code block, so the browser downloads no highlighter.
 */

const WRAP = "mx-auto max-w-[1180px] px-6";
const GITHUB = "https://github.com/topos-sh/topos";

/**
 * The ONE bit of interactivity a docs page carries: the generator placed a copy button beside
 * every code block, and this delegated handler serves them all. The payload is read from the
 * block itself — the highlighted `<pre>`'s `innerText` IS the author's code — so the markup
 * carries no duplicate copy of it.
 */
function copyFromCodeBlock(event: MouseEvent<HTMLElement>) {
  const button = (event.target as HTMLElement).closest<HTMLButtonElement>("button[data-copy]");
  if (button === null) {
    return;
  }
  const pre = button.closest(".docs-codeblock")?.querySelector("pre");
  if (!pre) {
    return;
  }
  void navigator.clipboard.writeText(pre.innerText.replace(/\n$/, "")).then(() => {
    button.setAttribute("data-copied", "");
    window.setTimeout(() => button.removeAttribute("data-copied"), 1600);
  });
}

function DocsNav() {
  return (
    <nav className="sticky top-0 z-20 border-line-soft border-b bg-ground/95 backdrop-blur">
      <div className={`${WRAP} flex h-[60px] items-center justify-between`}>
        <div className="flex items-baseline gap-3">
          <Link
            to="/"
            className="font-display font-semibold text-[15px] tracking-[-0.02em] focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
          >
            topos<span className="text-accent">_</span>
          </Link>
          <Link
            to="/docs"
            className="font-display text-[10px] text-faint uppercase tracking-[0.12em]"
          >
            Docs
          </Link>
        </div>
        <div className="flex items-center gap-6 text-[13.5px] text-dim">
          <a
            href={GITHUB}
            target="_blank"
            rel="noreferrer"
            className="transition-colors hover:text-ink max-sm:hidden"
          >
            GitHub
            <span className="sr-only"> (opens in a new tab)</span>
          </a>
          <Link
            to="/app"
            className="rounded-md bg-accent px-3.5 py-2 font-mono text-[12.5px] text-on-accent transition-colors hover:bg-accent-deep focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 active:scale-[0.98]"
          >
            Sign in
          </Link>
        </div>
      </div>
    </nav>
  );
}

function DocsFooter({ markdownPath }: { markdownPath: string }) {
  return (
    <footer className="mt-20 border-line-soft border-t py-10">
      <div className={`${WRAP} flex flex-wrap items-center justify-between gap-4 text-[13px]`}>
        <p className="text-faint">Apache-2.0 {"·"} © 2026 Topos</p>
        <div className="flex flex-wrap gap-6 text-faint">
          {/* The same page an agent should fetch — stated on the page, not hidden in a header. */}
          <a href={markdownPath} className="transition-colors hover:text-ink">
            This page as markdown
          </a>
          <a href="/docs/llms.txt" className="transition-colors hover:text-ink">
            llms.txt
          </a>
          <a
            href={GITHUB}
            target="_blank"
            rel="noreferrer"
            className="transition-colors hover:text-ink"
          >
            GitHub
            <span className="sr-only"> (opens in a new tab)</span>
          </a>
        </div>
      </div>
    </footer>
  );
}

function Sidebar({ sidebar }: { sidebar: DocsPageView["sidebar"] }) {
  return (
    <nav aria-label="Documentation" className="lg:sticky lg:top-[76px] lg:self-start">
      <ul className="space-y-6">
        {sidebar.map((group) => (
          <li key={group.group}>
            <p className="font-display text-[10px] text-faint uppercase tracking-[0.12em]">
              {group.group}
            </p>
            <ul className="mt-2 space-y-0.5">
              {group.pages.map((page) => (
                <li key={page.id}>
                  <NavLink
                    to={page.path}
                    end
                    className={({ isActive }) =>
                      `block rounded-md border-l-2 py-1 pl-3 text-[13.5px] transition-colors ${
                        isActive
                          ? "border-accent bg-accent-wash font-medium text-ink"
                          : "border-transparent text-dim hover:border-line hover:text-ink"
                      }`
                    }
                  >
                    {page.label}
                  </NavLink>
                </li>
              ))}
            </ul>
          </li>
        ))}
      </ul>
    </nav>
  );
}

function TableOfContents({ headings }: { headings: DocsPageView["headings"] }) {
  if (headings.length === 0) {
    return null;
  }
  return (
    // Sticky sits on the GRID ITEM itself: a sticky element's containing block is its grid area
    // (the full row), so a wrapper div around it would pin the travel to its own short height.
    <nav aria-label="On this page" className="sticky top-[76px] self-start max-xl:hidden">
      <p className="font-display text-[10px] text-faint uppercase tracking-[0.12em]">
        On this page
      </p>
      <ul className="mt-2 space-y-1 border-line-soft border-l">
        {headings.map((heading) => (
          <li key={heading.id}>
            <a
              href={`#${heading.id}`}
              className={`block py-0.5 text-[13px] text-dim transition-colors hover:text-ink ${
                heading.depth === 3 ? "pl-7" : "pl-3"
              }`}
            >
              {heading.text}
            </a>
          </li>
        ))}
      </ul>
    </nav>
  );
}

function Neighbours({ previous, next }: Pick<DocsPageView, "previous" | "next">) {
  if (previous === null && next === null) {
    return null;
  }
  return (
    <nav
      aria-label="Previous and next page"
      className="mt-12 flex flex-wrap justify-between gap-3 border-line-soft border-t pt-6"
    >
      {previous === null ? (
        <span />
      ) : (
        <Link
          to={previous.path}
          className="rounded-md border border-line px-4 py-3 text-left transition-colors hover:bg-panel2"
        >
          <span className="block font-display text-[10px] text-faint uppercase tracking-[0.12em]">
            Previous
          </span>
          <span className="mt-1 block text-[13.5px] text-ink">{previous.title}</span>
        </Link>
      )}
      {next !== null && (
        <Link
          to={next.path}
          className="rounded-md border border-line px-4 py-3 text-right transition-colors hover:bg-panel2"
        >
          <span className="block font-display text-[10px] text-faint uppercase tracking-[0.12em]">
            Next
          </span>
          <span className="mt-1 block text-[13.5px] text-ink">{next.title}</span>
        </Link>
      )}
    </nav>
  );
}

export function DocsShell({ page }: { page: DocsPageView }) {
  return (
    <div className="min-h-dvh text-[15px] leading-[1.6]">
      <DocsNav />
      <div
        className={`${WRAP} grid items-start gap-10 py-10 lg:grid-cols-[200px_minmax(0,1fr)] xl:grid-cols-[200px_minmax(0,1fr)_190px]`}
      >
        <Sidebar sidebar={page.sidebar} />
        <main className="min-w-0">
          <header className="border-line-soft border-b pb-5">
            <h1 className="font-display font-semibold text-[clamp(19px,2.4vw,25px)] text-ink leading-[1.45] tracking-[-0.03em]">
              {page.title}
            </h1>
            <p className="mt-2 text-[15px] text-dim">{page.description}</p>
          </header>
          {/* biome-ignore lint/a11y/useKeyWithClickEvents: pure event delegation — the real controls are the generated <button data-copy> elements, and their keyboard activation dispatches a native click this same handler receives. */}
          <article
            className="docs-prose mt-8"
            onClick={copyFromCodeBlock}
            // biome-ignore lint/security/noDangerouslySetInnerHtml: this repo's own MDX, compiled to markup at build time by scripts/gen-docs.mjs — the pipeline admits no raw HTML from source (remark-rehype runs without allowDangerousHtml) and the component set is a closed list.
            dangerouslySetInnerHTML={{ __html: page.html }}
          />
          <Neighbours previous={page.previous} next={page.next} />
        </main>
        <TableOfContents headings={page.headings} />
      </div>
      <DocsFooter markdownPath={page.markdownPath} />
    </div>
  );
}
