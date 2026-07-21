import type { ReactNode } from "react";
import {
  type ActionFunctionArgs,
  Form,
  Link,
  type LoaderFunctionArgs,
  type MetaFunction,
  redirect,
  useActionData,
  useLoaderData,
  useNavigation,
} from "react-router";
import { buttonClasses, Chip } from "@/components/ui";
import { composition } from "@/composition.server";
import { actorFromSession, notFound } from "@/lib/auth/guards.server";
import { withInvitationCeremony } from "@/lib/auth/registration.server";
import { getAuth } from "@/lib/auth/server";
import {
  acceptInvitationByToken,
  type DeviceGrantHint,
  declineInvitationByToken,
  invitationPageView,
  mintInvitationSignIn,
} from "@/lib/db/identity.server";
import { mailDelivery } from "@/lib/mail/transport.server";
import { personDisplay } from "@/lib/person-display";
import { allowPublicRead, clientKeyFromXff } from "@/lib/rate-limit.server";
import { wsPathServer } from "@/lib/ws-url.server";

export const meta: MetaFunction = () => [{ title: "Invitation · Topos" }];

/**
 * The tokened INVITATION page — the mailed link's landing. The link is worth one invitation,
 * never an account or a credential: viewing NEVER consumes (GET-safe for mail scanners);
 * accept and decline are explicit POSTs. The invitation binds to the INVITED EMAIL's account:
 * signed in as it → one-click accept; no such account → this page mints it (email locked to
 * the invited address; passwordless where a mail sign-in rung exists — the token's delivery to
 * that mailbox IS the proof, so no verification mail); signed in as someone else → the switch
 * page, never an accept as the current account. Every dead token — invalid, expired, revoked,
 * already used — renders ONE constant message that names neither the workspace nor any email.
 *
 * A terminal-first arrival (the CLI's `follow <invite-url>`) rides the same page carrying the
 * device-flow pass-through params; after the accept it continues into `/verify` so sign-in →
 * accept → device approval land as one visit.
 */

/** The URL carries the single-use token — never cached, never indexed. */
export function headers() {
  return { "Cache-Control": "no-store", "X-Robots-Tag": "noindex" };
}

/** The public-probe belt (the claim page's posture): a belted client gets the uniform miss. */
function requireBelt(request: Request): void {
  if (!allowPublicRead(clientKeyFromXff(request.headers.get("x-forwarded-for")))) {
    notFound();
  }
}

/** The ONE constant answer for every dead link — nothing about the workspace or the email. */
const GONE =
  "This invitation link isn't active. It may have been used already, expired, or been replaced by a newer one — ask whoever invited you for a fresh link.";

/** The device-flow pass-through params a CLI-started arrival carries (all non-secret; the
 * short code itself never enters a URL). Validated by shape, dropped otherwise. */
interface DeviceParams {
  device: string;
  port: string | null;
  state: string | null;
}

function deviceParamsFrom(url: URL): DeviceParams | null {
  const device = url.searchParams.get("device") ?? "";
  if (!/^[0-9a-f]{64}$/.test(device)) {
    return null;
  }
  const port = url.searchParams.get("port");
  const state = url.searchParams.get("state");
  return {
    device,
    port: port !== null && /^\d{4,5}$/.test(port) ? port : null,
    state: state !== null && /^[A-Za-z0-9_-]{8,128}$/.test(state) ? state : null,
  };
}

/** Where an accepted invitation lands: the /verify weave when a device flow is waiting, else
 * the hinted page, else the workspace root. */
