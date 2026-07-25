import { betterAuth } from "better-auth";
import { drizzleAdapter } from "better-auth/adapters/drizzle";
import { magicLink } from "better-auth/plugins/magic-link";
import { composition } from "@/composition.server";
import { serverEnv } from "@/env.server";
import { bindInvitedSeats, endAllSessionsOfUser } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { sendResetMail, sendVerificationMail } from "@/lib/mail/auth-mail.server";
import { mailDelivery } from "@/lib/mail/transport.server";
import { personDisplay } from "@/lib/person-display";
import { assertRegistrationAllowed } from "./registration.server";

/**
 * The private header `/api/auth/*` writes the app-resolved client key onto, and the ONE header
 * Better Auth's limiter reads. Never accepted from a caller: the route overwrites it on every
 * request before the library ever sees it.
 */
export const RESOLVED_CLIENT_IP_HEADER = "x-topos-client-ip";

/**
 * The auth construction, parameterized by the composition's AuthProviderConfig: the OSS
 * default is email+password with zero delivery dependency; magic links and social providers
 * register only when the composition provides them. There is no JWT plugin and no keyset —
 * the app talks to the vault over the internal lane with its own bearer, and a session never
 * becomes a token that leaves this tier.
 *
 * REGISTRATION IS COMPOSITION-OWNED (lib/auth/registration.server.ts): the
 * `user.create.before` hook runs under EVERY rung, so no provider bypasses the policy. The
 * OSS default is GATED — a proof is demanded (the claim ceremony, a pending invitation on an
 * armed-mail deployment, or — single tenancy only — the off-by-default `registration =
 * 'open'` knob). A composition that sets `registration: "open"` admits every sign-up; the
 * hook still runs, it just allows.
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
    // …and key it on the client the APP resolved. Left to itself the library reads
    // `x-forwarded-for` and trusts it only when it holds exactly ONE hop, so behind the ordinary
    // two-hop ingress it resolves nothing and buckets every caller together — one global
    // 3-per-10s sign-in allowance for the whole deployment. `/api/auth/*` overwrites this header
    // with the last-hop key on every request (see routes/api.auth.ts).
    advanced: { ipAddress: { ipAddressHeaders: [RESOLVED_CLIENT_IP_HEADER] } },
    ...(providers.emailAndPassword
      ? {
          emailAndPassword: {
            enabled: true,
            // A reset ENDS every browser session the old password could have opened — "I reset
            // my password" is the remediation people actually reach for, so it has to be one.
            // (The CLI credentials it cannot see are ended beside it, in recovery.server.ts and
            // the reset hook below; a bearer outlives a cookie and is the sharper half.)
            revokeSessionsOnPasswordReset: true,
            // …and every CLI credential with them. A bearer is the sharper half: it is
            // workspace-scoped, has no default expiry, and lives on a machine the person may no
            // longer hold, so leaving it alive would make the reset a reassurance rather than a
            // remedy. Ended as `self` — the account holder's own act.
            onPasswordReset: async ({ user }: { user: { id: string } }) => {
              await endAllSessionsOfUser(user.id, "self");
            },
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
              await bindInvitedSeats(user.id, user.email, personDisplay(user.name, user.email));
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
            // Hash-stored, like every other credential in this system: the plugin's default
            // keeps the live token in `web.verification.identifier` verbatim, so anything that
            // can read that table — a backup, a replica, the cloud role's mirror — could sign
            // in as its addressee inside the link's window.
            storeToken: "hashed",
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
