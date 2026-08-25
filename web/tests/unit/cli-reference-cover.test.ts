import { readdirSync, readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";

/**
 * EVERY COMMAND THE DOCUMENTATION NAMES HAS AN ENTRY TO LOOK UP.
 *
 * The CLI reference is generated from the binary's own command tree, but the generator renders
 * only the verbs named in three hand-kept lists (bins/topos/src/cli_ref.rs) — so a verb added to
 * the CLI and left out of those lists renders nowhere, and the page goes on looking complete.
 * `topos workspace`, `topos install`, and `topos relay` all shipped that way: the pages sent
 * readers to them, a refusal named them, an MCP config file spelled one out, and the reference
 * had nothing under that name.
 *
 * So: gather every `topos <verb>` the documentation spells — the reference's own prose included,
 * since its flag table is where `topos workspace use` was cited — and require each one to be a
 * heading on the reference page, or one of the aliases the page deliberately keeps.
 */

const DOCS = resolve(__dirname, "..", "..", "..", "docs");

/** Every `topos <verb>` spelled in a code span. */
function verbsNamedIn(text: string): string[] {
  return [...text.matchAll(/`topos ([a-z][a-z-]*)/g)].map((match) => match[1] as string);
}

describe("the CLI reference", () => {
  it("has an entry for every command the documentation names", () => {
    const reference = readFileSync(join(DOCS, "cli.md"), "utf8");
    // The headings ARE the entries — `### `topos add`` and the nested `#### `topos workspace use``.
    const entries = new Set(
      [...reference.matchAll(/^#{3,4} `topos ([a-z-]+)/gm)].map((match) => match[1] as string),
    );
    // The two spellings the page documents as aliases rather than as commands of their own.
    const aliases = new Set(
      [...reference.matchAll(/- `topos ([a-z-]+)`/g)].map((match) => match[1] as string),
    );
    expect(entries.size).toBeGreaterThan(10);

    const sources = readdirSync(DOCS)
      .filter((name) => name.endsWith(".mdx"))
      .map((name) => ({ name, text: readFileSync(join(DOCS, name), "utf8") }));
    // The reference itself, minus its headings — those are the answers, not the questions.
    sources.push({ name: "cli.md", text: reference.replace(/^#{2,4} .*$/gm, "") });

    const uncovered = sources.flatMap(({ name, text }) =>
      verbsNamedIn(text)
        .filter((verb) => !entries.has(verb) && !aliases.has(verb))
        .map(
          (verb) => `docs/${name} names \`topos ${verb}\`, which the reference has no entry for`,
        ),
    );

    expect([...new Set(uncovered)]).toEqual([]);
  });
});
