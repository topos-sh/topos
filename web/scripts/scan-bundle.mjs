#!/usr/bin/env node
/**
 * scan-bundle.mjs — the post-build leak gate. Scans every file the browser can be served
 * (build/client/**) for server-secret NAMES. None of these identifiers has any business existing
 * client-side; a single hit means a server-only value crossed the boundary and the build must not
 * ship. Run after `react-router build`.
 */
import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");

const CLIENT_DIR = join(WEB_ROOT, "build", "client");

const FORBIDDEN_NAMES = [
  "PLANE_INTERNAL_TOKEN",
  "PLANE_INTERNAL_URL",
  "BETTER_AUTH_SECRET",
  "DATABASE_URL",
];

if (!existsSync(join(WEB_ROOT, "build"))) {
  console.error("scan-bundle: no build directory — run `react-router build` first, then re-run");
  process.exit(1);
}

function* walk(dir) {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      yield* walk(full);
    } else {
      yield full;
    }
  }
}

const needles = FORBIDDEN_NAMES.map((name) => [name, Buffer.from(name, "utf8")]);

// The ONE tolerated occurrence shape: better-auth's client ships an isomorphic env GETTER that
// names its canonical env vars as string literals — `get BETTER_AUTH_SECRET(){return u("BETTER_AUTH_SECRET")}`
// (minified helper name varies). In a browser it reads nothing (no process.env), and this build
// runs WITHOUT secrets, so no value can have been inlined. We strip exactly that vendor getter
// pattern before scanning; any other occurrence of the name — ours or theirs — still fails.
const VENDOR_ENV_GETTER =
  /get (BETTER_AUTH_[A-Z_]+|AUTH_SECRET)\(\)\s*\{\s*return\s+\w+\(\s*"\1"\s*\)(?:\s*\?\?\s*[^}]{0,40})?\}/g;

if (!existsSync(CLIENT_DIR)) {
  console.error(
    `scan-bundle: ${relative(WEB_ROOT, CLIENT_DIR)} not present — build output unexpected`,
  );
  process.exit(1);
}

let scanned = 0;
let failed = false;

for (const file of walk(CLIENT_DIR)) {
  scanned += 1;
  const raw = readFileSync(file); // byte scan: catches minified + non-UTF-8 output alike
  const bytes = file.endsWith(".js")
    ? Buffer.from(raw.toString("utf8").replace(VENDOR_ENV_GETTER, "<vendor-env-getter>"), "utf8")
    : raw;
  for (const [name, needle] of needles) {
    if (bytes.includes(needle)) {
      failed = true;
      console.error(`FAIL ${relative(WEB_ROOT, file)}: contains "${name}"`);
    }
  }
}

if (scanned === 0) {
  console.error("scan-bundle: nothing scanned — build output layout unexpected");
  process.exit(1);
}
if (failed) {
  process.exit(1);
}
console.warn(`bundle scan passed (${scanned} files clean)`);
