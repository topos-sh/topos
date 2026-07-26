/**
 * The frontmatter contract — three keys, nothing else.
 *
 *   title        REQUIRED — the page's h1 and its document <title>
 *   description  REQUIRED — the meta description, the sidebar/index summary, the llms.txt line
 *   sidebar_label  optional — a shorter label for the sidebar entry
 *
 * A page that breaks the contract fails the GENERATOR, loudly, naming the file: a docs page must
 * never render blank or title-less because someone forgot a key. The parser is deliberately not a
 * YAML engine — the contract is three scalar string keys, so a scalar reader is the whole job and
 * there is no document-model surface to get surprised by.
 */

/** Thrown for every content-contract violation; the generator prints `message` and exits 1. */
export class DocsContentError extends Error {
  constructor(message) {
    super(message);
    this.name = "DocsContentError";
  }
}

const REQUIRED_KEYS = ["title", "description"];
const OPTIONAL_KEYS = ["sidebar_label"];
const KNOWN_KEYS = new Set([...REQUIRED_KEYS, ...OPTIONAL_KEYS]);

/** Strip one layer of matching quotes; anything else is taken literally (trimmed). */
function scalar(raw) {
  const text = raw.trim();
  if (text.length >= 2) {
    const first = text[0];
    const last = text[text.length - 1];
    if ((first === '"' && last === '"') || (first === "'" && last === "'")) {
      return text.slice(1, -1);
    }
  }
  return text;
}

/**
 * Split `source` into its validated frontmatter and the body that follows.
 * `file` names the page in every error message.
 */
export function parseFrontmatter(source, file) {
  const normalized = source.replace(/^﻿/, "").replace(/\r\n/g, "\n");
  if (!normalized.startsWith("---\n")) {
    throw new DocsContentError(
      `${file}: missing frontmatter — every docs page opens with a --- block carrying title and description`,
    );
  }
  const end = normalized.indexOf("\n---", 3);
  if (end === -1) {
    throw new DocsContentError(`${file}: the frontmatter block is never closed by a --- line`);
  }
  const block = normalized.slice(4, end + 1);
  // Everything after the closing `---` line, minus the blank lines separating it from the body.
  const body = normalized.slice(end + 4).replace(/^(?:[ \t]*\n)+/, "");

  const values = {};
  for (const [index, line] of block.split("\n").entries()) {
    if (line.trim() === "") {
      continue;
    }
    const match = line.match(/^([A-Za-z_][A-Za-z0-9_]*)\s*:\s*(.*)$/);
    if (!match) {
      throw new DocsContentError(
        `${file}: frontmatter line ${index + 1} is not a "key: value" pair — ${JSON.stringify(line)}`,
      );
    }
    const [, key, raw] = match;
    if (!KNOWN_KEYS.has(key)) {
      throw new DocsContentError(
        `${file}: unknown frontmatter key "${key}" — the contract is ${[...KNOWN_KEYS].join(", ")}`,
      );
    }
    if (key in values) {
      throw new DocsContentError(`${file}: frontmatter key "${key}" appears twice`);
    }
    const value = scalar(raw);
    if (value === "") {
      throw new DocsContentError(`${file}: frontmatter key "${key}" is empty`);
    }
    values[key] = value;
  }

  for (const key of REQUIRED_KEYS) {
    if (!(key in values)) {
      throw new DocsContentError(`${file}: frontmatter is missing the required "${key}" key`);
    }
  }

  return {
    title: values.title,
    description: values.description,
    sidebarLabel: values.sidebar_label ?? null,
    body,
  };
}
