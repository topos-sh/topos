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
});

export type ServerEnv = z.infer<typeof serverSchema>;

let cached: ServerEnv | undefined;

export function serverEnv(): ServerEnv {
  cached ??= serverSchema.parse(process.env);
  return cached;
}
