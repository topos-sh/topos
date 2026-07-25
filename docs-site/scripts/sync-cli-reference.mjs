#!/usr/bin/env node
// Derive the docs site's CLI reference page from the generated `docs/cli.md`.
//
// `docs/cli.md` is rendered from the real clap tree by `cargo xtask gen-cli-ref` and is the one
// authority for every flag. This script re-wraps those exact bytes as an MDX page so the site can
// never describe a flag the binary does not have.
//
//   node docs-site/scripts/sync-cli-reference.mjs           # write the page
//   node docs-site/scripts/sync-cli-reference.mjs --check    # fail if it is stale (CI gate)

import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = join(here, "..", "..");
const source = join(repoRoot, "docs", "cli.md");
const target = join(repoRoot, "docs-site", "cli", "reference.mdx");

const FRONTMATTER = `---
title: "CLI reference"
description: "Every topos verb and flag, generated from the binary's own command tree."
icon: "terminal"
---

<Info>
  This page is generated from the \`topos\` binary's own command tree — it cannot describe a flag the
  binary does not have. Run \`topos <verb> --help\` for the same text offline.
</Info>

`;

// Drop the generated file's H1 (Mintlify renders the frontmatter title) and its
// do-not-hand-edit banner, which is addressed to repo contributors, not readers.
function toMdx(markdown) {
  const lines = markdown.split("\n");
  const body = [];
  let seenHeading = false;
  for (const line of lines) {
    if (!seenHeading) {
      if (line.startsWith("# ")) {
        seenHeading = true;
        continue;
      }
      continue;
    }
    if (line.startsWith("> GENERATED from")) continue;
    body.push(line);
  }
  return FRONTMATTER + body.join("\n").replace(/^\n+/, "") + "\n";
}

const rendered = toMdx(readFileSync(source, "utf8"));

if (process.argv.includes("--check")) {
  let current = "";
  try {
    current = readFileSync(target, "utf8");
  } catch {
    /* missing counts as stale */
  }
  if (current !== rendered) {
    console.error(
      "docs-site/cli/reference.mdx is stale.\n" +
        "Run: node docs-site/scripts/sync-cli-reference.mjs",
    );
    process.exit(1);
  }
  console.log("docs-site/cli/reference.mdx is in sync with docs/cli.md");
} else {
  writeFileSync(target, rendered);
  console.log(`wrote ${target}`);
}
