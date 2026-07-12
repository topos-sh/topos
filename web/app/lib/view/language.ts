/**
 * Pure path → render-kind mapping for the skill file browser. No IO, no shiki import — it decides
 * WHICH renderer a bundle file gets (markdown prose, syntax-highlighted code, or escaped plaintext)
 * from the path alone, so a loader can branch before it ever loads bytes. Mirrors the pure
 * diff modules (classify.ts): a small deterministic function the highlighter and the page share.
 *
 * The extension match is case-INSENSITIVE (README.MD still renders as markdown); the two filename
 * matches (Dockerfile, Makefile) are case-SENSITIVE because that is how the tools themselves spell
 * them. Everything unrecognised — txt, csv, extensionless files, dotfiles — is "plain": shown as
 * escaped text, never guessed at.
 */

export type FileRenderKind = "markdown" | "code" | "plain";

export interface LanguageInfo {
  kind: FileRenderKind;
  /** shiki language id, set only when kind === "code" */
  lang?: string;
}

// extension (lower-cased, no dot) → shiki language id. The VALUE is the canonical shiki id we load
// and highlight with; several extensions collapse onto one id (mjs/cjs → javascript, yml → yaml,
// sh/bash/zsh → shellscript, patch → diff).
const EXT_TO_LANG: Record<string, string> = {
  ts: "typescript",
  tsx: "tsx",
  js: "javascript",
  jsx: "jsx",
  mjs: "javascript",
  cjs: "javascript",
  json: "json",
  jsonc: "jsonc",
  yaml: "yaml",
  yml: "yaml",
  toml: "toml",
  py: "python",
  rs: "rust",
  go: "go",
  rb: "ruby",
  sh: "shellscript",
  bash: "shellscript",
  zsh: "shellscript",
  fish: "fish",
  sql: "sql",
  html: "html",
  css: "css",
  xml: "xml",
  diff: "diff",
  patch: "diff",
  mdx: "mdx",
};

const MARKDOWN_EXT = new Set(["md", "markdown"]);

// basename (case-SENSITIVE) → shiki language id, for the two extensionless files the toolchain
// spells with a fixed capitalisation.
const FILENAME_TO_LANG: Record<string, string> = {
  Dockerfile: "dockerfile",
  Makefile: "makefile",
};

/**
 * The exact, de-duplicated set of shiki language ids this map can emit. The highlighter preloads
 * PRECISELY these (see highlight.server.ts), so this map stays the single source of truth for the
 * loaded language set — a new extension mapping automatically joins the preload list.
 */
export const HIGHLIGHT_LANGUAGES: readonly string[] = [
  ...new Set([...Object.values(EXT_TO_LANG), ...Object.values(FILENAME_TO_LANG)]),
];

export function languageForPath(path: string): LanguageInfo {
  const slash = path.lastIndexOf("/");
  const basename = slash === -1 ? path : path.slice(slash + 1);

  const filenameLang = FILENAME_TO_LANG[basename];
  if (filenameLang !== undefined) {
    return { kind: "code", lang: filenameLang };
  }

  const dot = basename.lastIndexOf(".");
  // No dot, or a leading-dot dotfile (".gitignore" → dot at index 0) has no usable extension.
  if (dot <= 0) {
    return { kind: "plain" };
  }
  const ext = basename.slice(dot + 1).toLowerCase();

  if (MARKDOWN_EXT.has(ext)) {
    return { kind: "markdown" };
  }
  const lang = EXT_TO_LANG[ext];
  if (lang !== undefined) {
    return { kind: "code", lang };
  }
  return { kind: "plain" };
}
