/**
 * The auth-provider seam — the fourth composition seam. The app owns accounts + sessions (the
 * login ceremony); WHICH rungs exist is composition config. The OSS default is email+password
 * with ZERO delivery dependency: a self-hosted team signs in without SMTP, OAuth apps, or any
 * external service. A downstream build swaps in its own rungs (magic links, social providers)
 * by providing this object — never by editing the auth construction.
 *
 * A session is evidence, never authority: every admission decision resolves against the
 * directory's roster at request time (see lib/auth/guards.server.ts).
 */
export interface MagicLinkDelivery {
  /** Deliver a sign-in link to `email`. Failures are the provider's to surface. */
  send(args: { email: string; url: string; token: string }): Promise<void>;
}

export interface SocialProviderConfig {
  clientId: string;
  clientSecret: string;
}

export interface AuthProviderConfig {
  /** The no-dependency default rung. On in OSS; a composition may turn it off. */
  emailAndPassword: boolean;
  /** Registered only when a delivery seam is provided — email stays optional. */
  magicLink?: MagicLinkDelivery;
  /** Better-Auth social provider ids (e.g. `google`) to their credentials. */
  socialProviders?: Record<string, SocialProviderConfig>;
}

/** The OSS default: email+password, nothing to configure, nothing to deliver. */
export const defaultAuthConfig: AuthProviderConfig = {
  emailAndPassword: true,
};
