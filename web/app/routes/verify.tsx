import { type ReactNode, useEffect, useRef } from "react";
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
import { BusyFields, buttonClasses } from "@/components/ui";
import { composition } from "@/composition.server";
import {
  actorFromSession,
  notFound,
  requireSession,
  type UserActor,
} from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { announceCeremony } from "@/lib/ceremony-event";
import {
  approveLoginFlow,
  denyLoginFlow,
  type PendingLoginFlowView,
  pendingLoginFlow,
  pendingLoginFlowByChallenge,
  seatOf,
  sessionBornStatus,
  theWorkspace,
  workspaceByName,
} from "@/lib/db/identity.server";
import { membershipsFor } from "@/lib/db/queries.server";
import { useSubmittingIntent } from "@/lib/pending";
import { allowVerifyLookup } from "@/lib/rate-limit.server";

export const meta: MetaFunction = () => [{ title: "Approve a login · Topos" }];

/**
 * The ONE login-approve ceremony (the gh-style device-authorization flow's browser half),
 * TWO-STATE: the code-entry form, or the resolved request card. A machine that wants to act
 * as you in ONE workspace shows a short code and points here; a SIGNED-IN person types that
 * code into a POST form — the code never enters ANY URL (no GET lookup, no code-embedding
 * link) — sees exactly what is asking and THE ONE workspace the session will reach, and
 * approves or denies. A LIVE browser session plus the explicit approve click IS the whole
 * ceremony. Denying destroys a pending request and mints nothing.
 *
 * The LOOPBACK arrival (the CLI auto-opened this page): the URL carries `device` — the hex of
 * the flow's device-code HASH — plus `port`/`state` naming the CLI's ephemeral 127.0.0.1
 * listener, so the outcome returns to the terminal via ONE state-bound localhost redirect
 * (the CLI's poll stays the source of truth). `device` does NOT pre-arm the card: it is
 * derivable by whoever started the flow, and starting one needs no credential, so a
 * pre-resolved approve button would be a one-click credential handover to a stranger who
 * mailed the link. The typed code — read off the operator's own terminal — is the out-of-band
 * proof that the approver is the asker, and it is required on every arrival.
 *
 * A flow carrying an INVITATION token discloses the join on the card; the approval ceremony
 * itself weaves accept-the-invitation → approve-the-device into one transaction (identity
 * layer), so sign-in → accept → approve is one visit even for a brand-new invitee.
 */

/** The loopback params a CLI-opened arrival carries (all non-secret), validated by shape. */
interface Loopback {
  port: string;
  state: string;
}

function loopbackFrom(source: { get(name: string): string | null | FormDataEntryValue }): {
  device: string | null;
  loopback: Loopback | null;
} {
  const device = String(source.get("device") ?? "");
  const port = String(source.get("port") ?? "");
  const state = String(source.get("state") ?? "");
  return {
    device: /^[0-9a-f]{64}$/.test(device) ? device : null,
    loopback:
      /^\d{4,5}$/.test(port) &&
      Number(port) >= 1024 &&
      Number(port) <= 65535 &&
      /^[A-Za-z0-9_-]{8,128}$/.test(state)
        ? { port, state }
        : null,
  };
}

/**
 * The localhost hand-off URL. `outcome` wakes the waiting CLI; `code`, present only on an
 * approved LOOPBACK flow, is the authorization code it must present (alongside the device code
 * it already holds) to redeem. The host is a LITERAL — never `localhost`, whose resolution can
 * be hijacked, and never a client-supplied URL.
 */
function loopbackReturn(loopback: Loopback, outcome: string, authCode: string | null): string {
  const qs = new URLSearchParams({ state: loopback.state, outcome });
  if (authCode !== null) {
    qs.set("code", authCode);
  }
  return `http://127.0.0.1:${loopback.port}/cb?${qs.toString()}`;
}

/** This page's own address, re-carrying only the validated pass-through params. */
function selfPath(device: string | null, loopback: Loopback | null): string {
  const qs = new URLSearchParams();
  if (device !== null) {
    qs.set("device", device);
  }
  if (loopback !== null) {
    qs.set("port", loopback.port);
    qs.set("state", loopback.state);
  }
  const search = qs.toString();
  return `/verify${search === "" ? "" : `?${search}`}`;
}

/** The ONE workspace this approval logs the machine into — the resolved card's subject. */
interface LinkTarget {
  name: string;
  displayName: string;
  /** The approver holds no seat there yet (an invitation accept — or `/new` — will seat them). */
  joining: boolean;
  /** The session will be born pending: the session-approval knob is on and the approver (or
   * the invitation's role) is not an owner. */
  awaitsApproval: boolean;
}