function acceptLanding(
  workspaceName: string,
  hint: DeviceGrantHint | null,
  device: DeviceParams | null,
): string {
  if (device !== null) {
    const qs = new URLSearchParams({ device: device.device });
    if (device.port !== null) {
      qs.set("port", device.port);
    }
    if (device.state !== null) {
      qs.set("state", device.state);
    }
    return `/verify?${qs.toString()}`;
  }
  if (hint !== null) {
    return wsPathServer(
      workspaceName,
      hint.kind === "channel" ? `channels/${hint.name}` : `skills/${hint.name}`,
    );
  }
  return wsPathServer(workspaceName);
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  requireBelt(request);
  const token = params.token ?? "";
  const url = new URL(request.url);
  const device = deviceParamsFrom(url);
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  const resolved = token === "" ? null : await invitationPageView(token, actor?.userId ?? null);
  if (resolved === null) {
    return { state: "gone" as const, message: GONE };
  }
  const { view, branch } = resolved;
  if (branch === "member") {
    // Already a member: straight into the workspace (the hinted page when one is named).
    // Nothing consumed — a GET stays a view.
    throw redirect(acceptLanding(view.workspaceName, view.hint, device));
  }
  return {
    state: "view" as const,
    branch,
    /** This page's own path+query — the sign-in return target, computed server-side. */
    selfPath: `${url.pathname}${url.search}`,
    // The invited address is shown to the token-holder (it is their own mailbox' mail);
    // the session's own email backs the switch page's "you are signed in as".
    invitedEmail: view.email,
    signedInEmail: branch === "other" ? (session?.user?.email ?? "") : null,
    workspaceDisplayName: view.workspaceDisplayName,
    inviterDisplay: view.inviterDisplay,
    role: view.role,
    deliveredCount: view.deliveredCount,
    viaChannels: view.viaChannels,
    hint: view.hint,
    // Account-mint mode: passwordless when a mail sign-in rung exists, else a password field.
    passwordless: Boolean(composition.auth.magicLink),
    mailArmed: mailDelivery().canSend,
    hasDeviceFlow: device !== null,
  };
}

const CREATE_FAILED = "Couldn't finish accepting the invitation. Try the link again.";

