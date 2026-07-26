import rehypeShikiFromHighlighter from "@shikijs/rehype/core";
import rehypeStringify from "rehype-stringify";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import remarkRehype from "remark-rehype";
import { createHighlighter } from "shiki";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";
import { unified } from "unified";
import { remarkDocsComponents } from "./components.mjs";
import { DocsContentError } from "./frontmatter.mjs";

/**
 * The docs render pipeline — the app's own remark/rehype chain, with two docs-specific plugins:
 *
 *   remark-parse → remark-gfm → components → code titles
 *     → remark-rehype → headings/anchors → shiki → rehype-stringify
 *
 * remark-rehype runs WITHOUT `allowDangerousHtml`, and the component plugin ahead of it has
 * already turned every tag it recognises into structure and every stray one into literal text.
 * So raw HTML cannot reach the output at all — a stronger guarantee than sanitizing after the
 * fact, and the reason this renderer needs no sanitizer: there is no path from source bytes to
 * live markup except the component set and shiki's own escaped spans.
 *
 * Highlighting happens HERE, at generate time, exactly once per code block. The browser receives
 * finished markup and downloads no highlighter.
 */

export const HIGHLIGHT_THEME = "github-light";

/**
 * The fence languages the docs may use. Unknown or absent languages fall back to plaintext
 * rather than failing a build — a fence still renders, just without colour.
 */
const DOCS_LANGUAGES = [
  "bash",
  "console",
  "diff",
  "dockerfile",
  "html",
  "ini",
  "javascript",
  "json",
  "jsonc",
  "markdown",
  "python",
  "rust",
  "shellscript",
  "sql",
  "toml",
  "tsx",
  "typescript",
  "xml",
  "yaml",
];

let highlighterPromise;

/** One highlighter for the whole generate run: the grammars load once, not per page. */
export function docsHighlighter() {
  highlighterPromise ??= createHighlighter({
    themes: [HIGHLIGHT_THEME],
    langs: DOCS_LANGUAGES,
    // The JavaScript RegExp engine, not oniguruma: the generator then loads no WebAssembly.
    engine: createJavaScriptRegexEngine({ forgiving: true }),
  });
  return highlighterPromise;
}

/** Depth-first walk over any unist tree. `visit` may replace a node by returning one. */
function walk(node, visit) {
  const children = node.children;
  if (!Array.isArray(children)) {
    return;
  }
  for (let index = 0; index < children.length; index += 1) {
    const replacement = visit(children[index], node, index);
    const current = replacement ?? children[index];
    if (replacement !== undefined) {
      children[index] = replacement;
    }
    walk(current, visit);
  }
}

/** A fenced block's `title="…"` meta becomes a captioned frame around the highlighted code. */
function remarkCodeTitles() {
  return (tree) => {
    walk(tree, (node) => {
      if (node.type !== "code" || typeof node.meta !== "string") {
        return undefined;
      }
      const match = node.meta.match(/title="([^"]*)"/);
      if (match === null || match[1] === "") {
        return undefined;
      }
      return {
        type: "docsElement",
        data: { hName: "figure", hProperties: { className: ["docs-code"] } },
        children: [
          {
            type: "docsElement",
            data: { hName: "figcaption", hProperties: { className: ["docs-code__title"] } },
            children: [{ type: "text", value: match[1] }],
          },
          { ...node, meta: null },
        ],
      };
    });
  };
}

const HEADING_LEVELS = { h2: 2, h3: 3 };

function textOf(node) {
  if (node.type === "text") {
    return node.value;
  }
  return (node.children ?? []).map(textOf).join("");
}

function slugify(text) {
  return (
    text
      .toLowerCase()
      .replace(/[^\p{Letter}\p{Number}]+/gu, "-")
      .replace(/^-+|-+$/g, "") || "section"
  );
}

/**
 * Give every h2/h3 a stable id and a self-link, and collect them in document order as the page's
 * table of contents. An h1 in the body is refused: the frontmatter title is the page's one h1,
 * so a second one would break both the outline and the document heading order.
 */
function rehypeHeadings(options) {
  const { file, headings } = options;
  const used = new Map();
  return (tree) => {
    walk(tree, (node) => {
      if (node.type !== "element") {
        return undefined;
      }
      if (node.tagName === "h1") {
        throw new DocsContentError(
          `${file}: the body carries an h1 — the frontmatter title is the page's only h1, so start body sections at "##"`,
        );
      }
      const depth = HEADING_LEVELS[node.tagName];
      if (depth === undefined) {
        return undefined;
      }
      const text = textOf(node).trim();
      const base = slugify(text);
      const seen = used.get(base) ?? 0;
      used.set(base, seen + 1);
      const id = seen === 0 ? base : `${base}-${seen + 1}`;
      node.properties = { ...node.properties, id };
      node.children = [
        ...node.children,
        {
          type: "element",
          tagName: "a",
          properties: {
            className: ["docs-anchor"],
            href: `#${id}`,
            "aria-label": `Link to “${text}”`,
          },
          children: [{ type: "text", value: "#" }],
        },
      ];
      headings.push({ depth, id, text });
      return undefined;
    });
  };
}

/**
 * Render one page body to HTML, returning the markup and the headings that make its TOC.
 * `file` names the page in every error the pipeline raises.
 */
export async function renderPage(body, file) {
  const highlighter = await docsHighlighter();
  const headings = [];
  const rendered = await unified()
    .use(remarkParse)
    .use(remarkGfm)
    .use(remarkDocsComponents, { file })
    .use(remarkCodeTitles)
    .use(remarkRehype)
    .use(rehypeHeadings, { file, headings })
    .use(rehypeShikiFromHighlighter, highlighter, {
      theme: HIGHLIGHT_THEME,
      fallbackLanguage: "plaintext",
      onError: () => {
        // A grammar shiki cannot process must not fail a build — the fence renders plain.
      },
    })
    .use(rehypeStringify)
    .process(body);
  return { html: String(rendered), headings };
}
