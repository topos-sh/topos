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
  useNavigation,
} from "react-router";
import { buttonClasses } from "@/components/ui";
import { composition } from "@/composition.server";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { announceCeremony } from "@/lib/ceremony-event";
import {
  approveDeviceAuth,
  denyDeviceAuth,
  type PendingDeviceAuthView,
  pendingDeviceAuth,
  pendingDeviceAuthByChallenge,
} from "@/lib/db/identity.server";
import { membershipsFor } from "@/lib/db/queries.server";

export const meta: MetaFunction = () => [{ title: "Approve a device · Topos" }];

/**
 * The ONE device-approve ceremony (the gh-style device flow's browser half), TWO-STATE: the
 * code-entry form, or the resolved request card. A device that wants to act as you shows a
 * short code and points here; a SIGNED-IN person types that code into a POST form — the code
 * never enters ANY URL (no GET lookup, no code-embedding link) — sees exactly what is asking
 * and EVERYWHERE the credential will reach, and approves or denies. A LIVE SESSION plus the
 * explicit approve click IS the whole ceremony. Denying destroys a pending request and mints
 * nothing.
 *
 * The LOOPBACK arrival (the CLI auto-opened this page): the URL carries `device` — the hex of
 * the flow's device-code HASH (the same value the store keys the row by; identifying, never
 * secret) — so the card resolves with zero typing, plus `port`/`state` naming the CLI's
 * ephemeral 127.0.0.1 listener. The outcome then returns to the terminal via ONE state-bound
 * localhost redirect; the CLI's poll stays the source of truth.
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

/**
 * The resolved card's workspace-reach list: EVERY workspace the minted credential will act in —
 * the approver's existing seats, plus the flow's requested workspace and any invitation join
 * when those aren't seats yet. Full disclosure: the credential is device-wide, not
 * workspace-scoped.
 */
function reachOf(
  memberships: { displayName: string; address: string }[],
  pending: PendingDeviceAuthView,
  multi: boolean,
): { label: string; joining: boolean }[] {
  const rows = memberships.map((m) => ({ label: m.displayName, joining: false }));
  const seatAddresses = new Set(memberships.map((m) => m.address));
  if (pending.inviteWorkspace !== null && !seatAddresses.has(pending.inviteWorkspace.name)) {
    rows.push({ label: pending.inviteWorkspace.displayName, joining: true });
  } else if (
    multi &&
    pending.requestedWorkspace !== "" &&
    !seatAddresses.has(pending.requestedWorkspace)
  ) {
    rows.push({ label: pending.requestedWorkspace, joining: true });
  }
  return rows;
}