/**
 * Resolve the flow's ONE workspace for the card — display only, outside the approve fence
 * (the ceremony re-resolves under its own lock): an invitation names its workspace; otherwise
 * the tenancy grammar decides (single → the install's one workspace, multi → the recorded
 * slug, which may not exist yet — a CLI-first person creates it mid-flow and returns owning
 * it). The approval mints THIS one workspace's session; further workspaces are further logins.
 */
async function linkTargetOf(
  actor: UserActor,
  pending: PendingLoginFlowView,
  multi: boolean,
): Promise<LinkTarget> {
  const ws =
    pending.inviteWorkspace !== null
      ? await workspaceByName(pending.inviteWorkspace.name)
      : multi
        ? await workspaceByName(pending.requestedWorkspace)
        : await theWorkspace();
  if (ws == null) {
    // Multi tenancy, a workspace not created yet: the approver will create it through the
    // `/new` weave and own it — a session its owner mints is born active.
    return {
      name: pending.requestedWorkspace,
      displayName: pending.requestedWorkspace,
      joining: true,
      awaitsApproval: false,
    };
  }
  const seat = await seatOf(actor.userId, ws.id);
  const role = (seat?.role ?? pending.inviteWorkspace?.role ?? "member") as Parameters<
    typeof sessionBornStatus
  >[0];
  const knob = ((ws as { sessionApproval?: string }).sessionApproval ?? "off") as "off" | "on";
  return {
    name: ws.name,
    displayName: ws.displayName,
    joining: seat === undefined,
    awaitsApproval: sessionBornStatus(role, knob) === "pending",
  };
}

export async function loader({ request }: LoaderFunctionArgs) {
  const url = new URL(request.url);
  const { device, loopback } = loopbackFrom(url.searchParams);
  const self = selfPath(device, loopback);
  const actor = actorFromSession(await getAuth().api.getSession({ headers: request.headers }));
  if (actor === null) {
    throw redirect(`/login?next=${encodeURIComponent(self)}`);
  }
  const resolved = device === null ? null : await pendingLoginFlowByChallenge(device);
  const multi = composition.tenancy === "multi";
  const memberships = await membershipsFor(actor);
  if (
    multi &&
    memberships.length === 0 &&
    (resolved === null || resolved.inviteWorkspace === null)
  ) {
    // The workspace-creation weave: a seatless approver cannot approve anything, so route them
    // through `/new` and back — unless the flow carries an invitation, whose accept will seat
    // them right here.
    const prefill =
      resolved !== null && resolved.requestedWorkspace !== ""
        ? `&name=${encodeURIComponent(resolved.requestedWorkspace)}`
        : "";
    throw redirect(`/new?next=${encodeURIComponent(self)}${prefill}`);
  }
  // The card resolves with ZERO TYPING — but only ever for a LOOPBACK-bound flow, and that
  // restriction IS the safety argument. `device` is the device code's own hash, so anyone who
  // started a flow can compute it, and starting one needs no credential. What a stranger cannot
  // do is receive the answer: a loopback approval hands its authorization code to the approver's
  // own 127.0.0.1 listener, so a phished click completes a login on the VICTIM's machine while
  // the sender's poll keeps reading `awaiting_redirect`. `pendingLoginFlowByChallenge` enforces
  // the binding in SQL — a DEVICE-bound flow resolves to nothing here and falls through to the
  // typed-code form, which stays its only door.
  return {
    multi,
    device,
    loopback,
    resolved:
      resolved === null
        ? null
        : { ...resolved, linked: await linkTargetOf(actor, resolved, multi) },
  };
}

const REQUEST_GONE =
  "That request expired or was already handled — nothing was approved. Ask the device to start again.";

