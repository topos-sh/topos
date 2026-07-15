import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Shared e2e constants: the ONE place the app-under-test env, ports, and the seeded member
 * identity are defined. playwright.config.ts feeds `appEnv()` to the react-router dev server; the
 * setup project seeds the DB with the same values. Everything here is synthetic.
 */

const HERE = dirname(fileURLToPath(import.meta.url));
// tests/e2e → web → repo root: the in-repo installer + plane migrations live under the repo root.
const REPO_ROOT = resolve(HERE, "..", "..", "..");

export const PLANE_PORT = 8791;
export const APP_PORT = 3100;
/** The fake SMTP sink (tests/fixtures/smtp-sink.mjs) the suite arms TOPOS_MAIL_SMTP_* toward. */
export const SMTP_SINK_PORT = 2598;
// localhost, NOT 127.0.0.1: the vite dev server blocks its own static/HMR resources cross-origin,
// and a 127.0.0.1 page against a localhost-bound dev server never loads its client chunks.
export const BASE_URL = `http://localhost:${APP_PORT}`;

export const MEMBER_EMAIL = "reviewer@example.com";
/** The boot-minted workspace's address slug (appEnv pins TOPOS_WORKSPACE_NAME to it). */
export const WORKSPACE_ADDRESS = "team";
/** The one password every e2e account is created with (email+password auth; better-auth min 8). */
export const E2E_PASSWORD = "e2e-password-1234";
export const STORAGE_STATE = "tests/e2e/.auth/user.json";

/**
 * Presets the first-boot setup claim code (`TOPOS_SETUP_CODE`), so the setup project can claim
 * the boot-minted workspace deterministically — the same stable-code contract CI/IaC rides.
 */
export const E2E_SETUP_CODE = "e2e-setup-code-0123456789";

/** The shared internal bearer the app injects on the vault's internal lane; the fixture REQUIRES
 * it (Bearer) and 404s the whole lane without it, mirroring the real vault. */
export const PLANE_INTERNAL_TOKEN = "e2e-internal-token";

// A SEPARATE database from the unit lane's topos_test (which also uses this server), on the same
// Postgres. topos_web (search_path=web) is the app role; the superuser bootstraps + seeds.
export const E2E_DATABASE_URL =
  process.env.DATABASE_URL ?? "postgres://topos_web:web@localhost:5439/topos_e2e";
/** The superuser URL the destructive bootstrap + row seed use (never the topos_web app URL —
 * SELECT-only on the plane schema by design). */
export const E2E_ADMIN_URL =
  process.env.E2E_ADMIN_URL ?? "postgres://postgres:postgres@localhost:5439/topos_e2e";
/** The server's maintenance database (for CREATE DATABASE topos_e2e). */
export const E2E_MAINTENANCE_URL =
  process.env.E2E_MAINTENANCE_URL ?? "postgres://postgres:postgres@localhost:5439/postgres";

/** The in-repo plane SQL migrations — the tests' DDL source (no vendoring). */
export const PLANE_MIGRATIONS_DIR = resolve(REPO_ROOT, "crates", "plane-store", "migrations");
/** The in-repo checksummed installer the /install route serves. */
export const INSTALL_SH_PATH = resolve(REPO_ROOT, "scripts", "install.sh");

export function appEnv(): Record<string, string> {
  return {
    DATABASE_URL: E2E_DATABASE_URL,
    BETTER_AUTH_SECRET: "e2e-only-secret-0123456789abcdef0123456789abcdef",
    BETTER_AUTH_URL: BASE_URL,
    PLANE_INTERNAL_URL: `http://127.0.0.1:${PLANE_PORT}`,
    PLANE_INTERNAL_TOKEN,
    INSTALL_SH_PATH,
    APP_ENV: "test",
    TOPOS_SETUP_CODE: E2E_SETUP_CODE,
    TOPOS_WORKSPACE_NAME: WORKSPACE_ADDRESS,
    // The suite runs MAIL-ARMED: all five TOPOS_MAIL_SMTP_* point at the local sink
    // (tests/fixtures/smtp-sink.mjs), so `canSend` is true product-wide — the invite form is
    // enabled, sign-ups send verification mail, and invited seats bind through the mailbox
    // round-trip. The assertable copies land in .outbox.jsonl (APP_ENV=test records always).
    TOPOS_MAIL_SMTP_HOST: "127.0.0.1",
    TOPOS_MAIL_SMTP_PORT: String(SMTP_SINK_PORT),
    TOPOS_MAIL_SMTP_USER: "sink",
    TOPOS_MAIL_SMTP_PASS: "sink",
    TOPOS_MAIL_SMTP_FROM: "Topos E2E <topos@e2e.test>",
  };
}
