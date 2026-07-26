import { assertKnownComponent, parseTag } from "./components.mjs";
import { DocsContentError } from "./frontmatter.mjs";

/**
 * Source preparation — the two passes that run before a page is ever parsed as markdown.
 *
 * 1. NORMALIZE. Markdown treats a line that is one HTML tag as an HTML block that swallows every
 *    following non-blank line, and treats a 4-space indent as code. Component children are
 *    conventionally written indented under their tag, so a page authored the natural way would
 *    lose its markdown. Normalizing fixes both without asking authors to think about it: every
 *    component tag becomes its own block (blank line above and below, at column 0) and each
 *    block's children are dedented to the margin. Fenced code is never touched.
 *
 * 2. EXPAND. One page carries the generated CLI reference marker. It is replaced with the
 *    contents of the repo's generated `docs/cli.md`, minus the reference's own H1 and its
 *    "GENERATED" provenance quote — the page provides the title and says where the text comes
 *    from, and with the H1 gone the reference's sections (h2) and commands (h3) land straight in
 *    the page TOC, so every command is jumpable. The file is written by
 *    `cargo xtask gen-cli-ref`; nothing here or in MDX ever restates it.
 */

/** The exact marker line a content page uses to pull in the generated CLI reference. */
export const CLI_REFERENCE_MARKER = "{/* GENERATED-CLI-REFERENCE */}";

const FENCE = /^(\s*)(```|~~~)/;

/** Walk lines, tracking fenced-code state, so no pass ever rewrites the inside of a fence. */
function* codeAware(lines) {
  let fence = null;
  for (const line of lines) {
    const match = line.match(FENCE);
    if (fence === null && match) {
      fence = match[2];
      yield { line, inFence: true, fenceEdge: true };
      continue;
    }
    if (fence !== null) {
      const closes = match !== null && match[2] === fence;
      if (closes) {
        fence = null;
      }
      yield { line, inFence: true, fenceEdge: closes };
      continue;
    }
    yield { line, inFence: false, fenceEdge: false };
  }
}

/**
 * Give every component tag its own block and pull each block's children back to the margin.
 * `strip` is decided by the first non-blank line inside a block and applied with a floor of the
 * line's own indent, so a closing tag or a short line can never be over-trimmed.
 */
export function normalizeSource(source, file) {
  const out = [];
  const stack = [];
  const pushBlank = () => {
    if (out.length > 0 && out[out.length - 1].trim() !== "") {
      out.push("");
    }
  };

  for (const { line, inFence } of codeAware(source.split("\n"))) {
    const open = stack.length > 0 ? stack[stack.length - 1] : null;
    // The first non-blank line inside a block sets that block's margin — a fenced opener
    // included, since a fence indented four spaces would otherwise read as indented code.
    if (open !== null && open.strip === null && line.trim() !== "") {
      open.strip = indentOf(line);
    }
    const strip = open === null ? 0 : (open.strip ?? 0);
    const dedented = line.slice(Math.min(indentOf(line), strip));

    if (inFence) {
      out.push(dedented);
      continue;
    }
    const trimmed = line.trim();
    assertKnownComponent(trimmed, file);
    const tag = trimmed.startsWith("<") ? parseTag(trimmed, file) : null;
    if (tag === null) {
      out.push(dedented);
      continue;
    }
    if (tag.kind === "close") {
      if (stack.pop() === undefined) {
        throw new DocsContentError(`${file}: </${tag.name}> closes nothing`);
      }
      pushBlank();
      out.push(trimmed);
      out.push("");
      continue;
    }
    pushBlank();
    out.push(trimmed);
    out.push("");
    if (tag.kind === "open") {
      stack.push({ name: tag.name, strip: null });
    }
  }

  if (stack.length > 0) {
    throw new DocsContentError(`${file}: <${stack[stack.length - 1].name}> is never closed`);
  }
  return `${collapseBlankRuns(out).join("\n").trim()}\n`;
}

function indentOf(line) {
  return line.length - line.trimStart().length;
}

/** At most one blank line between blocks — outside fences, where blank lines are content. */
function collapseBlankRuns(lines) {
  const out = [];
  for (const { line, inFence } of codeAware(lines)) {
    if (!inFence && line.trim() === "" && out.length > 0 && out[out.length - 1].trim() === "") {
      continue;
    }
    out.push(line);
  }
  return out;
}

/**
 * Drop the reference's own leading H1 and the "GENERATED" provenance blockquote under it. The
 * standalone file (docs/cli.md on GitHub, the built-in skill's reference.md) needs both; a docs
 * PAGE already has a title and states the provenance in its own intro.
 */
export function stripReferenceHeader(reference) {
  const lines = reference.split("\n");
  let index = 0;
  const skipBlanks = () => {
    while (index < lines.length && lines[index].trim() === "") {
      index += 1;
    }
  };
  skipBlanks();
  if (lines[index]?.startsWith("# ")) {
    index += 1;
  }
  skipBlanks();
  while (index < lines.length && lines[index].startsWith(">")) {
    index += 1;
  }
  return lines.slice(index).join("\n");
}

/** Does this page pull in the generated CLI reference? */
export function hasCliReferenceMarker(body) {
  return body.split("\n").some((line) => line.trim() === CLI_REFERENCE_MARKER);
}

/**
 * Replace the marker line with the generated reference. The reference is markdown, not MDX: it
 * is spliced as source so its tables, fences, and headings compile through the same pipeline as
 * everything else on the page.
 */
export function expandCliReference(body, reference) {
  return body
    .split("\n")
    .map((line) =>
      line.trim() === CLI_REFERENCE_MARKER ? stripReferenceHeader(reference).trim() : line,
    )
    .join("\n");
}