export async function action({ request }: ActionFunctionArgs) {
  // A POST has no query to preserve — the plain guard's /login bounce is right here.
  const session = await requireSession(request);
  const actor = actorFromSession(session);
  if (actor === null) {
    notFound();
  }
  const form = await request.formData();
  const intent = String(form.get("intent") ?? "");
  const { loopback } = loopbackFrom(form);

  // ONE belt over all three arms: `approve` and `deny` resolve a flow by the same typed code
  // `lookup` does, so leaving them unbelted would just move an enumeration loop one intent to
  // the left. Page actions never reach the /api/v1 door belt, so the action wears it.
  if (!allowVerifyLookup(actor.userId)) {
    throw data(null, { status: 429 });
  }

  if (intent === "lookup") {
    // The two-state page's first state: resolve the typed code into the request card. A POST,
    // deliberately — the code never rides a URL (history, logs, referers all stay clean).
    const userCode = String(form.get("code") ?? "")
      .trim()
      .toUpperCase();
    if (userCode === "") {
      throw data(null, { status: 400 });
    }
    const pending = await pendingLoginFlow(userCode);
    if (pending === null) {
      return { kind: "miss" as const };
    }
    return {
      kind: "resolved" as const,
      pending: {
        ...pending,
        linked: await linkTargetOf(actor, pending, composition.tenancy === "multi"),
      },
    };
  }

  const userCode = String(form.get("code") ?? "").trim();
  if (userCode === "" || (intent !== "approve" && intent !== "deny")) {
    throw data(null, { status: 400 });
  }

  if (intent === "approve") {
    // No re-authentication: the live session + this explicit approve click is the whole ceremony. The
    // ceremony itself resolves the flow's workspace and requires the approver's seat in it —
    // accepting a carried invitation first when the approver is its addressee — and a refusal
    // is indistinguishable from an expired code.
    // A LOOPBACK flow's authorization code has exactly ONE way out: the redirect to the asking
    // machine's listener. Approving one from a page that carries no return coordinates — a
    // second browser, a phone, the bare /verify address the terminal also prints — would record
    // consent and mint a code with nowhere to go, leaving that terminal waiting out the whole
    // TTL. Refuse BEFORE the ceremony: nothing is consumed, the flow stays pending, and it is
    // still finishable where it started. (The ceremony re-checks under its own lock; reaching
    // that path means a race and gets the uniform refusal.)
    if (loopback === null) {
      const card = await pendingLoginFlow(userCode);
      if (card?.binding === "loopback") {
        return data(
          {
            kind: "refused" as const,
            error:
              "Finish this login on the machine that started it — the approval goes straight " +
              "back to that terminal. Use the page it opened, or run `topos login` there again.",
          },
          { status: 400 },
        );
      }
    }
    const approved = await approveLoginFlow(
      userCode,
      { userId: actor.userId, display: actor.display },
      loopback !== null,
    );
    if (approved === null) {
      return data({ kind: "refused" as const, error: REQUEST_GONE }, { status: 400 });
    }
    if (loopback !== null) {
      // The state-bound localhost hand-off. For a LOOPBACK flow this carries the AUTHORIZATION
      // CODE — the second of the two secrets the exchange needs, and the one only this browser
      // can deliver, because the redirect resolves on the approver's OWN machine. A phisher who
      // mailed this link watches their victim complete a login they can never collect.
      throw redirect(loopbackReturn(loopback, "approved", approved.authCode));
    }
    return { kind: "approved" as const, name: approved.requestedName };
  }

  const denied = await denyLoginFlow(userCode, {
    userId: actor.userId,
    display: actor.display,
  });
  if (!denied) {
    return data({ kind: "refused" as const, error: REQUEST_GONE }, { status: 400 });
  }
  if (loopback !== null) {
    throw redirect(loopbackReturn(loopback, "denied", null));
  }
  return { kind: "denied" as const };
}

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

export default function VerifyPage() {
  const { resolved, loopback } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();

  // The `login_approved` ceremony announcement, fired ONCE when the approval-success state
  // renders. Ref-guarded so dev strict-mode's doubled effect and re-renders of the same
  // success never re-dispatch; leaving the success state (a fresh lookup) re-arms it for the
  // next distinct approval.
  const approved =
    actionData !== undefined && "kind" in actionData && actionData.kind === "approved";
  const announcedApproval = useRef(false);
  useEffect(() => {
    if (!approved) {
      announcedApproval.current = false;
      return;
    }
    if (announcedApproval.current) {
      return;
    }
    announcedApproval.current = true;
    announceCeremony("login_approved");
  }, [approved]);

  if (actionData !== undefined && "kind" in actionData && actionData.kind === "approved") {
    return (
      <Shell>
        <PlainState heading="Logged in">
          {actionData.name} is logged in — you can close this tab.
        </PlainState>
      </Shell>
    );
  }
  if (actionData !== undefined && "kind" in actionData && actionData.kind === "denied") {
    return (
      <Shell>
        <PlainState heading="Request denied">
          Nothing was connected — the machine is told on its next poll.
        </PlainState>
      </Shell>
    );
  }

  // The resolved card: from the loopback challenge (loader) or the typed-code lookup (action).
  const card =
    actionData !== undefined && "kind" in actionData && actionData.kind === "resolved"
      ? actionData.pending
      : resolved;

  return (
    <Shell>
      <div className="flex flex-col gap-6">
        <div className="flex flex-col gap-2 text-center">
          <p className="font-medium text-faint text-xs uppercase tracking-wide">Login approval</p>
          <h1 className="font-display font-semibold text-ink text-lg tracking-[-0.02em]">
            Approve a login
          </h1>
          {card === null && (
            <p className="text-dim text-sm">
              Enter the code your terminal shows — it looks like{" "}
              <code className="font-mono text-ink">AB29-CD34</code>.
            </p>
          )}
        </div>
        {actionData !== undefined && "kind" in actionData && actionData.kind === "refused" && (
          <p className="text-center text-red-600 text-sm" role="alert">
            {actionData.error}
          </p>
        )}
        {actionData !== undefined && "kind" in actionData && actionData.kind === "miss" && (
          <p className="text-center text-dim text-sm" role="status">
            No pending request for that code — it may have expired, or a character is off. Check
            your terminal and try again.
          </p>
        )}
        {card !== null ? <PendingRequest card={card} loopback={loopback} /> : <CodeLookup />}
      </div>
    </Shell>
  );
}