export async function action({ request, params }: ActionFunctionArgs) {
  requireBelt(request);
  const token = params.token ?? "";
  const url = new URL(request.url);
  const device = deviceParamsFrom(url);
  const self = `${url.pathname}${url.search}`;
  const form = await request.formData();
  const intent = String(form.get("intent") ?? "");
  const auth = getAuth();

  if (intent === "decline") {
    // Deliberately session-less: possession of the mailed token is the proof, and saying
    // "no thanks" must not require creating an account first.
    const declined = await declineInvitationByToken(token);
    if (declined === "gone") {
      throw redirect(self);
    }
    return { kind: "declined" as const };
  }

  if (intent === "switch") {
    // Sign the CURRENT account out and return here — the page then offers sign-in or the
    // account mint for the invited address. Never accepts as the current account.
    const out = await auth.api.signOut({ headers: request.headers, returnHeaders: true });
    const headers = new Headers();
    for (const cookie of out.headers.getSetCookie()) {
      headers.append("set-cookie", cookie);
    }
    throw redirect(self, { headers });
  }

  if (intent === "send-verification") {
    const session = await auth.api.getSession({ headers: request.headers });
    const email = session?.user?.email;
    if (typeof email !== "string" || email.length === 0 || !mailDelivery().canSend) {
      throw redirect(self);
    }
    await auth.api
      .sendVerificationEmail({ body: { email }, headers: request.headers })
      .catch(() => {});
    return { kind: "verification_sent" as const };
  }

  if (intent === "accept") {
    const session = await auth.api.getSession({ headers: request.headers });
    const actor = actorFromSession(session);
    if (actor === null) {
      throw redirect(self);
    }
    const result = await acceptInvitationByToken(
      token,
      { userId: actor.userId, display: actor.display },
      { mailboxProven: false },
    );
    if (result.outcome !== "accepted") {
      // The loader recomputes the honest state for every refusal (gone → the constant page,
      // wrong account → the switch page, unverified → the round-trip prompt).
      throw redirect(self);
    }
    throw redirect(acceptLanding(result.workspaceName, result.hint, device));
  }

  if (intent === "accept-new") {
    // The account-minting accept — only for a visitor with NO session, and only while the
    // invited address has NO account (decision: the link never signs into an existing
    // account). Both re-checked server-side; the loader's branch is just UI.
    const session = await auth.api.getSession({ headers: request.headers });
    if (actorFromSession(session) !== null) {
      throw redirect(self);
    }
    const resolved = await invitationPageView(token, null);
    if (resolved === null) {
      throw redirect(self);
    }
    if (resolved.branch !== "anon_new") {
      throw redirect(self);
    }
    const invitedEmail = resolved.view.email;
    const name = String(form.get("name") ?? "").trim();
    const passwordless = Boolean(composition.auth.magicLink);

    let userId: string;
    let display: string;
    const cookieHeaders = new Headers();
    if (passwordless) {
      // Mint the account THROUGH Better Auth's own magic-link door, no mail involved: the
      // invite token's delivery to this mailbox is already the proof, so a server-minted
      // single-use sign-in token bridges straight to the verify endpoint (which creates the
      // user with a proven mailbox and mints the session).
      const signIn = await mintInvitationSignIn(invitedEmail);
      const result = await withInvitationCeremony(() =>
        auth.api.magicLinkVerify({
          query: { token: signIn },
          headers: request.headers,
          returnHeaders: true,
        }),
      ).catch(() => null);
      if (result === null) {
        return { kind: "error" as const, message: CREATE_FAILED };
      }
      for (const cookie of result.headers.getSetCookie()) {
        cookieHeaders.append("set-cookie", cookie);
      }
      userId = result.response.user.id;
      display = personDisplay(name, invitedEmail);
      if (name.length > 0) {
        // The optional display name rides the account (the magic-link mint is born with '').
        await auth.api
          .updateUser({ body: { name }, headers: sessionHeaders(request, cookieHeaders) })
          .catch(() => {});
      }
    } else {
      const password = String(form.get("password") ?? "");
      if (password.length < 8) {
        return {
          kind: "error" as const,
          message: "Choose a password of at least 8 characters.",
        };
      }
      display = personDisplay(name, invitedEmail);
      const result = await withInvitationCeremony(() =>
        auth.api.signUpEmail({
          body: { email: invitedEmail, password, name: display },
          returnHeaders: true,
        }),
      ).catch(() => null);
      if (result === null) {
        return { kind: "error" as const, message: CREATE_FAILED };
      }
      for (const cookie of result.headers.getSetCookie()) {
        cookieHeaders.append("set-cookie", cookie);
      }
      userId = result.response.user.id;
    }

    // The accept itself — mailboxProven: holding the mailed token IS the mailbox round-trip,
    // so the fresh account's address is marked verified inside the same transaction.
    const accepted = await acceptInvitationByToken(
      token,
      { userId, display },
      { mailboxProven: true },
    );
    if (accepted.outcome !== "accepted") {
      // A race consumed the token between the resolve and the accept: the account stands (a
      // harmless orphan that can sign in and admits nothing); the page answers constantly.
      throw redirect(self, { headers: cookieHeaders });
    }
    throw redirect(acceptLanding(accepted.workspaceName, accepted.hint, device), {
      headers: cookieHeaders,
    });
  }

  throw redirect(self);
}

/** Merge freshly minted session cookies into a follow-up same-request API call's headers. */
function sessionHeaders(request: Request, minted: Headers): Headers {
  const headers = new Headers(request.headers);
  const cookies: string[] = [];
  const existing = request.headers.get("cookie");
  if (existing !== null) {
    cookies.push(existing);
  }
  for (const setCookie of minted.getSetCookie()) {
    const pair = setCookie.split(";")[0];
    if (pair !== undefined) {
      cookies.push(pair);
    }
  }
  headers.set("cookie", cookies.join("; "));
  return headers;
}

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

