import { type FormEvent, type ReactNode, useState } from "react";
import {
  type LoaderFunctionArgs,
  type MetaFunction,
  useLoaderData,
  useNavigate,
} from "react-router";
import { BusyFields } from "@/components/ui";
import { composition } from "@/composition.server";
import { authClient } from "@/lib/auth/client";
import { safeNextPath } from "@/lib/auth/guards.server";
import { REGISTRATION_REFUSED } from "@/lib/auth/registration.server";
import { mailDelivery } from "@/lib/mail/transport.server";
import { rebuildVerifyNext } from "@/lib/verify-path";

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
  // A `next` that targets /verify is REBUILT server-side from its shape-validated components
  // (the shared verify-path rule) rather than echoed: the value rides into the magic-link
  // mail's callbackURL and every rung's post-sign-in navigation, and a login approval must
  // resume — in any browser, fresh cookie jar included — on a path this server derived, never
  // on a raw string a link carried. Anything else takes the ordinary same-app-path validation.
  const rawNext = url.searchParams.get("next") ?? undefined;
  const next = rebuildVerifyNext(rawNext) ?? safeNextPath(rawNext);
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

/**
 * The rung with a request on the wire — at most ONE at a time. Every rung goes inert while any is
 * in flight (starting Google mid-magic-link would race two sign-ins against one page), and the
 * busy rung is the one that swaps its label, so the wait always names what it is waiting for.
 * Sign-in is the page where a slow answer is most likely — a mailed link is an SMTP round-trip,
 * a password is a deliberate slow hash — and it is exactly where an unmarked wait reads as a
 * dead button.
 */
type Busy = { rung: "password" } | { rung: "magic" } | { rung: "social"; provider: string } | null;

/**
 * What a rung says when its call REJECTED rather than answering. Better Auth's client returns
 * `{ error }` for anything the server decided and throws only when the request never completed —
 * offline, connection reset — so this line carries no information about the account and is not
 * an enumeration risk. It gets its own words because a rung's constant refusal would send
 * someone off to re-check a password that was fine.
 *
 * Every rejection path MUST re-arm the form. The fields are inside a disabled fieldset by then,
 * so a busy state left set is a login page you can only escape by reloading — the exact
 * dead-looking form this whole change exists to prevent.
 */
const UNREACHABLE = "Couldn’t reach the server. Check your connection and try again.";

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";
const PRIMARY_BTN =
  "h-11 w-full rounded-md bg-accent font-mono text-[13px] text-on-accent hover:bg-accent-deep focus:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-60";
const QUIET_BTN =
  "flex h-11 w-full items-center justify-center gap-2 rounded-md border border-line bg-panel font-mono text-[13px] text-dim hover:bg-panel2 focus:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 disabled:cursor-not-allowed disabled:opacity-60";
/** The two quiet text toggles (rung switch, sign-in/sign-up switch) — inert while a rung flies. */
const TOGGLE_BTN = "disabled:cursor-not-allowed disabled:opacity-60";

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
  const [busy, setBusy] = useState<Busy>(null);
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
  const anyBusy = busy !== null;

  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (anyBusy) return;
    setBusy({ rung: "password" });
    setError(null);
    const address = email.trim();
    const attempt = await (mode === "signin"
      ? authClient.signIn.email({ email: address, password })
      : authClient.signUp.email({
          email: address,
          password,
          // Better Auth requires a name on sign-up; the email is an honest default when the
          // optional field is left blank.
          name: name.trim() || address,
        })
    ).catch(() => null);
    if (attempt === null) {
      setBusy(null);
      setError(UNREACHABLE);
      return;
    }
    const { error: authError } = attempt;
    if (authError) {
      setBusy(null);
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
      setBusy(null);
      setVerifySent(true);
      return;
    }
    // Sign-up signs in on success, so both rungs land the same place. The rung stays BUSY across
    // the navigate: this card is on its way out, and re-arming it for those frames would offer a
    // second submit of a sign-in that already succeeded.
    navigate(next);
  }

  async function submitMagicLink(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (anyBusy) return;
    const address = email.trim();
    if (!address) {
      setError("Enter your email first.");
      return;
    }
    setError(null);
    // Held busy from here: the send is an SMTP round-trip on the server, the slowest wait the
    // page has, and until it answers this card must not look idle.
    setBusy({ rung: "magic" });
    const sent = await authClient.signIn
      .magicLink({ email: address, callbackURL: next })
      .catch(() => null);
    if (sent === null) {
      setBusy(null);
      setError(UNREACHABLE);
      return;
    }
    if (sent.error) {
      setBusy(null);
      setError("Couldn’t send the link. Check the address and try again.");
      return;
    }
    // The sent card replaces this one — leaving busy set keeps the form inert for those frames.
    setMagicSent(true);
  }

  async function continueWithSocial(provider: string) {
    if (anyBusy) return;
    setError(null);
    setBusy({ rung: "social", provider });
    const started = await authClient.signIn
      .social({
        // The composed provider id is a string; the client types it as its known-provider union.
        provider: provider as Parameters<typeof authClient.signIn.social>[0]["provider"],
        callbackURL: next,
      })
      .catch(() => null);
    if (started === null) {
      setBusy(null);
      setError(UNREACHABLE);
      return;
    }
    if (started.error) {
      setBusy(null);
      setError("Couldn’t start that sign-in. Try again.");
    }
    // Success hands the browser to the provider — stay busy through the redirect.
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
        <form onSubmit={submitMagicLink} className="mt-6">
          <BusyFields busy={anyBusy} className="space-y-3">
            <EmailField value={email} onChange={setEmail} />
            <button type="submit" className={PRIMARY_BTN}>
              {busy?.rung === "magic"
                ? "Sending the link…"
                : registrationOpen
                  ? "Continue with email"
                  : "Email me a sign-in link"}
            </button>
          </BusyFields>
        </form>
      ) : !emailAndPassword ? null : (
        <form onSubmit={submit} className="mt-6">
          <BusyFields busy={anyBusy} className="space-y-3">
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
            {/* The label names THIS rung's wait only: another rung in flight disables the button
                without pretending a password is being checked. */}
            <button type="submit" className={PRIMARY_BTN}>
              {busy?.rung === "password"
                ? signup
                  ? "Creating…"
                  : "Signing in…"
                : signup
                  ? "Create account"
                  : "Sign in"}
            </button>
          </BusyFields>
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
                disabled={anyBusy}
                className={QUIET_BTN}
              >
                {id === "google" && <GoogleIcon />}
                {busy?.rung === "social" && busy.provider === id
                  ? "Taking you there…"
                  : socialLabel(id)}
              </button>
            ))}
          </div>
        </>
      )}

      {/* Magic link leads; email+password is the quiet alternative reachable from here. */}
      {magicLink && !signup && emailAndPassword && (
        <button
          type="button"
          disabled={anyBusy}
          onClick={() => {
            setPasswordRevealed((v) => !v);
            setError(null);
          }}
          className={`mt-4 block w-full text-center text-sm text-dim underline-offset-2 hover:text-ink hover:underline ${TOGGLE_BTN}`}
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
            disabled={anyBusy}
            onClick={() => {
              setMode(signup ? "signin" : "signup");
              setError(null);
            }}
            className={`border-b border-hairline text-ink transition-colors hover:border-ink ${TOGGLE_BTN}`}
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
