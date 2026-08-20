import { z } from "zod";

/**
 * The server-tier environment — parsed LAZILY (deploy environments inject env at runtime; a CI
 * build runs without production secrets, so a top-level parse would fail the build). Every
 * secret and every vault-facing value lives here and ONLY here; nothing in this module may
 * reach the client bundle (enforced by the built-bundle scan). There is deliberately NO
 * client-side env: the app ships zero `VITE_`-prefixed values.
 */
const serverSchema = z.object({
  DATABASE_URL: z.string().min(1),
  BETTER_AUTH_SECRET: z.string().min(32),
  BETTER_AUTH_URL: z.url(),
  /** The vault, over the internal network — never a public URL requirement. */
  PLANE_INTERNAL_URL: z.url(),
  /**
   * The shared bearer for the vault's internal session lane. The app is the only holder; the
   * vault answers 404 on the whole lane when its side is unset.
   */
  PLANE_INTERNAL_TOKEN: z.string().min(1),
  /** Path the /install route serves; defaults to the repo's own installer. */
  INSTALL_SH_PATH: z.string().default("../scripts/install.sh"),
  /**
   * Path the /compose.yml route serves — the self-host deployment file; defaults to the repo's
   * own `docker-compose.yml` (resolved, like INSTALL_SH_PATH, against the process working
   * directory — an absolute path wins).
   */
  COMPOSE_YML_PATH: z.string().default("../docker-compose.yml"),
  /**
   * Path the /compose-init-db.sh route serves — the database's first-boot provisioning script,
   * the second file a self-host install fetches; defaults to the repo's own copy.
   */
  COMPOSE_INIT_DB_PATH: z.string().default("../scripts/compose-init-db.sh"),
  /**
   * Directory the /.well-known/agent-skills routes serve the built-in `topos` skill from;
   * defaults to the repo's own source (resolved, like INSTALL_SH_PATH, against the process
   * working directory — an absolute path wins).
   */
  BUILTIN_SKILL_DIR: z.string().default("../skills/topos"),
  APP_ENV: z.enum(["production", "development", "test"]).default("development"),
  /** The `/api/v1` door's rate belt (the vault's own belt retired with its public listener). */
  TOPOS_WEB_RATELIMIT: z.enum(["on", "off"]).default("on"),
  /**
   * The app's PUBLIC origin — the base every client-visible URL rides (resource addresses, the
   * protocol card's `api_base_url` = this origin + `/api`, the invite/share lines) and the
   * canonical host a browser on an alias origin is redirected to. Behind a TLS-terminating
   * reverse proxy this MUST be set: the container speaks plain HTTP, so a request-derived origin
   * would be `http://…` and the CLI refuses to re-root an https link onto an http base.
   *
   * Unset — or EMPTY, which is how compose and every deploy panel spell "unset" — the app falls
   * back to the request's own origin, which is correct for a same-origin deployment and is what
   * keeps a bare `docker compose up` reachable from another machine on the LAN (a hard-coded
   * localhost default would make the canonical redirect bounce every remote browser home).
   */
  TOPOS_PUBLIC_URL: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.url().optional(),
  ),
  /**
   * The first-boot workspace's address slug (renameable later in the product). The workspace
   * row is born at boot; this only names it.
   */
  TOPOS_WORKSPACE_NAME: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z
      .string()
      .regex(/^[a-z0-9][a-z0-9-]*$/)
      .max(100)
      .default("team"),
  ),
  /**
   * Presets the setup claim code (CI/IaC). Unset, a high-entropy code is minted fresh on
   * every boot while the workspace is unclaimed. Only the SHA-256 is ever stored.
   */
  TOPOS_SETUP_CODE: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.string().min(16).optional(),
  ),
  /**
   * Optional file path the printed setup line is ALSO written to (a compose volume makes it
   * readable without log access). Unset ⇒ logs only.
   */
  TOPOS_SETUP_LINK_FILE: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.string().optional(),
  ),
  /**
   * Optional Google Tag Manager container id. Set, the HTML shell (root.tsx) carries the
   * standard GTM head snippet + body noscript on every document — how a hosted deployment wires
   * its analytics without forking the shell. Unset — the OSS default — the app ships ZERO
   * third-party script. Empty spells unset, how compose and every deploy panel spell it; the
   * container-id shape is enforced so a malformed value can never reach the inline snippet.
   */
  TOPOS_GTM_CONTAINER_ID: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z
      .string()
      .regex(/^GTM-[A-Z0-9]+$/)
      .optional(),
  ),
  /**
   * Optional build identifier `/healthz` reports (a deployment stamps its image's revision in,
   * so deploy tooling can tell old from new during a rolling replacement — "the site answers
   * 200" no longer proves which build answered). Unset — the OSS default — the field is simply
   * absent from the health body. Empty spells unset.
   */
  TOPOS_BUILD: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.string().max(200).optional(),
  ),
  /**
   * The outbound-mail relay — BRING YOUR OWN SMTP, all five or none. With all five set, the
   * app's ONE mail seam really sends: invitations, address verification, password reset, and a
   * composition's magic links. Any missing ⇒ mail is off, and every flow that would have mailed
   * refuses in a constant shape rather than half-working (an invitation is refused typed, so no
   * row is written promising a mail nobody will get). Empty spells unset — how compose and every
   * deploy panel spell it. The user/pass/from are credentials-adjacent: they live here and never
   * in a log or an error (the transport throws coarse).
   */
  TOPOS_MAIL_SMTP_HOST: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.string().optional(),
  ),
  TOPOS_MAIL_SMTP_PORT: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.coerce.number().int().min(1).max(65535).optional(),
  ),
  TOPOS_MAIL_SMTP_USER: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.string().optional(),
  ),
  TOPOS_MAIL_SMTP_PASS: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.string().optional(),
  ),
  TOPOS_MAIL_SMTP_FROM: z.preprocess(
    (v) => (typeof v === "string" && v.trim() === "" ? undefined : v),
    z.string().optional(),
  ),
});

