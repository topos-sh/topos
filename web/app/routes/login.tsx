import { type FormEvent, type ReactNode, useState } from "react";
import {
  type LoaderFunctionArgs,
  type MetaFunction,
  useLoaderData,
  useNavigate,
} from "react-router";
import { composition } from "@/composition.server";
import { authClient } from "@/lib/auth/client";
import { safeNextPath } from "@/lib/auth/guards.server";
import { REGISTRATION_REFUSED } from "@/lib/auth/registration.server";
import { mailDelivery } from "@/lib/mail/transport.server";

export const meta: MetaFunction = () => [{ title: "Sign in · Topos" }];

/**
 * The `next` query (where sign-in returns to — e.g. back to a /verify page) is request data,
 * validated to a same-app path. WHICH sign-in rungs exist is composition, not client state: the
 * loader reads it server-side and passes plain flags (no server config reaches the client
 * bundle). The rungs, in order of prominence when present:
 *   - magic link — when a composition provides delivery, it LEADS: email + a primary
 *     "Email me a sign-in link"; email+password becomes a quiet "password instead" alternative.
 *   - social providers — one button per composed provider id, beneath the lead action.
 *   - email + password — the no-dependency default rung; it leads when magic link is absent, so
 *     a vanilla OSS build (no magic link, no social) renders exactly today's password-first page.
 *
 * Sign-UP follows the composition's registration policy, surfaced as ONE plain flag
 * (`registrationOpen`). GATED (the OSS default): sign-up is the invited/open-knob path (the
 * claim ceremony has its own page), and every refusal — uninvited, expired, already taken —
 * answers the ONE constant refusal string, carried through the loader so the client bundle never
 * imports the server module that owns it. The gate runs under EVERY rung (magic link and social
 * included), so no rung can reopen sign-up. OPEN (a hosted composition): the sign-up motion
 * exists on every composed rung — the magic-link lead serves sign-up too (a new address gets an
 * account on link consumption; the create hook stays the gate), social buttons sign up
 * naturally, and the copy drops the invited-only framing. With mail armed, a successful
 * password sign-up waits on the mailbox round-trip (an invited seat binds after verification);
 * mail-less, it signs in directly.
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const url = new URL(request.url);
  const next = safeNextPath(url.searchParams.get("next") ?? undefined);
  const auth = composition.auth;
  return {
    next,
    magicLink: Boolean(auth.magicLink),
    socialProviders: Object.keys(auth.socialProviders ?? {}),
    emailAndPassword: auth.emailAndPassword,
    mailArmed: mailDelivery().canSend,
    registrationOpen: composition.registration === "open",
    signupRefusal: REGISTRATION_REFUSED,
  };
}

type Mode = "signin" | "signup";

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";
const PRIMARY_BTN =
  "h-11 w-full rounded-md bg-accent font-mono text-[13px] text-on-accent hover:bg-accent-deep focus:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 disabled:opacity-60";
const QUIET_BTN =
  "flex h-11 w-full items-center justify-center gap-2 rounded-md border border-line bg-panel font-mono text-[13px] text-dim hover:bg-panel2 focus:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2";

/** The label/icon map for social providers — google today; an unknown id gets a titled fallback. */
function socialLabel(id: string): string {
  if (id === "google") {
    return "Continue with Google";
  }
  return `Continue with ${id.charAt(0).toUpperCase()}${id.slice(1)}`;
}

