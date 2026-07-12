#!/usr/bin/env node
/**
 * check-boundary.mjs — the source-level trust-boundary gate over app/.
 *
 * The web tier is surfaces only: it holds no signing key, computes no digest, and makes no
 * device-signed write. It also keeps a fail-closed actor model (guards mint actors; the DAL
 * requires them) and a zero-client-env stance. This script makes all of that executable:
 *
 *  1. Hard-zero crypto/signing identifiers anywhere under app/ (generated schema.d.ts excluded):
 *     ed25519, private key, createHash, createSign, createHmac, sha256, crypto.subtle, `sign(`
 *     as a word, the cipher primitives, and `digest`. The ONE carve-out: `bundle_digest` /
 *     `bundleDigest` are allowed — they DISPLAY the vault's recorded consent value; computing any
 *     digest stays forbidden absolutely.
 *  2. No (node:)crypto module specifier anywhere under app/ — any quote style, including a
 *     backtick template literal — and no process.getBuiltinModule escape hatch. (`crypto.randomUUID`
 *     is the Web Crypto GLOBAL, not an import — unaffected.)
 *  3. PLANE_INTERNAL_URL may appear ONLY in app/env.server.ts + app/lib/plane/client.server.ts;
 *     inside app/lib/plane/ only client.server.ts may call fetch( — every vault byte goes through
 *     the one allowlisted transport.
 *  4. No DEVICE-signed write path may even be spelled: `/v1/publish`, `/v1/proposals`,
 *     `/v1/reviews`, `/v1/reverts` as a request path, anywhere. (Those ops stay on the enrolled
 *     device; the web tier's writes ride the vault's internal session lane.)
 *  5. Every server-tier module under app/lib that imports app/env.server or the raw pg/drizzle
 *     driver must carry the `.server` suffix (React Router's server-module exclusion keys on it),
 *     so a value-bearing server module can never reach the client bundle. Exempt: the value-only
 *     Drizzle schema files (loaded by drizzle-kit in plain node) and the named auth entry.
 *  6. Every route module under app/routes/ that reads data (exports a loader/action) calls a
 *     require* guard — unless it is on the explicit sessionless allowlist. The shell's cookie
 *     bounce is optimistic UX only; authorization lives in the route's own guard.
 *  7. The raw DB surface (drizzle-orm, app/lib/db/schema*, app/lib/db/index.server) may be
 *     imported ONLY inside app/lib/db/**, the guards, the auth entry, and the healthz probe —
 *     everything else goes through the actor-requiring DAL (app/lib/db/queries.server.ts).
 *  8. Zero client env: no `VITE_` prefix and no custom `import.meta.env` read anywhere under app/.
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const APP = join(WEB_ROOT, "app");
const SCHEMA_DTS = join("app", "lib", "plane", "contract", "schema.d.ts");

const SOURCE_EXTENSIONS = new Set([".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs"]);
/** React Router treats a module as server-only iff its filename matches this. */
const SERVER_SUFFIX = /\.server\.(?:[cm]?[jt]sx?)$/;

function* walk(dir) {
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return; // app/routes may not exist yet in a partially-built tree
  }
  for (const entry of entries) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      yield* walk(full);
    } else {
      yield full;
    }
  }
}

function extensionOf(file) {
  const dot = file.lastIndexOf(".");
  return dot === -1 ? "" : file.slice(dot);
}

let failed = false;
function fail(file, message) {
  failed = true;
  console.error(`FAIL ${file}: ${message}`);
}

const files = [];
for (const full of walk(APP)) {
  if (!SOURCE_EXTENSIONS.has(extensionOf(full))) {
    continue;
  }
  const rel = relative(WEB_ROOT, full);
  if (rel === SCHEMA_DTS) {
    continue; // generated contract types — gated by regen-clean instead
  }
  files.push({ rel, text: readFileSync(full, "utf8"), base: rel.slice(rel.lastIndexOf(sep) + 1) });
}

