import { betterAuth } from "better-auth";
import { drizzleAdapter } from "better-auth/adapters/drizzle";
import { verifyPassword } from "better-auth/crypto";
import { magicLink } from "better-auth/plugins/magic-link";
import { and, eq } from "drizzle-orm";
import { composition } from "@/composition.server";
import { serverEnv } from "@/env.server";
import { bindInvitedSeats } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { account } from "@/lib/db/schema.auth";
import { sendResetMail, sendVerificationMail } from "@/lib/mail/auth-mail.server";
import { mailDelivery } from "@/lib/mail/transport.server";
import { assertRegistrationAllowed } from "./registration.server";

/**
 * The auth construction, parameterized by the composition's AuthProviderConfig: the OSS
 * default is email+password with zero delivery dependency; magic links and social providers
 * register only when the composition provides them. There is no JWT plugin and no keyset —
 * the app talks to the vault over the internal lane with its own bearer, and a session never
 * becomes a token that leaves this tier.
 *
 * REGISTRATION IS NEVER OPEN (lib/auth/registration.server.ts): the `user.create.before`
 * hook demands a proof — the claim ceremony, a pending invitation on an armed-mail
 * deployment, or the off-by-default `registration = 'open'` knob — under EVERY rung, so a
 * composition's extra providers cannot reopen sign-up.
 *
 * Mail is the identity rung for multi-user servers: with SMTP armed, sign-up sends a
 * verification mail, and the INVITED SEAT BINDS ONLY AFTER the mailbox round-trip
 * (afterEmailVerification → bindInvitedSeats). Verification never gates sign-IN itself —
 * the claim-born first owner is deliberately unverified on a mail-less install, and
 * authority is seats, not verification flags.
 */
function buildAuth() {
  const env = serverEnv();
  const providers = composition.auth;
  const mailArmed = mailDelivery().canSend;
  return betterAuth({
    database: drizzleAdapter(getDb(), { provider: "pg" }),
    baseURL: env.BETTER_AUTH_URL,
    secret: env.BETTER_AUTH_SECRET,
    // Better Auth's built-in limiter arms by NODE_ENV, which a served test build sets to
    // production — key it on the app's OWN env so the credential endpoints stay limited in
    // production and the suites' rapid sign-ins don't trip it.
    rateLimit: { enabled: env.APP_ENV === "production" },
    ...(providers.emailAndPassword
      ? {
          emailAndPassword: {
            enabled: true,
            // The reset rung exists exactly when mail is armed; the mail-less solo recovery
            // hatch is the box-side one-shot code (lib/auth/recovery.server.ts).
            ...(mailArmed
              ? {
                  sendResetPassword: async ({
                    user,
                    url,
                  }: {
                    user: { email: string };
                    url: string;
                  }) => {
                    await sendResetMail(user.email, url);
                  },
                }
              : {}),
          },
        }
      : {}),
    ...(mailArmed
      ? {
          emailVerification: {
            sendOnSignUp: true,
            autoSignInAfterVerification: true,
            sendVerificationEmail: async ({
              user,
              url,
            }: {
              user: { email: string };
              url: string;
            }) => {
              await sendVerificationMail(user.email, url);
            },
            afterEmailVerification: async (user: { id: string; email: string; name: string }) => {
              await bindInvitedSeats(user.id, user.email, user.name || user.email);
            },
          },
        }
      : {}),
    databaseHooks: {
      user: {
        create: {
          before: async (user: { email: string }) => {
            await assertRegistrationAllowed(user.email);
            return { data: user };
          },
        },
      },
    },
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

/**
 * Verify a signed-in user's password AGAIN — the step-up re-authentication the admin
 * ceremonies run immediately before acting. Reads the better-auth credential account row for
 * the session's user and checks the presented password with better-auth's own verifier (the
 * same hasher sign-in uses — no second implementation). `false` for a wrong password AND for
 * an account with no password rung (a magic-link/social-only deployment has no password to
 * re-enter; the v1 step-up is the password rung, stated honestly in the ceremony copy).
 */
export async function verifySessionPassword(userId: string, password: string): Promise<boolean> {
  if (password.length === 0) {
    return false;
  }
  const rows = await getDb()
    .select({ password: account.password })
    .from(account)
    .where(and(eq(account.userId, userId), eq(account.providerId, "credential")))
    .limit(1);
  const hash = rows[0]?.password;
  if (hash == null || hash.length === 0) {
    return false;
  }
  return verifyPassword({ hash, password });
}

/**
 * Whether a user has a password rung at all — a `credential` account row carrying a hash. This
 * is what decides the step-up METHOD (step-up.server's `stepUpMethod`): a password-less account
 * (magic-link/social-only) has no password to re-enter and confirms through the mail round-trip
 * instead. Reads only presence — the hash itself never leaves the database.
 */
export async function hasCredentialPassword(userId: string): Promise<boolean> {
  const rows = await getDb()
    .select({ password: account.password })
    .from(account)
    .where(and(eq(account.userId, userId), eq(account.providerId, "credential")))
    .limit(1);
  const hash = rows[0]?.password;
  return hash != null && hash.length > 0;
}