export default function LoginPage() {
  const {
    next,
    magicLink,
    socialProviders,
    emailAndPassword,
    mailArmed,
    registrationOpen,
    signupRefusal,
  } = useLoaderData<typeof loader>();
  const navigate = useNavigate();

  const [mode, setMode] = useState<Mode>("signin");
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [magicSent, setMagicSent] = useState(false);
  const [verifySent, setVerifySent] = useState(false);
  // When magic link leads, the email+password rung hides behind this quiet toggle.
  const [passwordRevealed, setPasswordRevealed] = useState(false);

  const signup = mode === "signup";
  // Magic link leads the sign-IN view; sign-up is always the password ceremony (the create hook
  // gates it, and magic link has no sign-up motion). Reveal the password form on demand — but a
  // composition that turned the password rung OFF never renders (or submits) a password form,
  // and its sign-up motion is whatever its remaining rungs provide (invite + magic link/social).
  const showMagic = magicLink && !signup && (!passwordRevealed || !emailAndPassword);

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (pending) return;
    setPending(true);
    setError(null);
    const address = email.trim();
    const { error: authError } =
      mode === "signin"
        ? await authClient.signIn.email({ email: address, password })
        : await authClient.signUp.email({
            email: address,
            password,
            // Better Auth requires a name on sign-up; the email is an honest default when the
            // optional field is left blank.
            name: name.trim() || address,
          });
    if (authError) {
      setPending(false);
      // GATED, the sign-up refusal is CONSTANT whatever failed — an uninvited address, an
      // expired invitation, and a taken email all read the same, so the form enumerates
      // nothing. OPEN, sign-up isn't invitation-framed: a failure gets a plain retry line
      // (still one constant string — nothing is enumerated either way).
      setError(
        mode === "signin"
          ? "Couldn’t sign in. Check your email and password."
          : registrationOpen
            ? "Couldn’t create the account. Check the details and try again."
            : signupRefusal,
      );
      return;
    }
    if (mode === "signup" && mailArmed) {
      // The seat binds only after the mailbox round-trip — hold here instead of navigating
      // into an app the account cannot enter yet.
      setPending(false);
      setVerifySent(true);
      return;
    }
    // Sign-up signs in on success, so both rungs land the same place.
    navigate(next);
  }

  async function submitMagicLink(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    const address = email.trim();
    if (!address) {
      setError("Enter your email first.");
      return;
    }
    setError(null);
    const { error: linkError } = await authClient.signIn.magicLink({
      email: address,
      callbackURL: next,
    });
    if (linkError) {
      setError("Couldn’t send the link. Check the address and try again.");
      return;
    }
    setMagicSent(true);
  }

  async function continueWithSocial(provider: string) {
    setError(null);
    const { error: socialError } = await authClient.signIn.social({
      // The composed provider id is a string; the client types it as its known-provider union.
      provider: provider as Parameters<typeof authClient.signIn.social>[0]["provider"],
      callbackURL: next,
    });
    if (socialError) {
      setError("Couldn’t start that sign-in. Try again.");
    }
  }

  if (magicSent) {
    return (
      <Shell>
        <p className="text-sm text-dim" role="status">
          Check your email — the link works for a few minutes.
        </p>
      </Shell>
    );
  }

  if (verifySent) {
    return (
      <Shell>
        <p className="text-sm text-dim" role="status">
          {registrationOpen
            ? // Open registration isn't invitation-framed — a seat may or may not be waiting.
              "Check your mailbox to verify your address."
            : "Check your mailbox to verify your address — your seat binds after verification."}
        </p>
      </Shell>
    );
  }

  return (
    <Shell>
      <h1 className="font-display font-semibold text-lg tracking-[-0.02em] text-ink">
        {signup ? "Create your account" : "Sign in to Topos"}
      </h1>
      <p className="mt-1 text-sm text-faint">
        {signup
          ? "Use your work email and a password."
          : showMagic
            ? registrationOpen
              ? // Open registration: the magic lead is "continue with email" — consuming the
                // link signs a known address in and creates an account for a new one.
                "We’ll email you a link — it signs you in, or creates your account if you’re new."
              : "We’ll email you a sign-in link — no password needed."
            : emailAndPassword
              ? "Sign in with your email and password."
              : "Continue with one of the options below."}
      </p>

      {showMagic ? (
        <form onSubmit={submitMagicLink} className="mt-6 space-y-3">
          <EmailField value={email} onChange={setEmail} />
          <button type="submit" className={PRIMARY_BTN}>
            {registrationOpen ? "Continue with email" : "Email me a sign-in link"}
          </button>
        </form>
      ) : !emailAndPassword ? null : (
        <form onSubmit={submit} className="mt-6 space-y-3">
          {signup && (
            <label className="block">
              <span className="mb-1 block text-sm font-medium text-dim">Name</span>
              <input
                type="text"
                name="name"
                autoComplete="name"
                value={name}
                onChange={(e) => setName(e.target.value)}
                className={INPUT}
                placeholder="Ada Lovelace"
              />
            </label>
          )}
          <EmailField value={email} onChange={setEmail} />
          <label className="block">
            <span className="mb-1 block text-sm font-medium text-dim">Password</span>
            <input
              type="password"
              name="password"
              required
              minLength={8}
              autoComplete={signup ? "new-password" : "current-password"}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              className={INPUT}
              placeholder="••••••••"
            />
          </label>
          <button type="submit" disabled={pending} className={PRIMARY_BTN}>
            {pending
              ? signup
                ? "Creating…"
                : "Signing in…"
              : signup
                ? "Create account"
                : "Sign in"}
          </button>
        </form>
      )}

      {error && (
        <p className="mt-3 text-sm text-red-600" role="alert">
          {error}
        </p>
      )}

      {socialProviders.length > 0 && (
        <>
          <Divider />
          <div className="mt-4 space-y-2">
            {socialProviders.map((id) => (
              <button
                key={id}
                type="button"
                onClick={() => continueWithSocial(id)}
                className={QUIET_BTN}
              >
                {id === "google" && <GoogleIcon />}
                {socialLabel(id)}
              </button>
            ))}
          </div>
        </>
      )}

      {/* Magic link leads; email+password is the quiet alternative reachable from here. */}
      {magicLink && !signup && emailAndPassword && (
        <button
          type="button"
          onClick={() => {
            setPasswordRevealed((v) => !v);
            setError(null);
          }}
          className="mt-4 block w-full text-center text-sm text-dim underline-offset-2 hover:text-ink hover:underline"
        >
          {passwordRevealed
            ? registrationOpen
              ? "Continue with email instead"
              : "Email me a sign-in link instead"
            : "Sign in with a password instead"}
        </button>
      )}

      {emailAndPassword && (
        <p className="mt-4 text-sm text-dim">
          {signup ? "Already have an account?" : "New to Topos?"}{" "}
          <button
            type="button"
            onClick={() => {
              setMode(signup ? "signin" : "signup");
              setError(null);
            }}
            className="border-b border-hairline text-ink transition-colors hover:border-ink"
          >
            {signup ? "Sign in" : "Create an account"}
          </button>
        </p>
      )}
    </Shell>
  );
}

