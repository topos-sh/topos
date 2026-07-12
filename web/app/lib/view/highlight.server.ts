import sanitizeHtml from "sanitize-html";
import { createHighlighter, type Highlighter } from "shiki";
import { createJavaScriptRegexEngine } from "shiki/engine/javascript";
import { HIGHLIGHT_LANGUAGES } from "./language";

/**
 * Server-side syntax highlighting for a single bundle file, mirroring pierre.server.ts: shiki
 * renders an HTML string which is then run through a STRICT sanitize-html allowlist matched to
 * shiki's actual output (pre/code/span + a fixed style vocabulary) before it may be embedded
 * anywhere. Every trust decision stays elsewhere; this only turns text into inert, highlighted
 * markup.
 *
 * Engine choice — the JavaScript RegExp engine (`shiki/engine/javascript`), NOT oniguruma: the
 * server build then carries no WebAssembly (the whole grammar set compiles against the platform
 * RegExp). `forgiving: true` makes a grammar pattern the JS engine can't express skip silently
 * instead of throwing, so highlighting degrades to fewer spans rather than failing a page.
 *
 * One lazily-created highlighter singleton (a cached promise) preloads EXACTLY the languages the
 * extension map can emit (HIGHLIGHT_LANGUAGES) under the "github-light" theme — the same theme the
 * review diff renders with. An unknown/absent language falls back to escaped plaintext; on any
 * unexpected failure the text is hand-escaped into a bare <pre>. Nothing here ever throws.
 */

export const HIGHLIGHT_THEME = "github-light";

let highlighterPromise: Promise<Highlighter> | undefined;

/** The shared highlighter singleton — reused by the markdown pipeline so both render paths load the
 *  language set and theme exactly once. */
export function sharedHighlighter(): Promise<Highlighter> {
  if (highlighterPromise === undefined) {
    highlighterPromise = createHighlighter({
      themes: [HIGHLIGHT_THEME],
      langs: [...HIGHLIGHT_LANGUAGES],
      engine: createJavaScriptRegexEngine({ forgiving: true }),
    });
  }
  return highlighterPromise;
}

const HEX_COLOR = /^#[0-9a-fA-F]{3,8}$/;

// Matched to shiki's actual output: `<pre class style tabindex><code class><span class style>…`.
const SANITIZE_OPTIONS: sanitizeHtml.IOptions = {
  allowedTags: ["pre", "code", "span"],
  allowedAttributes: {
    pre: ["class", "style", "tabindex"],
    code: ["class"],
    span: ["class", "style"],
  },
  allowedStyles: {
    "*": {
      color: [HEX_COLOR],
      "background-color": [HEX_COLOR],
      "font-style": [/^(?:italic|normal)$/],
      "font-weight": [/^(?:bold|normal|[1-9]00)$/],
      "text-decoration": [/^(?:underline|line-through|none)$/],
    },
  },
  disallowedTagsMode: "discard",
};

const HTML_ESCAPE: Record<string, string> = {
  "&": "&amp;",
  "<": "&lt;",
  ">": "&gt;",
  '"': "&quot;",
  "'": "&#39;",
};

function escapeHtml(text: string): string {
  return text.replace(/[&<>"']/g, (ch) => HTML_ESCAPE[ch] ?? ch);
}

/**
 * Render `text` as highlighted, sanitized HTML. `lang` is a shiki language id (from
 * languageForPath); undefined or a not-loaded id renders as plaintext. The output is always
 * sanitized before return, so the caller embeds it as-is.
 */
export async function renderCodeHTML(text: string, lang: string | undefined): Promise<string> {
  const highlighter = await sharedHighlighter();
  const loaded = new Set(highlighter.getLoadedLanguages());
  const resolvedLang = lang !== undefined && loaded.has(lang) ? lang : "plaintext";

  let raw: string;
  try {
    raw = highlighter.codeToHtml(text, { lang: resolvedLang, theme: HIGHLIGHT_THEME });
  } catch {
    // Defensive: a loaded lang or plaintext should never throw, but a render failure must never
    // escape — fall back to hand-escaped plaintext in a bare <pre>.
    raw = `<pre><code>${escapeHtml(text)}</code></pre>`;
  }
  return sanitizeHtml(raw, SANITIZE_OPTIONS);
}
