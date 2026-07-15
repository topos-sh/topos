#!/usr/bin/env node
/**
 * check-email-authz.mjs — the ONE-IDENTITY gate: nothing in app/ may AUTHORIZE by email
 * equality. `user.id` is THE identity; email is a mutable attribute and a login name — the
 * moment code compares an email to decide anything, an address change (or a lookalike fold)
 * becomes an authority event, which is exactly the bug class the identity unification killed.
 *
 * Banned everywhere (file:line reported):
 *  - `normalizeEmail` / `PRINTABLE_ASCII` — the retired email-canonicalization defenses; their
 *    reappearance means someone is comparing emails again;
 *  - `email ===` / `email ==` — a TS email-equality branch;
 *  - `eq(<table>.email` — a Drizzle email-equality predicate;
 *  - `email = ${` — a SQL-template email-equality predicate.
 *
 * The explicit allowlist — the three places an email LOOKUP is the design, not a comparison of
 * authority, plus the auth schema's own tripwire CHECKs:
 *  - app/lib/auth/registration.server.ts — the invitation lookup deciding sign-UP admission
 *    (the mailbox round-trip is the proof; no session exists yet);
 *  - app/lib/db/identity.server.ts — bindInvitedSeats, converting a VERIFIED address's pending
 *    invitations into seats (the one place an invitation becomes admission);
 *  - app/lib/auth/recovery.server.ts — the operator hatch's user lookup (machine control is
 *    the proof);
 *  - app/lib/db/schema.auth.ts + schema.app.ts — Better Auth's own tables + the lowercase
 *    CHECK tripwires (DDL, not authorization).
 *
 * Self-test: `node scripts/check-email-authz.mjs <dir>` scans <dir> as if it were app/.
 */
import { readdirSync, readFileSync, statSync } from "node:fs";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const WEB_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const APP_OVERRIDE = process.argv[2];
const APP = APP_OVERRIDE ? resolve(APP_OVERRIDE) : join(WEB_ROOT, "app");

const SOURCE_EXTENSIONS = new Set([".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs"]);

const ALLOWLIST = new Set([
  join("app", "lib", "auth", "registration.server.ts"),
  join("app", "lib", "db", "identity.server.ts"),
  join("app", "lib", "auth", "recovery.server.ts"),
  join("app", "lib", "db", "schema.auth.ts"),
  join("app", "lib", "db", "schema.app.ts"),
]);

/** [pattern, message]; every pattern is scanned per line so the report carries file:line. */
const BANNED = [
  [/\bnormalizeEmail\b/, "normalizeEmail — the retired email-canonicalization defense"],
  [/\bPRINTABLE_ASCII\b/, "PRINTABLE_ASCII — the retired email-charset defense"],
  [/\bemail\s*={2,3}/, "an email equality branch (email === …)"],
  [
    /\beq\(\s*[A-Za-z_$][\w$.]*\.email\b/,
    "a Drizzle email-equality predicate (eq(<table>.email …)",
  ],
  // biome-ignore lint/suspicious/noTemplateCurlyInString: the ${ IS the pattern under scan.
  [/\bemail\s*=\s*\$\{/, "a SQL email-equality predicate (email = ${...})"],
];

function* walk(dir) {
  let entries;
  try {
    entries = readdirSync(dir);
  } catch {
    return;
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
let scanned = 0;
for (const full of walk(APP)) {
  if (!SOURCE_EXTENSIONS.has(extensionOf(full))) {
    continue;
  }
  const rel = APP_OVERRIDE ? join("app", relative(APP, full)) : relative(WEB_ROOT, full);
  scanned += 1;
  if (ALLOWLIST.has(rel)) {
    continue;
  }
  const lines = readFileSync(full, "utf8").split("\n");
  for (let i = 0; i < lines.length; i += 1) {
    for (const [pattern, message] of BANNED) {
      if (pattern.test(lines[i])) {
        failed = true;
        console.error(`FAIL ${rel}:${i + 1}: ${message}`);
      }
    }
  }
}

if (failed) {
  process.exit(1);
}
console.warn(`email-authorization check passed (${scanned} files)`);