function EmailField({ value, onChange }: { value: string; onChange: (value: string) => void }) {
  return (
    <label className="block">
      <span className="mb-1 block text-sm font-medium text-dim">Email</span>
      <input
        type="email"
        name="email"
        required
        autoComplete="email"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        className={INPUT}
        placeholder="you@company.com"
      />
    </label>
  );
}

function Divider() {
  return (
    <div className="mt-5 flex items-center gap-3">
      <span className="h-px flex-1 bg-line-soft" />
      <span className="text-xs text-faint">or</span>
      <span className="h-px flex-1 bg-line-soft" />
    </div>
  );
}

/** The Google mark, inline so the CSP-strict page needs no external asset. */
function GoogleIcon() {
  return (
    <svg viewBox="0 0 18 18" width="16" height="16" aria-hidden="true">
      <path
        fill="#4285F4"
        d="M17.64 9.2c0-.64-.06-1.25-.16-1.84H9v3.48h4.84a4.14 4.14 0 0 1-1.8 2.72v2.26h2.92c1.7-1.57 2.68-3.88 2.68-6.62Z"
      />
      <path
        fill="#34A853"
        d="M9 18c2.43 0 4.47-.8 5.96-2.18l-2.92-2.26c-.8.54-1.83.86-3.04.86-2.34 0-4.32-1.58-5.03-3.7H.96v2.33A9 9 0 0 0 9 18Z"
      />
      <path
        fill="#FBBC05"
        d="M3.97 10.72a5.4 5.4 0 0 1 0-3.44V4.95H.96a9 9 0 0 0 0 8.1l3.01-2.33Z"
      />
      <path
        fill="#EA4335"
        d="M9 3.58c1.32 0 2.5.45 3.44 1.35l2.58-2.58C13.47.9 11.43 0 9 0A9 9 0 0 0 .96 4.95l3.01 2.33C4.68 5.16 6.66 3.58 9 3.58Z"
      />
    </svg>
  );
}

function Shell({ children }: { children: ReactNode }) {
  return (
    <main className="flex min-h-dvh items-center justify-center bg-ground px-4">
      <div className="w-full max-w-sm">
        <div className="rounded-lg border border-line-soft bg-panel p-8 shadow-card">
          {children}
        </div>
      </div>
    </main>
  );
}
