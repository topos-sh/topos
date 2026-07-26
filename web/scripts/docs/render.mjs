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
 * The docs render pipeline — the app's own remark/rehype chain, with the docs-specific plugins:
 *
 *   remark-parse → remark-gfm → components → code titles
 *     → remark-rehype → headings/anchors → code frames → shiki → pre-style strip → stringify
 *
 * remark-rehype runs WITHOUT `allowDangerousHtml`, and the component plugin ahead of it has
 * already turned every tag it recognises into structure and every stray one into literal text.
 * So raw HTML cannot reach the output at all — a stronger guarantee than sanitizing after the
 * fact, and the reason this renderer needs no sanitizer: there is no path from source bytes to
 * live markup except the component set and shiki's own escaped spans.
 *
 * Highlighting happens HERE, at generate time, exactly once per code block. The browser receives
 * finished markup and downloads no highlighter. Every block is framed with a copy button (the
 * shell wires ONE delegated click handler), and command fences (`bash`/`sh`/`console`) sit on the
 * design system's dark terminal glass — which is why shiki runs BOTH themes: the light theme
 * colours ordinary blocks, the dark theme's colours ride along as CSS variables the glass frame
 * switches to. Shiki's own inline `<pre>` page colours are stripped afterwards so the docs
 * stylesheet — not the theme's page defaults — owns every frame.
 */

export const HIGHLIGHT_THEMES = { light: "github-light", dark: "github-dark" };

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
    themes: Object.values(HIGHLIGHT_THEMES),
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

/** Fence languages that ARE commands — framed on the dark terminal glass so they stand out. */
const COMMAND_LANGUAGES = new Set(["bash", "sh", "shellscript", "console", "zsh"]);

/** One inline icon of the copy button (finished markup — the browser loads no icon set). */
function copyIcon(variant, children) {
  return {
    type: "element",
    tagName: "svg",
    properties: {
      className: ["docs-copy__icon", `docs-copy__icon--${variant}`],
      viewBox: "0 0 24 24",
      fill: "none",
      stroke: "currentColor",
      strokeWidth: "2",
      strokeLinecap: "round",
      strokeLinejoin: "round",
      ariaHidden: "true",
    },
    children,
  };
}

/** The copy affordance every code block carries; the docs shell wires ONE delegated handler. */
function copyButtonNode() {
  return {
    type: "element",
    tagName: "button",
    properties: {
      type: "button",
      className: ["docs-copy"],
      dataCopy: "",
      ariaLabel: "Copy to clipboard",
    },
    children: [
      copyIcon("copy", [
        {
          type: "element",
          tagName: "rect",
          properties: { width: "14", height: "14", x: "8", y: "8", rx: "2", ry: "2" },
          children: [],
        },
        {
          type: "element",
          tagName: "path",
          properties: { d: "M4 16c-1.1 0-2-.9-2-2V4c0-1.1.9-2 2-2h10c1.1 0 2 .9 2 2" },
          children: [],
        },
      ]),
      copyIcon("check", [
        { type: "element", tagName: "path", properties: { d: "M20 6 9 17l-5-5" }, children: [] },
      ]),
    ],
  };
}

/**
 * Wrap every `<pre>` in a positioned frame carrying the copy button, and mark command fences so
 * the stylesheet can set them on the dark glass. Runs BEFORE shiki (the language class is still
 * on the `<code>`); shiki then highlights the same `<pre>` inside the frame.
 */
function rehypeCodeFrames() {
  return (tree) => {
    walk(tree, (node, parent) => {
      if (node.type !== "element" || node.tagName !== "pre") {
        return undefined;
      }
      // The walk revisits the replacement's children — an already-framed pre stays as it is.
      if (parent?.properties?.className?.includes("docs-codeblock")) {
        return undefined;
      }
      const code = (node.children ?? []).find(
        (child) => child.type === "element" && child.tagName === "code",
      );
      const language = (code?.properties?.className ?? [])
        .map(String)
        .find((name) => name.startsWith("language-"))
        ?.slice("language-".length);
      const classes = ["docs-codeblock"];
      if (COMMAND_LANGUAGES.has(language ?? "")) {
        classes.push("docs-codeblock--command");
      }
      return {
        type: "element",
        tagName: "div",
        properties: { className: classes },
        children: [node, copyButtonNode()],
      };
    });
  };
}

/**
 * Post-shiki colour pass. Two jobs: drop shiki's inline page colours from every `<pre>`
 * (background + foreground — the docs stylesheet owns the frames, and an inline style would win
 * over it), and inside COMMAND frames flip each token to the dark theme's colour that shiki left
 * riding along as a `--shiki-dark` variable — rewritten here, at generate time, so the
 * stylesheet needs no `!important` to beat an inline style.
 */
function rehypeCodeColors() {
  const recolor = (node, inCommand) => {
    if (!Array.isArray(node.children)) {
      return;
    }
    const isCommand =
      inCommand ||
      (node.type === "element" &&
        (node.properties?.className ?? []).includes("docs-codeblock--command"));
    for (const child of node.children) {
      if (child.type === "element" && child.tagName === "pre" && child.properties?.style) {
        delete child.properties.style;
      }
      if (isCommand && child.type === "element" && typeof child.properties?.style === "string") {
        const style = child.properties.style;
        if (style.includes("--shiki-dark:")) {
          child.properties.style = style.replace(/(^|;)color:[^;]+/, "$1color:var(--shiki-dark)");
        }
      }
      recolor(child, isCommand);
    }
  };
  return (tree) => {
    recolor(tree, false);
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
    .use(rehypeCodeFrames)
    .use(rehypeShikiFromHighlighter, highlighter, {
      themes: HIGHLIGHT_THEMES,
      defaultColor: "light",
      fallbackLanguage: "plaintext",
      onError: () => {
        // A grammar shiki cannot process must not fail a build — the fence renders plain.
      },
    })
    .use(rehypeCodeColors)
    .use(rehypeStringify)
    .process(body);
  return { html: String(rendered), headings };
}