/** State one: the code form — a POST, so the code never lands in a URL. */
function CodeLookup() {
  const busy = useSubmittingIntent() !== null;
  return (
    <Form method="post">
      <input type="hidden" name="intent" value="lookup" />
      <BusyFields busy={busy} className="flex items-end gap-2">
        <label className="block flex-1">
          <span className="mb-1 block font-medium text-dim text-sm">Code</span>
          <input
            type="text"
            name="code"
            required
            autoComplete="off"
            spellCheck={false}
            className={`${INPUT} font-mono uppercase`}
            placeholder="AB29-CD34"
          />
        </label>
        <button type="submit" className={`${buttonClasses("quiet")} min-h-11`}>
          {busy ? "Looking up…" : "Look up"}
        </button>
      </BusyFields>
    </Form>
  );
}

/** One resolved-card row shape shared by loader (challenge) and action (lookup) arrivals. */
interface ResolvedCard extends PendingLoginFlowView {
  linked: LinkTarget;
}

/**
 * State two: the resolved request. What is asking, the CODE for the glance-check against the
 * terminal, THE ONE workspace being logged into (a login is one workspace's session — further
 * workspaces are further logins), and the two arms. The approve form posts the RESOLVED code
 * as a hidden field — the approval applies to exactly the request shown, never to whatever a
 * lookup input held.
 */
function PendingRequest({ card, loopback }: { card: ResolvedCard; loopback: Loopback | null }) {
  // WHICH arm is on the wire — both disable together (one decision per request), but only the one
  // that was clicked names its wait. Without this the approve button read "Working…" for a deny.
  const flying = useSubmittingIntent();
  const submitting = flying !== null;
  const passThrough = (
    <>
      {loopback !== null && (
        <>
          <input type="hidden" name="port" value={loopback.port} />
          <input type="hidden" name="state" value={loopback.state} />
        </>
      )}
    </>
  );
  return (
    <div className="flex flex-col gap-4 rounded-md border border-line-soft bg-ground p-4">
      <p className="text-ink text-sm">
        <span className="font-medium">“{card.requestedName}”</span> wants to log in as you.
      </p>
      <p className="text-dim text-sm">
        Its code is <code className="font-mono text-ink">{card.userCode}</code> — confirm it matches
        your terminal before approving.
      </p>
      <div className="text-dim text-sm">
        <p>
          Approving logs it into{" "}
          <span className="font-medium text-ink">{card.linked.displayName}</span>
          {card.linked.joining && <span className="text-faint"> — which you join on approve</span>}.
        </p>
        {card.inviteWorkspace !== null && (
          <p className="mt-2">
            This login carries an invitation to{" "}
            <span className="font-medium text-ink">{card.inviteWorkspace.displayName}</span> —
            approving accepts it.
          </p>
        )}
        {card.linked.awaitsApproval && (
          <p className="mt-2">
            Session approval is on there: the session waits until a workspace owner approves it —
            nothing is delivered before that.
          </p>
        )}
        <p className="mt-2">
          It publishes, syncs, and reads there until you end the session — from your sessions page
          or with topos logout. Any further workspace is its own login.
        </p>
      </div>
      <Form method="post" className="flex flex-col gap-3">
        <input type="hidden" name="intent" value="approve" />
        <input type="hidden" name="code" value={card.userCode} />
        {passThrough}
        <button
          type="submit"
          disabled={submitting}
          className={`${buttonClasses("primary")} min-h-11 w-full`}
        >
          {flying === "approve" ? "Approving…" : `Approve “${card.requestedName}”`}
        </button>
      </Form>
      <Form method="post">
        <input type="hidden" name="intent" value="deny" />
        <input type="hidden" name="code" value={card.userCode} />
        {passThrough}
        <button
          type="submit"
          disabled={submitting}
          className={`${buttonClasses("danger")} min-h-11 w-full`}
        >
          {flying === "deny" ? "Denying…" : "Deny — this isn’t me"}
        </button>
      </Form>
    </div>
  );
}

function PlainState({ heading, children }: { heading: string; children: ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-2 text-center">
      <p className="font-medium text-faint text-xs uppercase tracking-wide">Login approval</p>
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