export default function InvitePage() {
  const data = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();

  if (actionData?.kind === "declined") {
    return (
      <Shell>
        <PlainState heading="Invitation declined">
          Nothing was joined, and whoever invited you can see you passed. If you change your mind,
          ask them to invite you again.
        </PlainState>
      </Shell>
    );
  }

  if (data.state === "gone") {
    return (
      <Shell>
        <PlainState heading="Nothing to accept here">{data.message}</PlainState>
      </Shell>
    );
  }

  return (
    <Shell>
      <div className="flex flex-col gap-6">
        <Summary data={data} />
        {actionData?.kind === "error" && (
          <p className="text-center text-red-600 text-sm" role="alert">
            {actionData.message}
          </p>
        )}
        {actionData?.kind === "verification_sent" && (
          <p className="text-center text-dim text-sm" role="status">
            Verification mail sent — open it, confirm, then come back to this link.
          </p>
        )}
        <BranchArm data={data} />
      </div>
    </Shell>
  );
}

type ViewData = Extract<Awaited<ReturnType<typeof loader>>, { state: "view" }>;

/** The pre-accept summary: who invited, where to, the role, and what it delivers. */
function Summary({ data }: { data: ViewData }) {
  const delivers =
    data.deliveredCount > 0
      ? `${data.deliveredCount} shared ${data.deliveredCount === 1 ? "skill" : "skills"} via ${data.viaChannels.map((c) => `#${c}`).join(", ")}`
      : "the team's shared skills as they land";
  return (
    <div className="flex flex-col gap-3 text-center">
      <p className="font-medium text-faint text-xs uppercase tracking-wide">Invitation</p>
      <h1 className="font-display font-semibold text-ink text-lg tracking-[-0.02em]">
        {data.inviterDisplay ?? "A member"} invited you to {data.workspaceDisplayName}
      </h1>
      {data.hint !== null && (
        <p className="text-ink text-sm">
          First up: <span className="font-medium">{data.hint.name}</span> ({data.hint.kind}) —
          accepting follows it for you.
        </p>
      )}
      <p className="text-dim text-sm">
        Joining seats you as <Chip tone="accent">{data.role}</Chip> and delivers {delivers} to your
        AI agents — kept current automatically.
      </p>
      <p className="text-faint text-xs">
        This invitation is for <span className="font-mono">{data.invitedEmail}</span>.
      </p>
    </div>
  );
}

function BranchArm({ data }: { data: ViewData }) {
  switch (data.branch) {
    case "match":
      return <OneClickAccept />;
    case "match_unverified":
      return <UnverifiedFence mailArmed={data.mailArmed} />;
    case "other":
      return <SwitchAccount invitedEmail={data.invitedEmail} signedInEmail={data.signedInEmail} />;
    case "anon_existing":
      return <SignInFirst invitedEmail={data.invitedEmail} selfPath={data.selfPath} />;
    default:
      return <CreateAccount passwordless={data.passwordless} />;
  }
}

/** Signed in as the invited address — a live session plus the explicit click is the ceremony. */
function OneClickAccept() {
  const busy = useNavigation().state !== "idle";
  return (
    <div className="flex flex-col gap-3">
      <Form method="post">
        <input type="hidden" name="intent" value="accept" />
        <button
          type="submit"
          disabled={busy}
          className={`${buttonClasses("primary")} min-h-11 w-full`}
        >
          {busy ? "Working…" : "Accept invitation"}
        </button>
      </Form>
      <DeclineArm />
    </div>
  );
}

/** A brand-new person: the account mint, email locked to the invited address. */
function CreateAccount({ passwordless }: { passwordless: boolean }) {
  const busy = useNavigation().state !== "idle";
  return (
    <div className="flex flex-col gap-3">
      <Form method="post" className="flex flex-col gap-3">
        <input type="hidden" name="intent" value="accept-new" />
        <label className="block">
          <span className="mb-1 block font-medium text-dim text-sm">Your name (optional)</span>
          <input
            type="text"
            name="name"
            autoComplete="name"
            className={INPUT}
            placeholder="Ada Lovelace"
          />
        </label>
        {!passwordless && (
          <label className="block">
            <span className="mb-1 block font-medium text-dim text-sm">Choose a password</span>
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
        )}
        <button
          type="submit"
          disabled={busy}
          className={`${buttonClasses("primary")} min-h-11 w-full`}
        >
          {busy ? "Working…" : "Accept and create my account"}
        </button>
        {passwordless && (
          <p className="text-center text-faint text-xs">
            No password needed — this link came to your mailbox, and future sign-ins go the same
            way.
          </p>
        )}
      </Form>
      <DeclineArm />
    </div>
  );
}