// 1. hard-zero crypto/signing identifiers (with the documented bundle_digest display carve-out)
const HARD_ZERO = [
  [/ed25519/i, "ed25519"],
  [/private[_-]?key/i, "private key"],
  [/createHash/i, "createHash"],
  [/createSign/i, "createSign"],
  [/createHmac/i, "createHmac"],
  [/sha256/i, "sha256"],
  [/crypto\.subtle/i, "crypto.subtle"],
  [/\bsign\(/, "sign("],
  [/\bcreateCipheriv\b/, "createCipheriv"],
  [/\bcreateDecipheriv\b/, "createDecipheriv"],
  [/\brandomBytes\b/, "randomBytes"],
];
for (const { rel, text } of files) {
  // the display carve-out: strip the exact allowed identifiers before scanning for `digest`
  const carved = text.replaceAll(/bundle_digest|bundleDigest/g, "");
  for (const [regex, name] of HARD_ZERO) {
    if (regex.test(carved)) {
      fail(rel, `forbidden identifier: ${name}`);
    }
  }
  // Ban DIGEST COMPUTATION (a `digest(` / `.digest(` finalizer call), not the word — which
  // legitimately names the displayed `bundle_digest`/`bundleDigest` value and appears in prose
  // ("commit ids, digests"). Computing a digest still needs a banned primitive (createHash …).
  if (/\bdigest\s*\(/i.test(carved)) {
    fail(
      rel,
      "forbidden: a digest( computation (only bundle_digest/bundleDigest display is allowed)",
    );
  }
}

// 2. HARD ZERO: no (node:)crypto module specifier + no getBuiltinModule escape hatch
for (const { rel, text } of files) {
  if (/["'`](?:node:)?crypto["'`]/.test(text)) {
    fail(rel, "a (node:)crypto module specifier — the web tier has no crypto, full stop");
  }
  if (/\bgetBuiltinModule\b/.test(text)) {
    fail(rel, "process.getBuiltinModule — the import-free path into node builtins is forbidden");
  }
}

// 3. transport containment
const PLANE_URL_ALLOWED = new Set([
  join("app", "env.server.ts"),
  join("app", "lib", "plane", "client.server.ts"),
]);
const CLIENT_SERVER = join("app", "lib", "plane", "client.server.ts");
const PLANE_DIR = join("app", "lib", "plane") + sep;
for (const { rel, text } of files) {
  if (text.includes("PLANE_INTERNAL_URL") && !PLANE_URL_ALLOWED.has(rel)) {
    fail(rel, "PLANE_INTERNAL_URL outside app/env.server.ts + app/lib/plane/client.server.ts");
  }
  if (rel.startsWith(PLANE_DIR) && rel !== CLIENT_SERVER && /\bfetch\s*\(/.test(text)) {
    fail(rel, "fetch( inside app/lib/plane/ outside client.server.ts");
  }
}

// 4. no DEVICE-signed write path, anywhere
const DEVICE_WRITE_PATH = /\/v1\/(?:publish|proposals|reviews|reverts)\b/;
for (const { rel, text } of files) {
  if (DEVICE_WRITE_PATH.test(text)) {
    fail(rel, "a device-signed write path (/v1/publish|proposals|reviews|reverts) — never here");
  }
}

// 5. server-tier modules that hold env/db must carry the `.server` suffix
const SERVER_SUFFIX_EXEMPT = new Set([
  // Value-only Drizzle table defs: no secret, no env, no query capability — safe in any bundle,
  // and drizzle-kit's CLI loads them in plain node (outside the react-server condition).
  join("app", "lib", "db", "schema.ts"),
  join("app", "lib", "db", "schema.app.ts"),
  join("app", "lib", "db", "schema.auth.ts"),
  join("app", "lib", "db", "schema.plane.ts"),
  // The named auth entry (imported only by server modules; also allowlisted in rule 7 below).
  join("app", "lib", "auth", "server.ts"),
]);
const LIB_DIR = join("app", "lib") + sep;
const HOLDS_ENV_OR_DRIVER = /from\s+["'](?:[^"']*env\.server|pg|drizzle-orm(?:\/[^"']*)?)["']/;
for (const { rel, text } of files) {
  if (!rel.startsWith(LIB_DIR) || SERVER_SUFFIX_EXEMPT.has(rel)) {
    continue;
  }
  if (HOLDS_ENV_OR_DRIVER.test(text) && !SERVER_SUFFIX.test(rel)) {
    fail(
      rel,
      "server-tier module (imports env.server or pg/drizzle) must carry the .server suffix",
    );
  }
}

// 6. every data-reading route guards — unless explicitly sessionless
const ROUTES_DIR = join("app", "routes") + sep;
// The sessionless allowlist (no guard required): the public routes + the memberships API, which
// answers its own 401. Every OTHER route that reads data must call a require* guard.
const SESSIONLESS_ROUTES = new Set([
  "landing",
  "login",
  "verify",
  "healthz",
  "install",
  "claim-link",
  "api.auth",
  "redirect-create",
  "redirect-link",
  "api.memberships",
  // The resource addresses + the fallback: anonymous is a VALID state there (the constant
  // protocol card / teaser — no existence oracle), so the guard family cannot front them;
  // their signed-in arm resolves the session itself and keeps the house 404 posture.
  "resource-workspace",
  "resource-channel",
  "resource-skill",
  "catch-all",
]);
const GUARD_CALL = /\brequire(?:Session|Member|WorkspaceOwner|Reviewer)\s*\(/;
const READS_DATA = /export\s+(?:async\s+)?(?:function|const)\s+(?:loader|action)\b/;
for (const { rel, text, base } of files) {
  if (!rel.startsWith(ROUTES_DIR)) {
    continue;
  }
  const name = base.replace(/\.[^.]+$/, "");
  if (SESSIONLESS_ROUTES.has(name)) {
    continue;
  }
  if (READS_DATA.test(text) && !GUARD_CALL.test(text)) {
    fail(rel, "route reads data (loader/action) without an auth guard — 404-not-403 lives here");
  }
}

// 7. raw DB surface contained to the DAL + named infrastructure
const DB_SURFACE_ALLOWED = new Set([
  join("app", "lib", "auth", "guards.server.ts"),
  join("app", "lib", "auth", "server.ts"),
  join("app", "routes", "healthz.ts"),
]);
const DB_DIR = join("app", "lib", "db") + sep;
const DB_SURFACE =
  /from\s+["'](?:drizzle-orm(?:\/[^"']*)?|@\/lib\/db\/(?:schema(?:\.[^"']*)?|index\.server))["']/;
for (const { rel, text } of files) {
  if (rel.startsWith(DB_DIR) || DB_SURFACE_ALLOWED.has(rel)) {
    continue;
  }
  if (DB_SURFACE.test(text)) {
    fail(
      rel,
      "raw DB import (drizzle-orm/schema/index.server) outside the DAL — go through @/lib/db/queries.server",
    );
  }
}

// 8. zero client env: no VITE_ prefix, no custom import.meta.env read
const VITE_BUILTINS = new Set(["MODE", "DEV", "PROD", "SSR", "BASE_URL"]);
for (const { rel, text } of files) {
  // A real client-env var is `VITE_<NAME>` (a word char follows the prefix); this skips the
  // bare `VITE_` that prose uses to DESCRIBE the zero-client-env stance ("no `VITE_` values").
  if (/\bVITE_\w/.test(text)) {
    fail(rel, "a VITE_ client-env identifier — this app ships zero client env");
  }
  for (const match of text.matchAll(/import\.meta\.env\s*(?:\.\s*([A-Za-z_$][\w$]*)|\[)/g)) {
    const prop = match[1];
    if (prop === undefined || !VITE_BUILTINS.has(prop)) {
      fail(rel, "a custom import.meta.env read — no client env is exposed to the browser");
      break;
    }
  }
}

if (failed) {
  process.exit(1);
}
console.warn(`boundary check passed (${files.length} files)`);
