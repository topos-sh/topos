#!/usr/bin/env node
/**
 * check-design-tokens.mjs — the DESIGN.md ↔ app.css color-token drift gate.
 *
 * DESIGN.md's front matter duplicates the Klein color tokens that `@theme` in app/app.css
 * actually renders. Nothing structural ties the two files, and the design.md format lint
 * validates format only — so a token edited in one place silently forks the system. This script
 * makes the "DESIGN.md is the source of truth" claim executable: every `--color-*` in the @theme
 * block must appear in DESIGN.md's `colors:` map with the identical value, and vice versa.
 *
 * Named exception: `primary` is a design.md-format alias with no CSS token — it must equal
 * `accent` instead.
 */
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

/** The `--color-<name>: <value>;` pairs inside app.css's @theme block. */
function cssTokens() {
  const css = readFileSync(join(WEB_ROOT, "app/app.css"), "utf8");
  const themeStart = css.indexOf("@theme");
  const open = css.indexOf("{", themeStart);
  let depth = 1;
  let end = open + 1;
  while (depth > 0 && end < css.length) {
    const ch = css[end];
    if (ch === "{") depth += 1;
    if (ch === "}") depth -= 1;
    end += 1;
  }
  const block = css.slice(open + 1, end - 1);
  const tokens = new Map();
  for (const match of block.matchAll(/--color-([a-z0-9-]+)\s*:\s*([^;]+);/g)) {
    // Values may carry a trailing comment ("#00227a; /* hsl(...) */" keeps the ramp readable).
    const value = match[2].replace(/\/\*.*?\*\//g, "").trim();
    tokens.set(match[1], value.toLowerCase());
  }
  return tokens;
}

/** The `colors:` map from DESIGN.md's YAML front matter (flat `  key: "#hex"` lines). */
function designTokens() {
  const md = readFileSync(join(WEB_ROOT, "DESIGN.md"), "utf8");
  const front = md.split(/^---$/m)[1] ?? "";
  const colorsAt = front.indexOf("\ncolors:");
  if (colorsAt === -1) {
    return new Map();
  }
  const tokens = new Map();
  for (const line of front.slice(colorsAt + "\ncolors:".length).split("\n")) {
    if (/^\S/.test(line)) {
      break; // the next top-level front-matter key ends the colors map
    }
    const match = line.match(/^\s+([a-z0-9-]+):\s*"([^"]+)"/);
    if (match) {
      tokens.set(match[1], match[2].toLowerCase());
    }
  }
  return tokens;
}

const css = cssTokens();
const design = designTokens();
const failures = [];

if (css.size === 0) {
  failures.push("no --color-* tokens found in app.css @theme — the parser or the file broke");
}
if (design.size === 0) {
  failures.push("no colors: map found in DESIGN.md front matter — the parser or the file broke");
}

for (const [name, value] of css) {
  const documented = design.get(name);
  if (documented === undefined) {
    failures.push(`--color-${name} (${value}) is in app.css but missing from DESIGN.md colors`);
  } else if (documented !== value) {
    failures.push(`token "${name}" drifted: app.css ${value} vs DESIGN.md ${documented}`);
  }
}
for (const [name, value] of design) {
  if (name === "primary") {
    if (value !== design.get("accent")) {
      failures.push(`DESIGN.md "primary" (${value}) must alias "accent" (${design.get("accent")})`);
    }
    continue;
  }
  if (!css.has(name)) {
    failures.push(`DESIGN.md color "${name}" (${value}) has no --color-${name} in app.css`);
  }
}

if (failures.length > 0) {
  console.error("design-token drift check FAILED:");
  for (const failure of failures) {
    console.error(`  - ${failure}`);
  }
  console.error("Fix BOTH files together — DESIGN.md is the source of truth app.css renders.");
  process.exit(1);
}
console.warn(`design tokens in sync (${css.size} tokens)`);
