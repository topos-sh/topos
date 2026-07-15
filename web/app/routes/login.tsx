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
 * validated to a same-app path. Which sign-in rungs exist is composition, not client state: the
 * loader reads it server-side and passes plain flags, so the interactive form imports no server
 * config. The base rung is email + password (works with zero delivery dependency); the
 * magic-link rung shows only when a composition provides delivery.
 *
 * Sign-UP here is the invited/open-knob path (the claim ceremony has its own page), and
 * REGISTRATION IS NEVER OPEN by default: every refusal — uninvited, expired, already taken —
 * answers the ONE constant refusal string, carried through the loader so the client bundle
 * never imports the server module that owns it. With mail armed, a successful sign-up waits on
 * the mailbox round-trip (the seat binds after verification); mail-less, it signs in directly.
 */
export async function loader({ request }: LoaderFunctionArgs) {
  const url = new URL(request.url);
  const next = safeNextPath(url.searchParams.get("next") ?? undefined);
  return {
    next,
    magicLink: Boolean(composition.auth.magicLink),
    mailArmed: mailDelivery().canSend,
    signupRefusal: REGISTRATION_REFUSED,
  };
}

type Mode = "signin" | "signup";

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

export default function LoginPage() {
  const { next, magicLink, mailArmed, signupRefusal } = useLoaderData<typeof loader>();
  const navigate = useNavigate();

  const [mode, setMode] = useState<Mode>("signin");
  const [name, setName] = useState("");
  const [email, setEmail] = useState("");
  const [password, setPassword] = useState("");
  const [pending, setPending] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [magicSent, setMagicSent] = useState(false);
  const [verifySent, setVerifySent] = useState(false);

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
      // The sign-up refusal is CONSTANT whatever failed — an uninvited address, an expired
      // invitation, and a taken email all read the same, so the form enumerates nothing.
      setError(
        mode === "signin" ? "Couldn’t sign in. Check your email and password." : signupRefusal,
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

  async function sendMagicLink() {
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
          Check your mailbox to verify your address — your seat binds after verification.
        </p>
      </Shell>
    );
  }

  const signup = mode === "signup";
  return (
    <Shell>
      <h1 className="font-display font-semibold text-lg tracking-[-0.02em] text-ink">
        {signup ? "Create your account" : "Sign in to Topos"}
      </h1>
      <p className="mt-1 text-sm text-faint">
        {signup ? "Use your work email and a password." : "Sign in with your email and password."}
      </p>
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
        <label className="block">
          <span className="mb-1 block text-sm font-medium text-dim">Email</span>
          <input
            type="email"
            name="email"
            required
            autoComplete="email"
            value={email}
            onChange={(e) => setEmail(e.target.value)}
            className={INPUT}
            placeholder="you@company.com"
          />
        </label>
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
        <button
          type="submit"
          disabled={pending}
          className="h-11 w-full rounded-md bg-accent font-mono text-[13px] text-on-accent hover:bg-accent-deep focus:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2 disabled:opacity-60"
        >
          {pending ? (signup ? "Creating…" : "Signing in…") : signup ? "Create account" : "Sign in"}
        </button>
      </form>
      {error && (
        <p className="mt-3 text-sm text-red-600" role="alert">
          {error}
        </p>
      )}
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
      {magicLink && (
        <>
          <div className="mt-5 flex items-center gap-3">
            <span className="h-px flex-1 bg-line-soft" />
            <span className="text-xs text-faint">or</span>
            <span className="h-px flex-1 bg-line-soft" />
          </div>
          <button
            type="button"
            onClick={sendMagicLink}
            className="mt-4 h-11 w-full rounded-md border border-line bg-panel font-mono text-[13px] text-dim hover:bg-panel2 focus:outline-none focus-visible:ring-2 focus-visible:ring-accent focus-visible:ring-offset-2"
          >
            Email me a sign-in link
          </button>
        </>
      )}
    </Shell>
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
