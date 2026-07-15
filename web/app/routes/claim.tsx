import type { ReactNode } from "react";
import {
  type ActionFunctionArgs,
  data,
  Form,
  type LoaderFunctionArgs,
  type MetaFunction,
  redirect,
  useActionData,
  useLoaderData,
} from "react-router";
import { buttonClasses } from "@/components/ui";
import { notFound } from "@/lib/auth/guards.server";
import { withRegistrationCeremony } from "@/lib/auth/registration.server";
import { getAuth } from "@/lib/auth/server";
import { claimableWorkspace, consumeClaim } from "@/lib/db/identity.server";
import { allowPublicRead, clientKeyFromXff } from "@/lib/rate-limit.server";

export const meta: MetaFunction = () => [{ title: "Finish setup · Topos" }];

/**
 * The first-boot CLAIM ceremony — the one tokened URL in the product, printed to the server
 * logs at boot and dead after one use. Whoever opens the live link proves machine control (they
 * can read the box's logs), creates the first account, and is seated as the workspace's first
 * owner. Everything about this page is uniform-miss: no code, a wrong code, a spent code, and a
 * belted client all render the same 404 as any missing route.
 */

/** The URL carries the setup code — this page never lands in a cache or a search index. */
export function headers() {
  return { "Cache-Control": "no-store", "X-Robots-Tag": "noindex" };
}

/** The public-probe belt, shared by GET and POST: a denied client gets the uniform miss. */
function requireBelt(request: Request): void {
  if (!allowPublicRead(clientKeyFromXff(request.headers.get("x-forwarded-for")))) {
    notFound();
  }
}

export async function loader({ request }: LoaderFunctionArgs) {
  requireBelt(request);
  const code = new URL(request.url).searchParams.get("code") ?? "";
  if (code === "") {
    notFound();
  }
  const workspace = await claimableWorkspace(code);
  if (workspace === null) {
    notFound();
  }
  return { workspaceName: workspace.displayName, code };
}

/**
 * ONE honest error for every sign-up failure — a taken email included. This page is public and
 * reachable by anyone holding the link, so it distinguishes nothing about existing accounts.
 */
const CLAIM_SIGNUP_FAILED =
  "Couldn’t create the account with those details. Check them and try again.";

export async function action({ request }: ActionFunctionArgs) {
  requireBelt(request);
  const form = await request.formData();
  // The form carries the code in a hidden field; a bare POST to the tokened URL works too.
  const code =
    String(form.get("code") ?? "") || (new URL(request.url).searchParams.get("code") ?? "");
  if (code === "" || (await claimableWorkspace(code)) === null) {
    notFound();
  }

  const email = String(form.get("email") ?? "").trim();
  const password = String(form.get("password") ?? "");
  const name = String(form.get("name") ?? "").trim();
  // Mirror Better Auth's own minimum so a rejected password never reaches the ceremony.
  if (email.length === 0 || password.length < 8) {
    return data(
      { error: "Enter your email and a password of at least 8 characters." },
      { status: 400 },
    );
  }
  const display = name || email;

  // Create the account INSIDE the registration ceremony: the `user.create.before` hook admits
  // this one sign-up path without an invitation or the open knob. `returnHeaders` captures the
  // session cookies Better Auth mints, so the claimant lands signed in.
  const auth = getAuth();
  const result = await withRegistrationCeremony(() =>
    auth.api.signUpEmail({
      body: { email, password, name: display },
      returnHeaders: true,
    }),
  ).catch(() => null);
  if (result === null) {
    return data({ error: CLAIM_SIGNUP_FAILED }, { status: 400 });
  }

  const claimed = await consumeClaim(code, result.response.user.id, display);
  if (claimed === null) {
    // The race loser: a concurrent submit consumed the code between the probe above and the
    // atomic consume. The account just created stands — a user row with no seat, which can
    // sign in and admits nothing — an accepted, harmless orphan; the answer stays the uniform
    // miss.
    notFound();
  }

  const responseHeaders = new Headers();
  for (const cookie of result.headers.getSetCookie()) {
    responseHeaders.append("set-cookie", cookie);
  }
  throw redirect("/workspaces", { headers: responseHeaders });
}

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

export default function ClaimPage() {
  const { workspaceName, code } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();
  return (
    <Shell>
      <p className="font-medium text-faint text-xs uppercase tracking-wide">Finish setup</p>
      <h1 className="mt-2 font-display font-semibold text-ink text-lg tracking-[-0.02em]">
        Claim {workspaceName}
      </h1>
      <p className="mt-1 text-faint text-sm">
        Create the first account on this install — it becomes the workspace owner.
      </p>
      <Form method="post" className="mt-6 space-y-3">
        <input type="hidden" name="code" value={code} />
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">Name</span>
          <input
            type="text"
            name="name"
            autoComplete="name"
            className={INPUT}
            placeholder="Ada Lovelace"
          />
        </label>
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">Email</span>
          <input
            type="email"
            name="email"
            required
            autoComplete="email"
            className={INPUT}
            placeholder="you@company.com"
          />
        </label>
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">Password</span>
          <input
            type="password"
            name="password"
            required
            minLength={8}
            autoComplete="new-password"
            className={INPUT}
            placeholder="••••••••"
          />
        </label>
        <button type="submit" className={`${buttonClasses("primary")} min-h-11 w-full`}>
          Claim this workspace
        </button>
      </Form>
      {actionData?.error !== undefined && (
        <p className="mt-3 text-red-600 text-sm" role="alert">
          {actionData.error}
        </p>
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
