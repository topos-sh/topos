import type { ReactNode } from "react";
import {
  type ActionFunctionArgs,
  data,
  Link,
  type LoaderFunctionArgs,
  type MetaFunction,
  redirect,
  useLoaderData,
} from "react-router";
import { buttonClasses } from "@/components/ui";
import { ApproveEnrollCard } from "@/components/verify/ApproveEnrollCard";
import { ApproveLoginCard } from "@/components/verify/ApproveLoginCard";
import { ApproveStandupCard } from "@/components/verify/ApproveStandupCard";
import { VerifyCard } from "@/components/verify/VerifyCard";
import { actorFromSession } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { vaultFetch } from "@/lib/plane/client.server";
import { getVerificationContext } from "@/lib/plane/reads.server";

export const meta: MetaFunction = () => [{ title: "Device verification · Topos" }];

/**
 * PUBLIC — this page arrives via emailed links (and the agent's verification_uri_complete) on
 * phones and must work on a blocking, one-document render. Signed OUT: the vault-reported
 * disclosure + a "Sign in to continue" gate. Signed IN (with a verified email): the approve
 * variant for the session's intent — join (enroll) or create-a-workspace (standup) — a form, an
 * action, and a redirect back here with an `outcome` query (pure display state; the vault's
 * answer is the authority).
 */

/** The vault's static create-denial reason for the per-owner cap (rendered as the limit state). */
const CAP_REASON = "workspace creation limit reached";

type VerifyOutcome = "approved" | "limit" | "miss" | "error";

// ── The action: the approve legs ──────────────────────────────────────────────────────────────
// A verified session actor is required HERE (the gate); the acting identity then rides the vault
// transport's own acting-principal header — never anything client-supplied (the form submits only
// an intent and, for standup, a display name). Result → a redirect back with `outcome` (display
// only; forging the query changes pixels, never the vault).

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const userCode = params.userCode ?? "";
  const actor = actorFromSession(await getAuth().api.getSession({ headers: request.headers }));
  if (!actor) {
    return redirect(`/login?next=${encodeURIComponent(`/verify/${userCode}`)}`);
  }
  const form = await request.formData();
  const intent = String(form.get("intent") ?? "");

  if (intent === "approve-enroll") {
    return redirect(outcomeUrl(userCode, await approveEnroll(userCode, actor.email)));
  }
  if (intent === "approve-standup") {
    const name = String(form.get("display_name") ?? "").trim();
    return redirect(
      outcomeUrl(
        userCode,
        await approveStandup(userCode, actor.email, name === "" ? undefined : name),
      ),
    );
  }
  throw data(null, { status: 400 });
}

function outcomeUrl(userCode: string, outcome: VerifyOutcome): string {
  return `/verify/${encodeURIComponent(userCode)}?outcome=${outcome}`;
}

/** Read the vault write's outcome envelope, tolerating a body that never parsed. */
async function readOutcome(res: Response): Promise<{ outcome?: string; reason?: string }> {
  try {
    return (await res.json()) as { outcome?: string; reason?: string };
  } catch {
    return {};
  }
}

/** Approve an ENROLL session (join an existing workspace) as the signed-in email. */
async function approveEnroll(userCode: string, actingEmail: string): Promise<VerifyOutcome> {
  try {
    const res = await vaultFetch({
      method: "POST",
      template: "/internal/v1/device-sessions/{user_code}/approve",
      params: { user_code: userCode },
      actingEmail,
    });
    const { outcome } = await readOutcome(res);
    if (outcome === "confirmed") return "approved";
    if (outcome === "not_found") return "miss";
    return "error";
  } catch {
    return "error";
  }
}

/**
 * Approve a STANDUP session — creates the workspace (named by the optional field; blank takes the
 * vault default) and seats the signed-in email as its first owner in the directory. A same-email
 * re-click (`already_approved`) renders the SAME success — idempotent UX; the per-owner cap
 * denial renders the honest limit state.
 */
async function approveStandup(
  userCode: string,
  actingEmail: string,
  name: string | undefined,
): Promise<VerifyOutcome> {
  try {
    const res = await vaultFetch({
      method: "POST",
      template: "/internal/v1/device-sessions/{user_code}/approve-standup",
      params: { user_code: userCode },
      actingEmail,
      body: name === undefined ? {} : { display_name: name },
    });
    const { outcome, reason } = await readOutcome(res);
    if (outcome === "approved" || outcome === "already_approved") return "approved";
    if (outcome === "denied") return reason === CAP_REASON ? "limit" : "error";
    if (outcome === "not_found") return "miss";
    return "error";
  } catch {
    return "error";
  }
}

// ── The loader: what to render ────────────────────────────────────────────────────────────────