/** The invited address already has an account — sign in first, then return here. */
function SignInFirst({ invitedEmail, selfPath }: { invitedEmail: string; selfPath: string }) {
  return (
    <div className="flex flex-col gap-3">
      <Link
        to={`/login?next=${encodeURIComponent(selfPath)}`}
        className={`${buttonClasses("primary")} flex min-h-11 w-full items-center justify-center`}
      >
        Sign in as {invitedEmail} to accept
      </Link>
      <DeclineArm />
    </div>
  );
}

/** Signed in as someone else — name the invited address, offer switching; never accept. */
function SwitchAccount({
  invitedEmail,
  signedInEmail,
}: {
  invitedEmail: string;
  signedInEmail: string | null;
}) {
  const busy = useNavigation().state !== "idle";
  return (
    <div className="flex flex-col gap-3">
      <p className="text-center text-dim text-sm">
        You're signed in as <span className="font-mono text-ink">{signedInEmail ?? "…"}</span>, but
        this invitation is for <span className="font-mono text-ink">{invitedEmail}</span>. Switch
        accounts to accept it.
      </p>
      <Form method="post">
        <input type="hidden" name="intent" value="switch" />
        <button
          type="submit"
          disabled={busy}
          className={`${buttonClasses("primary")} min-h-11 w-full`}
        >
          Sign out and continue as {invitedEmail}
        </button>
      </Form>
      <DeclineArm />
    </div>
  );
}

/** Signed in as the invited address, but the mailbox was never proven — one round-trip first. */
function UnverifiedFence({ mailArmed }: { mailArmed: boolean }) {
  const busy = useNavigation().state !== "idle";
  return (
    <div className="flex flex-col gap-3">
      <p className="text-center text-dim text-sm">
        Your account hasn't verified this address yet — confirm it once and the invitation is yours
        to accept.
      </p>
      {mailArmed ? (
        <Form method="post">
          <input type="hidden" name="intent" value="send-verification" />
          <button
            type="submit"
            disabled={busy}
            className={`${buttonClasses("primary")} min-h-11 w-full`}
          >
            Send the verification mail
          </button>
        </Form>
      ) : (
        <p className="text-center text-faint text-xs">
          This server has no outgoing mail set up — ask its operator to arm SMTP, or to verify your
          address another way.
        </p>
      )}
    </div>
  );
}

/** The quiet decline — recorded; whoever invited you sees it; re-invitable. */
function DeclineArm() {
  const busy = useNavigation().state !== "idle";
  return (
    <Form method="post" className="text-center">
      <input type="hidden" name="intent" value="decline" />
      <button
        type="submit"
        disabled={busy}
        className="text-faint text-xs underline-offset-2 hover:underline"
      >
        Decline this invitation
      </button>
    </Form>
  );
}

function PlainState({ heading, children }: { heading: string; children: ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-2 text-center">
      <p className="font-medium text-faint text-xs uppercase tracking-wide">Invitation</p>
      <h1 className="font-display font-semibold text-ink text-lg tracking-[-0.02em]">{heading}</h1>
      <p className="text-dim text-sm">{children}</p>
    </div>
  );
}

function Shell({ children }: { children: ReactNode }) {
  return (
    <main className="mx-auto flex min-h-dvh w-full max-w-md flex-col justify-center px-4 py-10">
      <div className="rounded-lg border border-line-soft bg-panel p-6 shadow-sm sm:p-8">
        {children}
      </div>
    </main>
  );
}
