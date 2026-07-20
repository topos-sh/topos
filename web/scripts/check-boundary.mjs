#!/usr/bin/env node
/**
 * check-boundary.mjs — the source-level trust-boundary gate over app/.
 *
 * The web tier is the authority for identity and policy, but it still computes no digest and
 * holds no signing machinery: hashing happens IN Postgres (sha256 over presented secrets) or
 * inside better-auth's own password hasher — never in this tier's code. It keeps a fail-closed
 * actor model (guards mint actors; the DAL requires them), one vault transport, and a
 * zero-client-env stance. This script makes all of that executable:
 *
 *  1. Hard-zero crypto/signing identifiers anywhere under app/: ed25519, private key,
 *     createHash, createSign, createHmac, crypto.subtle, `sign(` as a word, the cipher
 *     primitives, `digest(` computation, and `sha256` — with TWO carve-outs: the Postgres-side
 *     hashing spelling `sha256(convert_to(` (SQL text, not a TS call) and the stored-hash
 *     COLUMN identifiers (`*_sha256` / `*Sha256`). `bundle_digest`/`bundleDigest` stay allowed —
 *     they DISPLAY the vault's recorded consent value.
 *     A THIRD carve-out is ONE EXACT EXPRESSION in ONE named module:
 *     app/lib/agent-skills.server.ts may spell the advertised-digest template
 *     `sha256:${createHash("sha256").update(<ident>).digest("hex")}` plus the bare createHash
 *     import that feeds it — ONCE each. It computes the sha256 the agent-skills discovery
 *     index ADVERTISES for the public skill bytes this same process serves; no secret is
 *     hashed and nothing signs (the digest exists for the READER to verify). Anything past
 *     the pinned form — a second call site, another algorithm, a userland sha256, hmac/sign/
 *     subtle — fails there too.
 *  2. The (node:)crypto module specifier and `randomBytes` are allowed ONLY in
 *     app/lib/db/identity.server.ts and app/lib/auth/recovery.server.ts — the two mints of
 *     random SECRETS/ids (randomness is this tier's; hashing stays in Postgres/better-auth) —
 *     plus the specifier (never randomBytes) in the public-digest module above.
 *     No process.getBuiltinModule escape hatch anywhere.
 *  3. Transport containment: PLANE_INTERNAL_URL only in app/env.server.ts +
 *     app/lib/plane/client.server.ts; `fetch(` inside app/lib/plane/ only in client.server.ts;
 *     the `/internal/v1` custody-lane spelling and `vaultFetch(` calls confined to
 *     app/lib/plane/ — every vault byte goes through the one allowlisted transport.
 *  4. The dead acting-identity header may not be spelled: `x-topos-acting-email` is GONE (the
 *     vault is identity-free; authorization happens in this tier's guards + rows).
 *  5. Every server-tier module under app/lib that imports app/env.server or the raw pg/drizzle
 *     driver must carry the `.server` suffix. Exempt: the value-only Drizzle schema files and
 *     the named auth entry.
 *  6. Every route module under app/routes/ that reads data (exports a loader/action) calls a
 *     require* guard — unless it is on the explicit sessionless allowlist.
 *  7. The raw DB surface (drizzle-orm, app/lib/db/schema*, app/lib/db/index.server) may be
 *     imported ONLY inside app/lib/db/**, the guards, the auth entry, and the healthz probe.
 *  8. Zero client env: no `VITE_` prefix and no custom `import.meta.env` read anywhere.
 *
 * Self-test: `node scripts/check-boundary.mjs <dir>` scans <dir> as if it were app/ (the red
 * test drives a fixture tree with planted violations through the same rules).
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";

const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const APP_OVERRIDE = process.argv[2];
const APP = APP_OVERRIDE ? resolve(APP_OVERRIDE) : join(WEB_ROOT, "app");
/** rel paths are always spelled app/… so the per-file rules hold under a fixture override. */
const REL_ROOT = APP_OVERRIDE ? dirname(APP) : WEB_ROOT;

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

const SCHEMA_DTS = join("app", "lib", "plane", "contract", "schema.d.ts");

const files = [];
for (const full of walk(APP)) {
  if (!SOURCE_EXTENSIONS.has(extensionOf(full))) {
    continue;
  }
  const rel = APP_OVERRIDE ? join("app", relative(APP, full)) : relative(REL_ROOT, full);
  if (rel === SCHEMA_DTS) {
    continue; // generated contract types — gated by regen-clean instead
  }
  files.push({ rel, text: readFileSync(full, "utf8"), base: rel.slice(rel.lastIndexOf(sep) + 1) });
}