export async function loader({ request, params }: LoaderFunctionArgs) {
  const userCode = params.userCode ?? "";
  const outcome = new URL(request.url).searchParams.get("outcome");

  // The post-approve display states — a redirect lands here with `outcome`, no context read
  // needed. Display only.
  if (outcome === "approved") {
    return { state: "approved" as const };
  }
  if (outcome === "limit") {
    return { state: "limit" as const };
  }
  // `error` and `miss` fall through: the context read below re-renders the live affordance (so
  // "try again" is real), or answers 404 for a session that died in the meantime.

  // The session and the context are independent reads — start the session alongside the context
  // read instead of after it. The detached catch keeps an early context-path return from leaving
  // an unhandled rejection behind; awaiting below still surfaces a real session-read failure.
  const sessionPromise = getAuth().api.getSession({ headers: request.headers });
  void sessionPromise.catch(() => {});
  const result = await getVerificationContext(userCode);

  if (!result.ok) {
    if (result.kind === "not_found") {
      return { state: "not_found" as const };
    }
    if (result.kind === "rate_limited") {
      return { state: "rate_limited" as const };
    }
    return { state: "unreachable" as const };
  }
  if (outcome === "miss") {
    // A live session, but the approve missed (a different email got there first, or a race).
    return { state: "miss" as const };
  }

  const banner = outcome === "error";
  const actor = actorFromSession(await sessionPromise);
  if (!actor) {
    // Signed out (or unverified): the full disclosure + the sign-in gate.
    return { state: "signed_out" as const, userCode, banner, context: result.data };
  }
  if (result.data.intent === "login") {
    return {
      state: "login" as const,
      userCode,
      banner,
      context: result.data,
      sessionEmail: actor.email,
    };
  }
  if (result.data.intent === "standup") {
    const localpart = actor.email.split("@")[0] ?? actor.email;
    return {
      state: "standup" as const,
      userCode,
      banner,
      context: result.data,
      sessionEmail: actor.email,
      defaultName: `${localpart}'s workspace`,
    };
  }
  return {
    state: "enroll" as const,
    userCode,
    banner,
    context: result.data,
    sessionEmail: actor.email,
  };
}

// ── The page ──────────────────────────────────────────────────────────────────────────────────

export default function VerifyPage() {
  const view = useLoaderData<typeof loader>();
  return (
    <main className="mx-auto flex min-h-dvh w-full max-w-md flex-col justify-center px-4 py-10">
      <div className="rounded-lg border border-line-soft bg-panel p-6 shadow-sm sm:p-8">
        <VerifyBody view={view} />
      </div>
    </main>
  );
}

function VerifyBody({ view }: { view: Awaited<ReturnType<typeof loader>> }) {
  switch (view.state) {
    case "approved":
      return (
        <PlainState heading="Approved">
          Return to your terminal — your agent finishes from here (it may already have).
        </PlainState>
      );
    case "limit":
      return (
        <PlainState heading="You already own 3 workspaces">
          The vault declined: workspace creation limit reached. Nothing was created — use one of
          your existing workspaces, or contact us.
        </PlainState>
      );
    case "not_found":
      return (
        <PlainState heading="This code isn’t active">
          Codes expire after a few minutes — start again from your device.
        </PlainState>
      );
    case "rate_limited":
      return (
        <PlainState heading="Too many attempts">Wait a minute and reload this page.</PlainState>
      );
    case "unreachable":
      return (
        <PlainState heading="Verification is unavailable right now">
          The vault couldn’t be reached. Nothing was confirmed — reload this page in a moment.
        </PlainState>
      );
    case "miss":
      return (
        <PlainState heading="This approval didn’t match">
          The session couldn’t be approved by this account. If this device isn’t yours, ignore this
          page — nothing happens.
        </PlainState>
      );
    case "signed_out":
      return (
        <div className="flex flex-col gap-6">
          <Banner show={view.banner} />
          <VerifyCard context={view.context} />
          <Link
            to={`/login?next=${encodeURIComponent(`/verify/${view.userCode}`)}`}
            className={`${buttonClasses("primary")} min-h-11 w-full`}
          >
            Sign in to continue
          </Link>
        </div>
      );
    case "standup":
      return (
        <div className="flex flex-col gap-6">
          <Banner show={view.banner} />
          <ApproveStandupCard
            userCode={view.userCode}
            context={view.context}
            sessionEmail={view.sessionEmail}
            defaultName={view.defaultName}
          />
        </div>
      );
    case "login":
      return (
        <div className="flex flex-col gap-6">
          <Banner show={view.banner} />
          <ApproveLoginCard
            userCode={view.userCode}
            context={view.context}
            sessionEmail={view.sessionEmail}
          />
        </div>
      );
    case "enroll":
      return (
        <div className="flex flex-col gap-6">
          <Banner show={view.banner} />
          <ApproveEnrollCard
            userCode={view.userCode}
            context={view.context}
            sessionEmail={view.sessionEmail}
          />
        </div>
      );
  }
}

/** The retryable-error alert — rendered ABOVE the live affordance, never instead of it. */
function Banner({ show }: { show: boolean }) {
  if (!show) {
    return null;
  }
  return (
    <p className="text-center text-red-600 text-sm" role="alert">
      That didn’t go through — the vault couldn’t complete the approval. Nothing was confirmed. Try
      again below.
    </p>
  );
}

function PlainState({ heading, children }: { heading: string; children: ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-2 text-center">
      <p className="font-medium text-xs text-faint uppercase tracking-wide">Device verification</p>
      <h1 className="font-display font-semibold text-lg tracking-[-0.02em] text-ink">{heading}</h1>
      <p className="text-sm text-dim">{children}</p>
    </div>
  );
}
