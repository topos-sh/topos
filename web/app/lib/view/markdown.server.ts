import rehypeShikiFromHighlighter from "@shikijs/rehype/core";
import rehypeStringify from "rehype-stringify";
import remarkGfm from "remark-gfm";
import remarkParse from "remark-parse";
import remarkRehype from "remark-rehype";
import sanitizeHtml from "sanitize-html";
import { unified } from "unified";
import { HIGHLIGHT_THEME, sharedHighlighter } from "./highlight.server";

/**
 * Server-side markdown rendering for a bundle's prose file (SKILL.md / README.md), mirroring
 * pierre.server.ts's render-then-sanitize shape. The unified pipeline is deliberately narrow:
 *
 *   remark-parse → remark-gfm → remark-rehype → shiki (fenced code) → rehype-stringify
 *
 * remark-rehype runs WITHOUT `allowDangerousHtml`, so raw HTML embedded in the markdown source is
 * DROPPED at the mdast→hast boundary (a `<script>` in a SKILL.md never reaches the output). Fenced
 * code blocks are highlighted by the SAME highlighter singleton the code view uses (shared via
 * @shikijs/rehype/core) — an unknown or missing fence language falls back to plaintext and never
 * throws (fallbackLanguage + onError). sanitize-html then runs as defense-in-depth over a strict
 * allowlist, so nothing trusts the pipeline's output blindly.
 *
 * Relative links are KEPT (allowProtocolRelative:false blocks `//host` but a scheme-less href like
 * `other.md` passes): the file-browser URL structure nests bundle files under one skill path, so a
 * relative link between two files in the same bundle resolves to the sibling's browser URL. Images
 * are dropped entirely — rendering one would need a byte-serving route this tier deliberately does
 * not expose, so `<img>` is simply not in the allowlist. Every `<a>` gains rel="noopener noreferrer".
 */

const HEX_COLOR = /^#[0-9a-fA-F]{3,8}$/;

const SANITIZE_OPTIONS: sanitizeHtml.IOptions = {
  allowedTags: [
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "p",
    "a",
    "ul",
    "ol",
    "li",
    "blockquote",
    "pre",
    "code",
    "span",
    "em",
    "strong",
    "del",
    "hr",
    "br",
    "table",
    "thead",
    "tbody",
    "tr",
    "th",
    "td",
    "input",
  ],
  allowedAttributes: {
    // rel is allowlisted only so the fixed rel="noopener noreferrer" the transform below adds is
    // not stripped straight back off — its value is a constant, never attacker-controlled.
    a: ["href", "rel"],
    ol: ["start"],
    input: ["type", "checked", "disabled"],
    ul: ["class"],
    li: ["class"],
    pre: ["class", "style"],
    code: ["class"],
    span: ["class", "style"],
    th: ["style", "align"],
    td: ["style", "align"],
  },
  allowedStyles: {
    "*": {
      color: [HEX_COLOR],
      "background-color": [HEX_COLOR],
      "font-style": [/^(?:italic|normal)$/],
      "font-weight": [/^(?:bold|normal|[1-9]00)$/],
      "text-decoration": [/^(?:underline|line-through|none)$/],
      "text-align": [/^(?:left|center|right)$/],
    },
  },
  allowedSchemes: ["http", "https", "mailto"],
  allowProtocolRelative: false,
  transformTags: {
    a: sanitizeHtml.simpleTransform("a", { rel: "noopener noreferrer" }),
  },
  disallowedTagsMode: "discard",
};

/** Render markdown `text` to sanitized HTML. Highlighting shares the code view's highlighter. */
export async function renderMarkdownHTML(text: string): Promise<string> {
  const highlighter = await sharedHighlighter();
  const rendered = await unified()
    .use(remarkParse)
    .use(remarkGfm)
    .use(remarkRehype)
    .use(rehypeShikiFromHighlighter, highlighter, {
      theme: HIGHLIGHT_THEME,
      fallbackLanguage: "plaintext",
      onError: () => {
        // A fence with a grammar shiki can't process must not fail the page — it renders plain.
      },
    })
    .use(rehypeStringify)
    .process(text);
  return sanitizeHtml(String(rendered), SANITIZE_OPTIONS);
}