// 1. hard-zero crypto/signing identifiers (with the documented carve-outs)
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
];
/** The two random-secret mints — the ONLY homes of node:crypto randomness. */
const CRYPTO_MINT_ALLOWED = new Set([
  join("app", "lib", "db", "identity.server.ts"),
  join("app", "lib", "auth", "recovery.server.ts"),
]);
/**
 * The ONE sanctioned public-bytes digest: the agent-skills discovery module hashes the built-in
 * skill's PUBLIC bytes — the same bytes this process serves under /.well-known/agent-skills — so
 * the index it advertises cannot drift from what is served. No secret, no signing. The carve-out
 * is the EXACT expression, not the spellings: the single advertised-digest template plus the bare
 * createHash import that feeds it, each stripped ONCE before the scan — so a second call site,
 * a different algorithm (createHash("sha1")), a userland sha256, or any other crypto identifier
 * (hmac/sign/subtle/ed25519) still fails in this module too.
 */
const PUBLIC_DIGEST_ALLOWED = new Set([join("app", "lib", "agent-skills.server.ts")]);
const SANCTIONED_DIGEST_EXPR =
  /`sha256:\$\{createHash\("sha256"\)\.update\([A-Za-z_$][\w$]*\)\.digest\("hex"\)\}`/;
const SANCTIONED_DIGEST_IMPORT = 'import { createHash } from "node:crypto";';
for (const { rel, text } of files) {
  // The carve-outs, stripped before scanning:
  //  - bundle_digest/bundleDigest DISPLAY the vault's recorded consent value;
  //  - `sha256(convert_to(` is the POSTGRES hashing spelling inside SQL text (TS has no such
  //    function — presented secrets are hashed in the database, never here);
  //  - `*_sha256`/`*Sha256` name the STORED-HASH columns those statements compare against.
  let carved = text
    .replaceAll(/bundle_digest|bundleDigest/g, "")
    .replaceAll(/sha256\(convert_to\(/g, "")
    // The identity mint's named builder of that SQL fragment (still zero TS hashing).
    .replaceAll(/\bsha256OfText\b/g, "")
    // Stored-hash COLUMN/CHECK identifiers (snake `…_sha256…`, camel `…Sha256…`). A BARE
    // `sha256` identifier — a TS-side hash call — is deliberately not covered and still fails.
    .replaceAll(/[A-Za-z0-9_]+_sha256[A-Za-z0-9_]*/g, "")
    .replaceAll(/[A-Za-z0-9_]*Sha256[A-Za-z0-9_]*/g, "");
  if (PUBLIC_DIGEST_ALLOWED.has(rel)) {
    // Strip the FIRST occurrence of each sanctioned expression only (non-global `replace`) —
    // a second spelling of either is a violation and stays in the scanned text, so the full
    // rule set below still fires on it.
    carved = carved.replace(SANCTIONED_DIGEST_EXPR, "").replace(SANCTIONED_DIGEST_IMPORT, "");
  }
  for (const [regex, name] of HARD_ZERO) {
    if (regex.test(carved)) {
      fail(rel, `forbidden identifier: ${name}`);
    }
  }
  // Ban DIGEST COMPUTATION (a `digest(` / `.digest(` finalizer call), not the word — which
  // legitimately names the displayed `bundle_digest`/`bundleDigest` value and appears in prose.
  if (/\bdigest\s*\(/i.test(carved)) {
    fail(
      rel,
      "forbidden: a digest( computation (only bundle_digest/bundleDigest display is allowed)",
    );
  }
  if (/\brandomBytes\b/.test(text) && !CRYPTO_MINT_ALLOWED.has(rel)) {
    fail(rel, "randomBytes outside the two random-secret mints (identity/recovery .server.ts)");
  }
}

// 2. the (node:)crypto module specifier confined to the two mints; no builtin escape hatch
for (const { rel, text } of files) {
  if (
    /["'`](?:node:)?crypto["'`]/.test(text) &&
    !CRYPTO_MINT_ALLOWED.has(rel) &&
    !PUBLIC_DIGEST_ALLOWED.has(rel)
  ) {
    fail(rel, "a (node:)crypto module specifier outside the two random-secret mints");
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
  if (!rel.startsWith(PLANE_DIR) && text.includes("/internal/v1")) {
    fail(rel, "the /internal/v1 custody lane spelled outside app/lib/plane/");
  }
  if (!rel.startsWith(PLANE_DIR) && /\bvaultFetch\s*\(/.test(text)) {
    fail(rel, "vaultFetch( called outside app/lib/plane/ — go through the typed wrappers");
  }
}

// 4. the dead acting-identity header may not come back
for (const { rel, text } of files) {
  if (/x-topos-acting-email/i.test(text)) {
    fail(rel, "the retired x-topos-acting-email header — the vault is identity-free now");
  }
}

// 5. server-tier modules that hold env/db must carry the `.server` suffix
const SERVER_SUFFIX_EXEMPT = new Set([
  // Value-only Drizzle table defs: no secret, no env, no query capability — safe in any bundle,
  // and drizzle-kit's CLI loads them in plain node (outside the react-server condition).
  join("app", "lib", "db", "schema.ts"),
  join("app", "lib", "db", "schema.app.ts"),
  join("app", "lib", "db", "schema.auth.ts"),
  join("app", "lib", "db", "schema.custody.ts"),
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
  // The first-boot claim + the mail-less recovery hatch: sessionless BY DESIGN (the code IS the
  // proof — machine control), uniform-404 on a miss, public-read belted.
  "claim",
  "recovery",
  "healthz",
  "install",
  // The `.sh` alias (same loader as /install) + the public agent-onboarding document: both
  // constant public bytes, sessionless by design.
  "install-sh",
  "agent",
  // The machine-discovery lane: llms.txt + the agent-skills discovery index, its legacy
  // spelling, and the built-in skill's files — all constant public bytes, sessionless by design.
  "llms-txt",
  "agent-skills-index",
  "agent-skills-index-legacy",
  "agent-skills-file",
  "api.auth",
  "api.memberships",
  // The FACE layout tolerates anonymous (the constant teaser / landing renders with no session);
  // its chrome loads only when a session is present, so it resolves the session itself rather than
  // guarding. The three face MODULES (workspace-dashboard, skill-current, channel-detail) DO call
  // require* on their signed-in arm, so they are NOT here.
  "face-shell",
  // The reserved-segment stub (multi-tenant `claim`): a loader that only answers the house 404.
  "reserved",
  // The fallback: anonymous is a VALID state (the constant protocol card / house 404 — no
  // existence oracle), so the guard family cannot front it.
  "catch-all",
  // The device flow's unauthenticated start + poll: no credential EXISTS yet (approval mints
  // it); the belt is their gate and the flow rows are single-use, short-TTL.
  "api.v1.device-authorize",
  "api.v1.device-token",
  // The lane's catch-all answers the constant uniform 404 — it reads nothing.
  "api.v1.$",
]);
// The guard family: the request-level require* mints plus memberInScope — the one
// membership-or-404 resolution the face modules call on their signed-in arm (their anonymous
// arm resolves the session itself, teaser-or-404, so the require* wrappers cannot front them).
const GUARD_CALL =
  /\b(?:require(?:Session|MemberInScope|Member|OwnerInScope|WorkspaceOwner|Reviewer|DeviceActor)|memberInScope)\s*\(/;
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
  // The two auth ceremonies whose sanctioned email LOOKUPS the email-authz gate allowlists —
  // their reads are the design (registration proof; the operator recovery hatch).
  join("app", "lib", "auth", "registration.server.ts"),
  join("app", "lib", "auth", "recovery.server.ts"),
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

// 9. no `/workspaces`-rooted URL literal anywhere under app/ — the signed-in surface is
// origin-rooted (single tenancy) or `/:ws`-slug-nested (multi), built through app/lib/ws-path.ts.
// The device lane's `/v1/workspaces/…` / `/internal/v1/workspaces/…` / `/api/v1/workspaces/…`
// spellings are unaffected: they don't START with `/workspaces` (a quote/backtick precedes the
// prefix, not `/workspaces`). Zero allowlist entries.
const WORKSPACES_URL_LITERAL = /["'`]\/workspaces/;
for (const { rel, text } of files) {
  if (WORKSPACES_URL_LITERAL.test(text)) {
    fail(
      rel,
      "a `/workspaces`-rooted URL literal — build workspace paths through app/lib/ws-path.ts",
    );
  }
}

if (failed) {
  process.exit(1);
}
console.warn(`boundary check passed (${files.length} files)`);