export async function loader({ request }: LoaderFunctionArgs) {
  const url = new URL(request.url);
  const { device, loopback } = loopbackFrom(url.searchParams);
  const self = selfPath(device, loopback);
  const actor = actorFromSession(await getAuth().api.getSession({ headers: request.headers }));
  if (actor === null) {
    throw redirect(`/login?next=${encodeURIComponent(self)}`);
  }
  const resolved = device === null ? null : await pendingDeviceAuthByChallenge(device);
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
  return {
    multi,
    device,
    loopback,
    resolved:
      resolved === null ? null : { ...resolved, reach: reachOf(memberships, resolved, multi) },
    memberships: memberships.map((m) => ({ displayName: m.displayName, address: m.address })),
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

  if (intent === "lookup") {
    // The two-state page's first state: resolve the typed code into the request card. A POST,
    // deliberately — the code never rides a URL (history, logs, referers all stay clean).
    const userCode = String(form.get("code") ?? "")
      .trim()
      .toUpperCase();
    if (userCode === "") {
      throw data(null, { status: 400 });
    }
    const pending = await pendingDeviceAuth(userCode);
    if (pending === null) {
      return { kind: "miss" as const };
    }
    const memberships = await membershipsFor(actor);
    return {
      kind: "resolved" as const,
      pending: {
        ...pending,
        reach: reachOf(
          memberships.map((m) => ({ displayName: m.displayName, address: m.address })),
          pending,
          composition.tenancy === "multi",
        ),
      },
    };
  }

  const userCode = String(form.get("code") ?? "").trim();
  if (userCode === "" || (intent !== "approve" && intent !== "deny")) {
    throw data(null, { status: 400 });
  }

  if (intent === "approve") {
    // No step-up: the live session + this explicit approve click is the whole ceremony. The
    // ceremony itself resolves the flow's workspace and requires the approver's seat in it —
    // accepting a carried invitation first when the approver is its addressee — and a refusal
    // is indistinguishable from an expired code.
    const approved = await approveDeviceAuth(userCode, {
      userId: actor.userId,
      display: actor.display,
    });
    if (approved === null) {
      return data({ kind: "refused" as const, error: REQUEST_GONE }, { status: 400 });
    }
    if (loopback !== null) {
      // The state-bound single-use localhost return: outcome only, no secret — the CLI's poll
      // delivers the credential as always.
      throw redirect(
        `http://127.0.0.1:${loopback.port}/cb?state=${encodeURIComponent(loopback.state)}&outcome=approved`,
      );
    }
    return { kind: "approved" as const, name: approved.requestedName };
  }

  const denied = await denyDeviceAuth(userCode, {
    userId: actor.userId,
    display: actor.display,
  });
  if (!denied) {
    return data({ kind: "refused" as const, error: REQUEST_GONE }, { status: 400 });
  }
  if (loopback !== null) {
    throw redirect(
      `http://127.0.0.1:${loopback.port}/cb?state=${encodeURIComponent(loopback.state)}&outcome=denied`,
    );
  }
  return { kind: "denied" as const };
}

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

export default function VerifyPage() {
  const { resolved, loopback } = useLoaderData<typeof loader>();
  const actionData = useActionData<typeof action>();

  // The `device_approved` ceremony announcement, fired ONCE when the approval-success state
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
    announceCeremony("device_approved");
  }, [approved]);

  if (actionData !== undefined && "kind" in actionData && actionData.kind === "approved") {
    return (
      <Shell>
        <PlainState heading="Device connected">
          {actionData.name} is connected — you can close this tab; the device picks the approval up
          on its next poll.
        </PlainState>
      </Shell>
    );
  }
  if (actionData !== undefined && "kind" in actionData && actionData.kind === "denied") {
    return (
      <Shell>
        <PlainState heading="Request denied">
          Nothing was connected — the device is told on its next poll.
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
          <p className="font-medium text-faint text-xs uppercase tracking-wide">Device approval</p>
          <h1 className="font-display font-semibold text-ink text-lg tracking-[-0.02em]">
            Approve a device
          </h1>
          {card === null && (
            <p className="text-dim text-sm">
              Enter the code your device shows — it looks like{" "}
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
  const busy = useNavigation().state !== "idle";
  return (
    <Form method="post" className="flex items-end gap-2">
      <input type="hidden" name="intent" value="lookup" />
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
      <button type="submit" disabled={busy} className={`${buttonClasses("quiet")} min-h-11`}>
        Look up
      </button>
    </Form>
  );
}

/** One resolved-card row shape shared by loader (challenge) and action (lookup) arrivals. */
interface ResolvedCard extends PendingDeviceAuthView {
  reach: { label: string; joining: boolean }[];
}

/**
 * State two: the resolved request. What is asking, the CODE for the glance-check against the
 * terminal, EVERY workspace the credential will reach, and the two arms. The approve form
 * posts the RESOLVED code as a hidden field — the approval applies to exactly the request
 * shown, never to whatever a lookup input held.
 */
function PendingRequest({ card, loopback }: { card: ResolvedCard; loopback: Loopback | null }) {
  const navigation = useNavigation();
  const submitting = navigation.state !== "idle";
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
        Device <span className="font-medium">“{card.requestedName}”</span> wants to act as you.
      </p>
      <p className="text-dim text-sm">
        Its code is <code className="font-mono text-ink">{card.userCode}</code> — confirm it matches
        your terminal before approving.
      </p>
      <div className="text-dim text-sm">
        <p className="mb-1">Approving gives it a credential that acts with your seat in:</p>
        <ul className="list-inside list-disc">
          {card.reach.map((row) => (
            <li key={`${row.label}-${row.joining}`}>
              <span className="text-ink">{row.label}</span>
              {row.joining && <span className="text-faint"> — joins on approve</span>}
            </li>
          ))}
        </ul>
        {card.inviteWorkspace !== null && (
          <p className="mt-2">
            This enrollment carries an invitation to{" "}
            <span className="font-medium text-ink">{card.inviteWorkspace.displayName}</span> —
            approving accepts it.
          </p>
        )}
        <p className="mt-2">
          It publishes, follows, and reads there until you sign it out from your devices.
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
          {submitting ? "Working…" : `Approve “${card.requestedName}”`}
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
          Deny — this isn’t my device
        </button>
      </Form>
    </div>
  );
}

function PlainState({ heading, children }: { heading: string; children: ReactNode }) {
  return (
    <div className="flex flex-col items-center gap-2 text-center">
      <p className="font-medium text-faint text-xs uppercase tracking-wide">Device approval</p>
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
