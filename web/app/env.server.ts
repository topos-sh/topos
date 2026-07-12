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
});

export type ServerEnv = z.infer<typeof serverSchema>;

let cached: ServerEnv | undefined;

export function serverEnv(): ServerEnv {
  cached ??= serverSchema.parse(process.env);
  return cached;
}
