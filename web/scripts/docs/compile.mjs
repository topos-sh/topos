import { readdir, readFile } from "node:fs/promises";
import { join, posix, relative, sep } from "node:path";
import { DocsContentError, parseFrontmatter } from "./frontmatter.mjs";
import { CLI_REFERENCE_FILE, DOCS_CONTENT_ROOT, NAV_FILE } from "./paths.mjs";
import { reduceToMarkdown } from "./reduce.mjs";
import { renderPage } from "./render.mjs";
import { expandCliReference, hasCliReferenceMarker, normalizeSource } from "./source.mjs";

/**
 * COMPILE THE DOCS. One pass over the content root produces everything the app serves: each
 * page's HTML, its plain-markdown twin, its heading tree, and the sidebar the nav manifest
 * describes.
 *
 * `nav.json` is the single source of the docs tree — order, grouping, membership. It is checked
 * BOTH ways against the files on disk: a nav entry with no page, or a page missing from the nav,
 * fails the build. A sidebar that quietly disagrees with the pages is worse than no sidebar, and
 * an unreachable page is a page nobody will ever read.
 */

/** Page ids are lowercase path segments; the depth ceiling is what the `.md` routes cover. */
const PAGE_ID = /^[a-z0-9]+(?:-[a-z0-9]+)*(?:\/[a-z0-9]+(?:-[a-z0-9]+)*){0,2}$/;

/** Every `.mdx` file under `root`, as page ids (path relative to the root, without `.mdx`). */
async function discoverPages(root) {
  const found = [];
  const walk = async (dir) => {
    let entries;
    try {
      entries = await readdir(dir, { withFileTypes: true });
    } catch (error) {
      if (error.code === "ENOENT") {
        throw new DocsContentError(`the docs content root does not exist: ${root}`);
      }
      throw error;
    }
    for (const entry of entries.sort((a, b) => a.name.localeCompare(b.name))) {
      const full = join(dir, entry.name);
      if (entry.isDirectory()) {
        await walk(full);
      } else if (entry.name.endsWith(".mdx")) {
        found.push(relative(root, full).split(sep).join(posix.sep).slice(0, -".mdx".length));
      }
    }
  };
  await walk(root);
  return found;
}

/** Read and shape-check `nav.json`. */
async function readNav(root) {
  const file = join(root, NAV_FILE);
  let raw;
  try {
    raw = await readFile(file, "utf8");
  } catch (error) {
    if (error.code === "ENOENT") {
      throw new DocsContentError(
        `${NAV_FILE} is missing from ${root} — the sidebar manifest is the docs tree`,
      );
    }
    throw error;
  }
  let parsed;
  try {
    parsed = JSON.parse(raw);
  } catch (error) {
    throw new DocsContentError(`${NAV_FILE}: not valid JSON — ${error.message}`);
  }
  if (!Array.isArray(parsed) || parsed.length === 0) {
    throw new DocsContentError(`${NAV_FILE}: expected a non-empty array of { group, pages }`);
  }
  return parsed.map((entry, index) => {
    if (typeof entry?.group !== "string" || entry.group.trim() === "") {
      throw new DocsContentError(`${NAV_FILE}: entry ${index} is missing a "group" title`);
    }
    if (!Array.isArray(entry.pages) || entry.pages.length === 0) {
      throw new DocsContentError(`${NAV_FILE}: group "${entry.group}" lists no pages`);
    }
    for (const page of entry.pages) {
      if (typeof page !== "string" || !PAGE_ID.test(page)) {
        throw new DocsContentError(
          `${NAV_FILE}: "${page}" is not a page id — lowercase, hyphenated, at most three path segments, no .mdx`,
        );
      }
    }
    return { group: entry.group, pages: [...entry.pages] };
  });
}

/** The nav and the files on disk must name exactly the same set of pages. */
function assertNavMatchesFiles(nav, discovered) {
  const listed = nav.flatMap((group) => group.pages);
  const seen = new Set();
  for (const id of listed) {
    if (seen.has(id)) {
      throw new DocsContentError(`${NAV_FILE}: "${id}" is listed more than once`);
    }
    seen.add(id);
  }
  const onDisk = new Set(discovered);
  const missing = listed.filter((id) => !onDisk.has(id));
  if (missing.length > 0) {
    throw new DocsContentError(
      `${NAV_FILE} lists pages that do not exist: ${missing.map((id) => `${id}.mdx`).join(", ")}`,
    );
  }
  const orphans = discovered.filter((id) => !seen.has(id));
  if (orphans.length > 0) {
    throw new DocsContentError(
      `these pages are missing from ${NAV_FILE} (nothing links to them): ${orphans
        .map((id) => `${id}.mdx`)
        .join(", ")}`,
    );
  }
  if (!seen.has("index")) {
    throw new DocsContentError(`${NAV_FILE}: no "index" page — /docs would have nothing to serve`);
  }
}

/** Compile one page: frontmatter, the CLI expansion, the HTML, and the markdown twin. */
async function compilePage(root, id, cliReference) {
  const file = `${id}.mdx`;
  const source = await readFile(`${join(root, ...id.split("/"))}.mdx`, "utf8");
  const meta = parseFrontmatter(source, file);

  let body = meta.body;
  if (hasCliReferenceMarker(body)) {
    if (cliReference === null) {
      throw new DocsContentError(
        `${file}: expands the CLI reference, but ${CLI_REFERENCE_FILE} is not readable — run \`cargo xtask gen-cli-ref\``,
      );
    }
    body = expandCliReference(body, cliReference);
  }
  body = normalizeSource(body, file);

  const { html, headings } = await renderPage(body, file);
  return {
    id,
    title: meta.title,
    description: meta.description,
    sidebarLabel: meta.sidebarLabel ?? meta.title,
    html,
    markdown: reduceToMarkdown(body, meta, file),
    headings,
  };
}

/**
 * Compile the whole content set. `contentRoot` defaults to the repo's `docs/`; tests point it at
 * a fixture tree. The result is deterministic — pages follow nav order, nothing carries a
 * timestamp or an absolute path — so the committed module only changes when the content does.
 */
export async function compileDocs({ contentRoot = DOCS_CONTENT_ROOT } = {}) {
  const nav = await readNav(contentRoot);
  const discovered = await discoverPages(contentRoot);
  assertNavMatchesFiles(nav, discovered);

  const cliReference = await readFile(CLI_REFERENCE_FILE, "utf8").catch(() => null);
  const order = nav.flatMap((group) => group.pages);
  const pages = [];
  for (const id of order) {
    pages.push(await compilePage(contentRoot, id, cliReference));
  }
  return { nav, order, pages };
}
