import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * The docs pipeline's fixed paths — the ONE place the content root is named.
 *
 * The MDX source lives at the REPO ROOT (`docs/`), deliberately outside `web/`: the same files
 * stay readable on GitHub, and the generator — not the running server — is what reads them. The
 * runtime never touches this directory; it imports the committed generated module below, so the
 * shipped image needs no docs files at all.
 *
 * `TOPOS_DOCS_DIR` re-points the content root (relative paths resolve against `web/`). Tests aim
 * it at `tests/fixtures/docs` so the pipeline is exercised on a small, stable content set instead
 * of the live documentation.
 */

export const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..");

/** The documentation source root: the repo's own `docs/`, or whatever `TOPOS_DOCS_DIR` names. */
export const DOCS_CONTENT_ROOT = process.env.TOPOS_DOCS_DIR
  ? resolve(WEB_ROOT, process.env.TOPOS_DOCS_DIR)
  : resolve(WEB_ROOT, "..", "docs");

/** The sidebar manifest — the single source of the docs tree (order, grouping, membership). */
export const NAV_FILE = "nav.json";

/**
 * The generated CLI reference, produced by `cargo xtask gen-cli-ref` and committed. One content
 * page expands it in place; it is never hand-edited and never duplicated into MDX.
 */
export const CLI_REFERENCE_FILE = resolve(WEB_ROOT, "..", "docs", "cli.md");

/** The committed module the app imports. Regenerate with `bun run gen:docs`. */
export const GENERATED_MODULE = join(WEB_ROOT, "app", "lib", "docs", "content.generated.server.ts");