export type ServerEnv = z.infer<typeof serverSchema>;

/**
 * Secrets `docker-compose.yml` once defaulted, so that a first `up` needed zero setup. It no
 * longer does — the two are required at parse time now, and `.env.example` carries them blank —
 * but these exact strings stay refused FOREVER: they sit in this repository's public git history,
 * so the auth secret is a session-forging key anyone can look up, and a deployment that copied an
 * older compose file (or an older `.env`) forward must not quietly keep running on one.
 */
const SHIPPED_DEFAULTS: ReadonlyArray<[keyof ServerEnv, string]> = [
  ["BETTER_AUTH_SECRET", "change-me-auth-secret-change-me-auth-secret"],
  ["PLANE_INTERNAL_TOKEN", "change-me-internal-token"],
];

/**
 * Refuse the once-published defaults in production. Compose now refuses to even parse without
 * real values, so this is the BACKSTOP for every other path to a running app — a hand-written
 * unit file, a PaaS env panel, an old compose file kept around. Scoped to
 * `APP_ENV === "production"` on purpose: a dev tree may hold whatever placeholder it likes, and
 * compose is what sets `APP_ENV: production`.
 */
function assertNoShippedDefaults(env: ServerEnv): void {
  if (env.APP_ENV !== "production") {
    return;
  }
  const offenders = SHIPPED_DEFAULTS.filter(([key, value]) => env[key] === value).map(
    ([key]) => key,
  );
  if (offenders.length > 0) {
    throw new Error(
      `refusing to start: ${offenders.join(", ")} still hold${offenders.length === 1 ? "s" : ""} a value this project once shipped as a default. ` +
        "These are public values — set real secrets in .env (see .env.example) before exposing this deployment.",
    );
  }
}

let cached: ServerEnv | undefined;

export function serverEnv(): ServerEnv {
  if (cached === undefined) {
    const parsed = serverSchema.parse(process.env);
    assertNoShippedDefaults(parsed);
    cached = parsed;
  }
  return cached;
}
