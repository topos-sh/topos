import { preloadDiffHTML } from "@pierre/diffs/ssr";
import sanitizeHtml from "sanitize-html";

/**
 * The ONE @pierre/diffs import in the app. Server-string rendering only: preloadDiffHTML
 * returns an HTML string which is then SANITIZED before it may be embedded anywhere —
 * an allowlist of the renderer's actual output (pre/code/div/span + data-* attributes +
 * a fixed set of style properties). Every event handler, script, URL-bearing style, or
 * unexpected element is dropped; text content stays escaped. File paths, authors, and
 * commit messages never enter this HTML — they render as React text nodes at the call
 * sites; only file CONTENTS flow through here.
 */

export const diffRendererName = "@pierre/diffs";

export interface RenderFileDiffInput {
  path: string;
  prevPath?: string;
  oldText?: string;
  newText?: string;
  /** Skip syntax highlighting (the >MAX_HIGHLIGHT_BYTES plain path). */
  plain?: boolean;
}

const RENDER_OPTIONS = {
  theme: "github-light",
  themeType: "light" as const,
  diffStyle: "unified" as const,
  disableFileHeader: true,
};

// The renderer's observed output vocabulary (pinned @pierre/diffs 1.2.12; the snapshot test
// pins it too). Style values are matched against strict shapes — no url(), no expressions.
const HEX_COLOR = /^#[0-9a-fA-F]{3,8}$/;
const SANITIZE_OPTIONS: sanitizeHtml.IOptions = {
  allowedTags: ["pre", "code", "div", "span"],
  allowedAttributes: { "*": ["data-*", "style"] },
  allowedStyles: {
    "*": {
      color: [HEX_COLOR],
      "background-color": [HEX_COLOR],
      "font-weight": [/^(?:bold|normal|[1-9]00)$/],
      "font-style": [/^(?:italic|normal)$/],
      "text-decoration": [/^(?:underline|line-through|none)$/],
      "grid-row": [/^span \d{1,5}$/],
      "min-height": [/^calc\(\d{1,5} \* 1lh\)$/],
    },
  },
  disallowedTagsMode: "discard",
};

/** Drop the per-render copies of the static library chrome (sprite sheet + style blocks). */
function stripChrome(html: string): string {
  return html.replace(/<svg[\s\S]*?<\/svg>/, "").replace(/<style[\s\S]*?<\/style>/g, "");
}

export async function renderFileDiffHTML(input: RenderFileDiffInput): Promise<string> {
  const lang = input.plain === true ? ("text" as const) : undefined;
  const raw = await preloadDiffHTML({
    oldFile: { name: input.prevPath ?? input.path, contents: input.oldText ?? "", lang },
    newFile: { name: input.path, contents: input.newText ?? "", lang },
    options: RENDER_OPTIONS,
  });
  return sanitizeHtml(stripChrome(raw), SANITIZE_OPTIONS);
}

let chromeCache: string | undefined;

/**
 * The renderer's page-level static assets (its stylesheet + icon sprite), extracted ONCE from
 * a render of FIXED, constant content — no user byte can reach this string, so it is the one
 * HTML block the review page embeds unsanitized. Injected a single time per page.
 */
export async function diffChromeAssets(): Promise<string> {
  if (chromeCache === undefined) {
    const fixed = await preloadDiffHTML({
      oldFile: { name: "a.txt", contents: "a\n" },
      newFile: { name: "a.txt", contents: "b\n" },
      options: RENDER_OPTIONS,
    });
    const sprite = fixed.match(/<svg[\s\S]*?<\/svg>/)?.[0] ?? "";
    const styles = fixed.match(/<style[\s\S]*?<\/style>/g)?.join("") ?? "";
    chromeCache = `${sprite}${styles}`;
  }
  return chromeCache;
}
