import { betterAuth } from "better-auth";
import { drizzleAdapter } from "better-auth/adapters/drizzle";
import { magicLink } from "better-auth/plugins/magic-link";
import { composition } from "@/composition.server";
import { serverEnv } from "@/env.server";
import { getDb } from "@/lib/db/index.server";

/**
 * The auth construction, parameterized by the composition's AuthProviderConfig (the fourth
 * seam): the OSS default is email+password with zero delivery dependency; magic links and
 * social providers register only when the composition provides them. There is no JWT plugin
 * and no keyset — the app talks to the vault over the internal lane with its own bearer, and
 * a session never becomes a token that leaves this tier.
 */
function buildAuth() {
  const env = serverEnv();
  const providers = composition.auth;
  // With NO out-of-band identity rung configured (the OSS default), there is nothing to verify
  // an address against — possession of the password IS the identity claim on a self-hosted
  // instance (the roster still decides every admission; an uninvited sign-up holds no seat).
  // Stated honestly rather than left as a permanently-false flag that would brick the actor
  // mint: accounts born on the password rung are recorded verified-as-claimed. The moment a
  // composition provides ANY real rung (magic link or a social provider), this hook stays dark
  // and that rung's own verification decides — otherwise a password sign-up could pre-claim an
  // address a social sign-in would later prove.
  const hasSocial =
    providers.socialProviders !== undefined && Object.keys(providers.socialProviders).length > 0;
  const selfAssertedEmails = providers.emailAndPassword && !providers.magicLink && !hasSocial;
  return betterAuth({
    database: drizzleAdapter(getDb(), { provider: "pg" }),
    baseURL: env.BETTER_AUTH_URL,
    secret: env.BETTER_AUTH_SECRET,
    // Better Auth's built-in limiter arms by NODE_ENV, which a served test build sets to
    // production — key it on the app's OWN env so the credential endpoints stay limited in
    // production and the suites' rapid sign-ins don't trip it.
    rateLimit: { enabled: env.APP_ENV === "production" },
    ...(providers.emailAndPassword ? { emailAndPassword: { enabled: true } } : {}),
    ...(selfAssertedEmails
      ? {
          databaseHooks: {
            user: {
              create: {
                before: (user: { email: string }) =>
                  Promise.resolve({ data: { ...user, emailVerified: true } }),
              },
            },
          },
        }
      : {}),
    plugins: providers.magicLink
      ? [
          magicLink({
            sendMagicLink: (args) => composition.auth.magicLink?.send(args) ?? Promise.resolve(),
          }),
        ]
      : [],
    ...(providers.socialProviders && Object.keys(providers.socialProviders).length > 0
      ? { socialProviders: providers.socialProviders }
      : {}),
    // Cookies stay host-only on purpose (no Domain= / cross-subdomain option): the session
    // must never ride to sibling subdomains.
  });
}

export type Auth = ReturnType<typeof buildAuth>;

// Lazy singleton: construction reads env, which is absent during a CI build.
let auth: Auth | undefined;

export function getAuth(): Auth {
  auth ??= buildAuth();
  return auth;
}
