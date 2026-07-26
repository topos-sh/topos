#!/usr/bin/env node
/**
 * gen-docs.mjs — compile docs/ into the committed module the app serves.
 *
 *   bun run gen:docs            write app/lib/docs/content.generated.server.ts
 *   bun run gen:docs --check    regenerate in memory and fail on drift (the CI gate)
 *
 * The two-command loop is the same one the other generated artifacts in this repo use: edit the
 * source, regenerate, commit the result. Generating instead of reading at request time is what
 * lets the runtime image ship without any documentation files and lets a downstream build
 * inherit the docs by compiling this app's source unchanged.
 *
 * `TOPOS_DOCS_DIR` re-points the content root (relative to web/); tests aim it at
 * tests/fixtures/docs.
 */
import { readFile, writeFile } from "node:fs/promises";
import { relative } from "node:path";
import { compileDocs } from "./docs/compile.mjs";
import { emitModule } from "./docs/emit.mjs";
import { DocsContentError } from "./docs/frontmatter.mjs";
import { DOCS_CONTENT_ROOT, GENERATED_MODULE, WEB_ROOT } from "./docs/paths.mjs";

const check = process.argv.includes("--check");
const target = relative(WEB_ROOT, GENERATED_MODULE);

async function main() {
  const compiled = await compileDocs({ contentRoot: DOCS_CONTENT_ROOT });
  const next = emitModule(compiled);

  if (!check) {
    await writeFile(GENERATED_MODULE, next);
    console.warn(`${target}: ${compiled.pages.length} pages from ${DOCS_CONTENT_ROOT}`);
    return;
  }

  const current = await readFile(GENERATED_MODULE, "utf8").catch(() => null);
  if (current === null) {
    console.error(`docs drift check FAILED: ${target} does not exist — run \`bun run gen:docs\``);
    process.exit(1);
  }
  if (current !== next) {
    console.error(
      `docs drift check FAILED: ${target} does not match ${DOCS_CONTENT_ROOT}.\n` +
        "Run `bun run gen:docs` and commit the result.",
    );
    process.exit(1);
  }
  console.warn(`docs in sync (${compiled.pages.length} pages)`);
}

main().catch((error) => {
  if (error instanceof DocsContentError) {
    console.error(`docs content error: ${error.message}`);
    process.exit(1);
  }
  throw error;
});
