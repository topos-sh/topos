import { type ReactNode, useEffect, useRef, useState } from "react";
import {
  type ActionFunctionArgs,
  data,
  Form,
  type LoaderFunctionArgs,
  type MetaFunction,
  redirect,
  useActionData,
  useFetcher,
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
  type LoginApproveChoice,
  type PendingInvitationChoice,
  type PendingLoginFlowView,
  pendingInvitationsFor,
  pendingLoginFlow,
  pendingLoginFlowByChallenge,
  type SeatChoice,
  seatChoicesFor,
  theWorkspace,
} from "@/lib/db/identity.server";
import { createWorkspacePrecheck, workspaceNameAvailable } from "@/lib/db/workspace-create.server";
import { useSubmittingIntent } from "@/lib/pending";
import { publicOrigin } from "@/lib/plane/public-base.server";
import { allowVerifyLookup } from "@/lib/rate-limit.server";
import { type Loopback, loopbackFrom, verifySelfPath } from "@/lib/verify-path";
import {
  ADDRESS_TAKEN,
  CREATE_RATE_LIMITED,
  NAME_REQUIRED,
  SLUG_SHAPE,
  WORKSPACE_LIMIT,
} from "@/lib/workspace-create-copy";
import {
  isWorkspaceNameShape,
  toWorkspaceSlug,
  toWorkspaceSlugDraft,
  WORKSPACE_NAME_MAX,
} from "@/lib/workspace-name";

export const meta: MetaFunction = () => [{ title: "Approve a login · Topos" }];

/**
 * The ONE login-approve ceremony — the pick-or-create-and-approve page. A machine that wants
 * to act as you shows a short code and points here; a SIGNED-IN person types that code into a
 * POST form — the code never enters ANY URL (no GET lookup, no code-embedding link) — sees
 * exactly what is asking, CHOOSES THE WORKSPACE the session will reach (a seat they hold, a
 * pending invitation of theirs, or — where creation is open — a brand-new workspace born in
 * the same act), and approves or denies. A live browser session plus the explicit click
 * records consent + the chosen workspace; NOTHING is minted here — the CLI's next poll runs
 * the exchange that mints the session, so approval from ANY browser on ANY device completes
 * the login. Denying destroys a pending request and mints nothing.
 *
 * The LOOPBACK arrival (the CLI auto-opened this page): the URL carries `device` — the hex of
 * the flow's device-code HASH — plus `port`/`state` naming the CLI's ephemeral 127.0.0.1
 * listener. The redirect back to that listener is a PURE ACCELERATOR (state + outcome only —
 * no secret rides it): it wakes the waiting poll instead of leaving it to its interval. The
 * challenge pre-arms the card for a LOOPBACK flow only — for a device-bound flow the typed
 * code, read off the operator's own terminal, stays the out-of-band proof that binds approver
 * to asker.
 *
 * A flow carrying an INVITATION token pre-binds the card to the invited workspace when the
 * token still resolves for this account; the approval weaves accept-the-invitation →
 * approve into one transaction (identity layer), so sign-in → accept → approve is one visit
 * even for a brand-new invitee.
 */

/**
 * The localhost hand-off URL — `state` + `outcome` ONLY. The poll is the one completion
 * mechanism; this redirect merely wakes it. The host is a LITERAL — never `localhost`, whose
 * resolution can be hijacked, and never a client-supplied URL.
 */
function loopbackReturn(loopback: Loopback, outcome: string): string {
  const qs = new URLSearchParams({ state: loopback.state, outcome });
  return `http://127.0.0.1:${loopback.port}/cb?${qs.toString()}`;
}

/** The chooser's server-read half: the viewer's standing options, display only. */
interface ChooserData {
  seats: SeatChoice[];
  invitations: PendingInvitationChoice[];
  /** Invitations exist for the viewer's address but the mailbox is unproven — say so. */
  heldUnverified: boolean;
  /** Multi tenancy with workspace creation open — the create option renders. */
  createAllowed: boolean;
  /** Single tenancy: the boot workspace is still unclaimed (the guidance mentions setup). */
  bootUnclaimed: boolean;
}

async function chooserFor(actor: UserActor): Promise<ChooserData> {
  const multi = composition.tenancy === "multi";
  const seats = await seatChoicesFor(actor.userId);
  const { invitations, heldUnverified } = await pendingInvitationsFor(actor.userId);
  const createAllowed =
    multi && (await composition.entitlements.forWorkspace(null)).allows("workspace-create");
  const bootUnclaimed = multi ? false : ((await theWorkspace())?.claimedAt ?? null) === null;
  return { seats, invitations, heldUnverified, createAllowed, bootUnclaimed };
}

export async function loader({ request }: LoaderFunctionArgs) {
  const url = new URL(request.url);
  const { device, loopback } = loopbackFrom(url.searchParams);
  const self = verifySelfPath(device, loopback);
  const actor = actorFromSession(await getAuth().api.getSession({ headers: request.headers }));
  if (actor === null) {
    throw redirect(`/login?next=${encodeURIComponent(self)}`);
  }
  // The create option's live-availability probe — the same `?check=` arm /new answers, for the
  // inline create form's debounced fetcher. Signed-in only (the bounce above ran first).
  const check = url.searchParams.get("check");
  if (check !== null) {
    return { name: check, available: await workspaceNameAvailable(check) };
  }
  // The card resolves with ZERO TYPING — but only ever for a LOOPBACK-bound flow
  // (`pendingLoginFlowByChallenge` enforces the binding in SQL). The challenge is the device
  // code's own hash, so anyone who started a flow can compute it; the typed code stays the
  // device flow's only door.
  const resolved = device === null ? null : await pendingLoginFlowByChallenge(device, actor.userId);
  return {
    multi: composition.tenancy === "multi",
    device,
    loopback,
    resolved,
    chooser: await chooserFor(actor),
    origin: publicOrigin(request),
  };
}

/** The guessing belt's own in-page line, on whichever arm ran the person out of tokens. A refusal
 * a PERSON meets has to be a sentence on the form they are standing at, with the wait in it — the
 * house fault screen says nothing they can act on, and mistyping a code is the commonest way to
 * arrive here. */
const TOO_MANY_LOOKUPS = "Too many attempts — wait a few seconds and try again";

/**
 * THE CODE'S OWN SHAPE — `XXXX-XXXX`: eight characters from the unambiguous alphabet plus the
 * hyphen that groups them for reading aloud. It is the field's maxlength AND the server's cap:
 * the input used to take any amount of text, so a stray paste (a whole command line, a URL) went
 * to the lookup as if it might be a code.
 */
const USER_CODE_LENGTH = 9;

/** The empty submit's answer. A form that does nothing and says nothing on a click is the one
 *  outcome a person cannot learn anything from — this is the same sentence the page opens with,
 *  said again where the click happened. */
const CODE_REQUIRED = "Enter the code your terminal shows";

const REQUEST_GONE =
  "That request expired or was already handled — nothing was approved. Ask the device to start again.";

/**
 * The answer a DECISION arm gives when the typed code resolved nothing — expired, already
 * handled, or never a code at all. It is the only shape a guess can take on `approve` and
 * `deny`, so it is where those two arms pay the belt.
 *
 * A decision on a code that RESOLVES costs nothing: the card was already in front of the person,
 * and the act of deciding it is not a guess — which is why ten mistyped lookups must never lock
 * out the approve of the request on screen. A loop of guesses pays for every miss instead, and
 * meets the same wall the lookup does, in the same words: the two doors share one bucket, so
 * spreading a scan across both buys nothing.
 */
function decisionMissed(actor: UserActor) {
  return allowVerifyLookup(actor.userId)
    ? data({ kind: "refused" as const, error: REQUEST_GONE }, { status: 400 })
    : data({ kind: "refused" as const, error: TOO_MANY_LOOKUPS }, { status: 429 });
}

/** Parse the chooser's posted pick into the ceremony's choice. Null = the invite-token
 * pre-bound arm (valid only while the flow's token binds — the fence decides). */
function choiceFromPick(
  pick: string,
  form: FormData,
): LoginApproveChoice | null | { invalid: true } {
  if (pick === "invite-token") {
    return null;
  }
  if (pick.startsWith("seat:")) {
    return { kind: "seat", workspace: pick.slice("seat:".length) };
  }
  if (pick.startsWith("invitation:")) {
    return { kind: "invitation", id: pick.slice("invitation:".length) };
  }
  if (pick === "create") {
    return {
      kind: "create",
      displayName: String(form.get("displayName") ?? "").trim(),
      // The FULL slug rule applies at submit: the field holds the keystroke-tolerant draft
      // (a trailing hyphen mid-typing survives there), and the canonical spelling is what
      // the ceremony sees — same rule the form applies on blur.
      slug: toWorkspaceSlug(String(form.get("slug") ?? "")),
    };
  }
  return { invalid: true };
}

export async function action({ request }: ActionFunctionArgs) {
  // A POST has no query to preserve — the plain guard's /login bounce is right here.
  const session = await requireSession(request);
  const actor = actorFromSession(session);
  if (actor === null) {
    notFound();
  }
  const form = await request.formData();
  const intent = String(form.get("intent") ?? "");
  // `device` rides the challenge-resolved card's forms: the wake-up redirect below fires only
  // when the decided flow IS that exact loopback-bound flow (server-compared) — a typed-code
  // card approved from a loopback-armed URL must not spend the listener's one wake.
  const { device, loopback } = loopbackFrom(form);

  // THE LOOKUP IS THE DEFAULT ARM. `approve` and `deny` are the two acts that DECIDE a request,
  // and each names itself; every other submission of this page is the code lookup — including one
  // that arrives with no `intent` field at all.
  //
  // Pressing Enter in the code field IS clicking "Look up", and it has to answer like it. A
  // submission can reach here without the form's hidden field for reasons the person who typed
  // the code cannot see (a form re-submitted by something else on the page carries no submitter;
  // a script or extension between the two can drop a hidden input), and keying the lookup on that
  // field being present turned every one of those into a bare 400 — a page with no card, no
  // message, and nothing to do next. Reading the lookup as the default costs nothing: it resolves
  // only a flow THIS actor may approve, and it is the arm the guessing belt sits on, so a
  // submission that loses a hidden field loses no protection with it.
  if (intent !== "approve" && intent !== "deny") {
    // The two-state page's first state: resolve the typed code into the request card. A POST,
    // deliberately — the code never rides a URL (history, logs, referers all stay clean).
    const userCode = String(form.get("code") ?? "")
      .trim()
      .toUpperCase();
    if (userCode === "") {
      // In the page, not the house fault screen: an empty submit is the commonest thing a person
      // does by accident, and it has an answer they can act on.
      return data({ kind: "refused" as const, error: CODE_REQUIRED }, { status: 400 });
    }
    // Longer than a code can be: answered like any other code that names nothing, and for free —
    // a string that cannot be a code is not a guess of the code space, so it never spends the belt
    // and never reaches the database.
    if (userCode.length > USER_CODE_LENGTH) {
      return { kind: "miss" as const };
    }
    // THE GUESSING BELT, keyed per acting person and spent HERE — on the one arm that turns a
    // typed code into a request. A code space of ~2^40 has to meet a wall long before it matters,
    // and this is the only door that answers "is this a code at all". Deciding a request you have
    // already looked up is not a guess, so `approve` and `deny` spend nothing: the person with a
    // card in front of them can always act on it, however many codes they mistyped getting there.
    // Page actions never reach the /api/v1 door belt, so the action wears its own.
    if (!allowVerifyLookup(actor.userId)) {
      return data({ kind: "refused" as const, error: TOO_MANY_LOOKUPS }, { status: 429 });
    }
    const pending = await pendingLoginFlow(userCode, actor.userId);
    if (pending === null) {
      return { kind: "miss" as const };
    }
    return { kind: "resolved" as const, pending };
  }

  const userCode = String(form.get("code") ?? "").trim();
  if (userCode === "") {
    throw data(null, { status: 400 });
  }

  if (intent === "approve") {
    // No re-authentication: the live session + this explicit approve click is the whole
    // ceremony. The fence itself validates the chosen standing (seat / invitation / creation)
    // under its own lock — an ordinary refusal is indistinguishable from an expired code; only
    // the create arm answers typed.
    const pick = String(form.get("pick") ?? "");
    const choice = choiceFromPick(pick, form);
    if (typeof choice === "object" && choice !== null && "invalid" in choice) {
      throw data(null, { status: 400 });
    }
    if (choice !== null && choice.kind === "create") {
      // The SAME surface pre-check /new runs (tenancy + the entitlement gate), then the field
      // validation — byte-identical refusal strings, then the fence (which owns the counted
      // per-person floors, under the same advisory lock as /new's transaction). Existence
      // FIRST: on single tenancy (or creation switched off) the option does not exist, so a
      // crafted POST answers the house 404 whatever its fields say.
      const precheck = await createWorkspacePrecheck();
      if (precheck === "off") {
        notFound();
      }
      if (choice.displayName.length < 1 || choice.displayName.length > 100) {
        return await createRefused(userCode, actor, NAME_REQUIRED, choice, 400);
      }
      if (!isWorkspaceNameShape(choice.slug)) {
        return await createRefused(userCode, actor, SLUG_SHAPE, choice, 400);
      }
    }
    const approved = await approveLoginFlow(
      userCode,
      { userId: actor.userId, display: actor.display },
      choice,
    );
    if (approved === null) {
      return decisionMissed(actor);
    }
    if (approved.outcome === "taken") {
      // The whole transaction rolled back — the flow is still pending, so re-resolve the card
      // and surface the typed error on the create form for the retry.
      const choiceCreate = choice as Extract<LoginApproveChoice, { kind: "create" }>;
      return await createRefused(userCode, actor, ADDRESS_TAKEN, choiceCreate, 400);
    }
    if (approved.outcome === "rate-limited" || approved.outcome === "owned-limit") {
      // The create arm's counted floors, refused inside the fence — same re-resolve, the
      // matching honest string on the form.
      const choiceCreate = choice as Extract<LoginApproveChoice, { kind: "create" }>;
      return approved.outcome === "rate-limited"
        ? await createRefused(userCode, actor, CREATE_RATE_LIMITED, choiceCreate, 429)
        : await createRefused(userCode, actor, WORKSPACE_LIMIT, choiceCreate, 403);
    }
    if (approved.outcome === "workspace-full") {
      // An invitation-bound approval met the member limit: the flow AND the invitation stay
      // pending (approving again works once there is room), and the card says the real reason.
      return data(
        { kind: "refused" as const, error: "This workspace is at its member limit." },
        { status: 409 },
      );
    }
    if (loopback !== null && device !== null && approved.flowChallenge === device) {
      // The state-bound localhost hand-off — a pure accelerator that wakes the waiting CLI;
      // its poll (the one completion mechanism) then runs the exchange that mints. Fired
      // ONLY when the flow just approved is the exact loopback flow this page arrived armed
      // for (its challenge, server-compared) — deciding any other card from the same URL
      // shows the plain success and leaves the listener's one wake unspent.
      throw redirect(loopbackReturn(loopback, "approved"));
    }
    return {
      kind: "approved" as const,
      name: approved.requestedName,
      workspaceDisplay: approved.workspaceDisplay,
    };
  }

  const denied = await denyLoginFlow(userCode, {
    userId: actor.userId,
    display: actor.display,
  });
  if (denied === null) {
    return decisionMissed(actor);
  }
  if (loopback !== null && device !== null && denied.flowChallenge === device) {
    throw redirect(loopbackReturn(loopback, "denied"));
  }
  return { kind: "denied" as const };
}

/** The create arm's typed refusal: re-resolve the still-pending card so the form survives the
 * round-trip with the error beside it (a vanished flow falls back to the uniform gone). */
async function createRefused(
  userCode: string,
  actor: { userId: string },
  error: string,
  choice: { displayName: string; slug: string },
  status: number,
) {
  const pending = await pendingLoginFlow(userCode.toUpperCase(), actor.userId);
  if (pending === null) {
    return data({ kind: "refused" as const, error: REQUEST_GONE }, { status: 400 });
  }
  return data(
    {
      kind: "create-refused" as const,
      error,
      displayName: choice.displayName,
      slug: choice.slug,
      pending,
    },
    { status },
  );
}

const INPUT =
  "block h-11 w-full rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

export default function VerifyPage() {
  const loaderData = useLoaderData<typeof loader>();
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

  // The route component only renders for PAGE navigations — the availability probe is a
  // fetcher.load that never re-renders it — so the page shape is guaranteed here.
  const chooser = "chooser" in loaderData ? loaderData.chooser : undefined;
  if (chooser === undefined) {
    return null;
  }
  const loopback = ("loopback" in loaderData ? loaderData.loopback : null) ?? null;
  const multi = ("multi" in loaderData ? loaderData.multi : false) === true;
  const origin = ("origin" in loaderData ? loaderData.origin : "") ?? "";

  if (actionData !== undefined && "kind" in actionData && actionData.kind === "approved") {
    return (
      <Shell>
        <PlainState heading="Approved">
          “{actionData.name}” finishes connecting on its next poll — you can close this tab.
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

  // The resolved card: from the loopback challenge (loader), the typed-code lookup (action),
  // or a create refusal's re-resolve (the flow is still pending; the error rides beside it).
  const createRefusal =
    actionData !== undefined && "kind" in actionData && actionData.kind === "create-refused"
      ? actionData
      : null;
  const card =
    createRefusal !== null
      ? createRefusal.pending
      : actionData !== undefined && "kind" in actionData && actionData.kind === "resolved"
        ? actionData.pending
        : (("resolved" in loaderData ? loaderData.resolved : null) ?? null);

  // The listener pass-through rides ONLY the challenge-resolved flow's own card: a typed-code
  // lookup of a DIFFERENT request on a loopback-armed URL renders without it, so deciding that
  // card can never spend the waiting listener's one wake (the action re-verifies server-side —
  // this is the honest client half of the same rule).
  const resolvedFromLoader = ("resolved" in loaderData ? loaderData.resolved : null) ?? null;
  const challenge = ("device" in loaderData ? loaderData.device : null) ?? null;
  const fromChallenge =
    resolvedFromLoader !== null && card !== null && card.userCode === resolvedFromLoader.userCode;

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
        {card !== null ? (
          <PendingRequest
            card={card}
            loopback={fromChallenge ? loopback : null}
            challenge={fromChallenge ? challenge : null}
            chooser={chooser}
            multi={multi}
            origin={origin}
            createRefusal={createRefusal}
          />
        ) : (
          <CodeLookup />
        )}
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
          {/* No `required`: the browser's own bubble is not this form's voice, and an empty
              submit has one answer — the action's line, in the app's words, in the same place
              every other refusal on this page appears. */}
          <input
            type="text"
            name="code"
            maxLength={USER_CODE_LENGTH}
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

/** A radio pick value: `seat:<address>` · `invitation:<id>` · `create` · `invite-token`. */
type Pick = string;

/** The default pick: the preselect hint's match first (never the create form), else the one
 * obvious option — seats before invitations, the create form only when it leads. */
function initialPick(card: PendingLoginFlowView, chooser: ChooserData): Pick | null {
  if (card.preselect !== null) {
    const seat = chooser.seats.find((s) => s.name === card.preselect);
    if (seat !== undefined) {
      return `seat:${seat.name}`;
    }
    const invitation = chooser.invitations.find((i) => i.workspaceName === card.preselect);
    if (invitation !== undefined) {
      return `invitation:${invitation.id}`;
    }
  }
  if (chooser.seats.length > 0) {
    return `seat:${chooser.seats[0]?.name}`;
  }
  if (chooser.invitations.length > 0) {
    return `invitation:${chooser.invitations[0]?.id}`;
  }
  if (chooser.createAllowed) {
    return "create";
  }
  return null;
}

/**
 * State two: the resolved request. What is asking, the CODE for the glance-check against the
 * terminal, THE WORKSPACE CHOICE (a login is one workspace's session — further workspaces are
 * further logins), and the two arms. The approve form posts the RESOLVED code as a hidden
 * field — the approval applies to exactly the request shown, never to whatever a lookup input
 * held — plus the pick; every posted standing is re-validated inside the approve fence.
 */
function PendingRequest({
  card,
  loopback,
  challenge,
  chooser,
  multi,
  origin,
  createRefusal,
}: {
  card: PendingLoginFlowView;
  loopback: Loopback | null;
  /** The flow's device-code-hash hex, present ONLY on the challenge-resolved flow's own card —
   * the action fires the listener wake-up exactly when the decided flow matches it. */
  challenge: string | null;
  chooser: ChooserData;
  multi: boolean;
  origin: string;
  createRefusal: { error: string; displayName: string; slug: string } | null;
}) {
  const liveInvite = card.invite !== null && card.invite.state === "live" ? card.invite : null;
  const [pick, setPick] = useState<Pick | null>(() =>
    createRefusal !== null
      ? "create"
      : liveInvite !== null
        ? "invite-token"
        : initialPick(card, chooser),
  );
  // WHICH arm is on the wire — both disable together (one decision per request), but only the
  // one that was clicked names its wait. Without this the approve button read "Working…" for a
  // deny.
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
      {challenge !== null && <input type="hidden" name="device" value={challenge} />}
    </>
  );

  // ONE obvious option renders as PROSE + the one button, never radio chrome — and on single
  // tenancy the chooser must never offer a phantom pick at all: the install IS its one
  // workspace, so a seat wins outright (a stale invitation to the same workspace decides
  // nothing) and radios cannot exist there by construction.
  const soloSeat =
    liveInvite === null &&
    chooser.seats.length === 1 &&
    (chooser.invitations.length === 0 || !multi);
  const soloInvitation =
    liveInvite === null &&
    !soloSeat &&
    chooser.seats.length === 0 &&
    chooser.invitations.length > 0 &&
    (!multi || chooser.invitations.length === 1)
      ? chooser.invitations[0]
      : undefined;
  const noStanding =
    liveInvite === null && chooser.seats.length === 0 && chooser.invitations.length === 0;
  const guidanceOnly = noStanding && !chooser.createAllowed;
  // The label follows the RENDERED arm (a solo arm's hidden pick outranks any radio state a
  // preselect may have seeded).
  const approveLabel =
    liveInvite !== null || soloInvitation !== undefined
      ? "Accept and connect"
      : soloSeat
        ? "Connect and approve this device"
        : pick === "create"
          ? "Create and connect"
          : pick?.startsWith("invitation:")
            ? "Accept and connect"
            : "Connect and approve this device";

  return (
    <div className="flex flex-col gap-4 rounded-md border border-line-soft bg-ground p-4">
      <p className="text-ink text-sm">
        <span className="font-medium">“{card.requestedName}”</span> wants to connect as you.
      </p>
      <p className="text-dim text-sm">
        Its code is <code className="font-mono text-ink">{card.userCode}</code> — confirm it matches
        your terminal before approving.
      </p>

      <InviteFallthroughLine invite={card.invite} />

      <Form method="post">
        <input type="hidden" name="intent" value="approve" />
        <input type="hidden" name="code" value={card.userCode} />
        {passThrough}
        {/* The whole choice goes inert while EITHER arm is on the wire — radios, create
            fields, and button together (one decision per request; no dead-feeling controls). */}
        <BusyFields busy={submitting} className="flex flex-col gap-3">
          {liveInvite !== null ? (
            <>
              <input type="hidden" name="pick" value="invite-token" />
              <div className="text-dim text-sm">
                <p>
                  You’re invited to{" "}
                  <span className="font-medium text-ink">{liveInvite.workspaceDisplay}</span> —
                  accepting connects this machine.
                </p>
                {liveInvite.awaitsApproval && <AwaitsApprovalNote />}
              </div>
            </>
          ) : soloSeat ? (
            <>
              <input type="hidden" name="pick" value={`seat:${chooser.seats[0]?.name}`} />
              <div className="text-dim text-sm">
                <p>
                  Approving connects it to{" "}
                  <span className="font-medium text-ink">{chooser.seats[0]?.displayName}</span>{" "}
                  <span className="font-mono text-faint">({chooser.seats[0]?.name})</span>.
                </p>
                {chooser.seats[0]?.awaitsApproval && <AwaitsApprovalNote />}
              </div>
            </>
          ) : soloInvitation !== undefined ? (
            // A lone invitation takes the same prose-and-one-button shape a lone seat gets —
            // one obvious option is a sentence, never a radio group of one.
            <>
              <input type="hidden" name="pick" value={`invitation:${soloInvitation.id}`} />
              <div className="text-dim text-sm">
                <p>
                  You’re invited to{" "}
                  <span className="font-medium text-ink">{soloInvitation.workspaceDisplay}</span> —
                  accepting connects this machine.
                </p>
                {soloInvitation.awaitsApproval && <AwaitsApprovalNote />}
              </div>
            </>
          ) : guidanceOnly ? (
            <Guidance multi={multi} chooser={chooser} />
          ) : (
            <WorkspaceChooser
              chooser={chooser}
              pick={pick}
              setPick={setPick}
              origin={origin}
              createRefusal={createRefusal}
            />
          )}

          {!guidanceOnly && (
            <>
              <ApprovingMeans />
              <button
                type="submit"
                disabled={pick === null}
                className={`${buttonClasses("primary")} min-h-11 w-full`}
              >
                {flying === "approve" ? "Connecting…" : approveLabel}
              </button>
            </>
          )}
        </BusyFields>
      </Form>
      <Form method="post">
        <input type="hidden" name="intent" value="deny" />
        <input type="hidden" name="code" value={card.userCode} />
        {passThrough}
        <BusyFields busy={submitting}>
          <button type="submit" className={`${buttonClasses("danger")} min-h-11 w-full`}>
            {flying === "deny" ? "Denying…" : "Deny — this isn’t me"}
          </button>
        </BusyFields>
      </Form>
    </div>
  );
}

/** The one honest line when a flow-carried invitation cannot pre-bind the card. */
function InviteFallthroughLine({ invite }: { invite: PendingLoginFlowView["invite"] }) {
  if (invite === null || invite.state === "live") {
    return null;
  }
  return (
    <p className="text-dim text-sm" role="status">
      {invite.state === "dead" &&
        "This login carried an invitation that is no longer live — it may have been accepted already or expired."}
      {invite.state === "other" &&
        `This login carries an invitation to ${invite.workspaceDisplay} addressed to a different account — open the emailed invitation link, which handles switching accounts.`}
      {invite.state === "unverified" &&
        `This login carries an invitation to ${invite.workspaceDisplay} for your address, but your email isn’t verified — open the emailed invitation link, which proves the mailbox.`}
    </p>
  );
}

/** What approving means — the standing consequences copy, one workspace at a time. */
function ApprovingMeans() {
  return (
    <p className="text-dim text-sm">
      It publishes, syncs, and reads there until you end the session — from your sessions page or
      with topos logout. Any further workspace is its own login.
    </p>
  );
}

function AwaitsApprovalNote() {
  return (
    <p className="mt-2 text-dim text-sm">
      Session approval is on there: the session waits until a workspace owner approves it — nothing
      is delivered before that.
    </p>
  );
}

/** The dead-end-free guidance when the viewer can neither pick nor create. */
function Guidance({ multi, chooser }: { multi: boolean; chooser: ChooserData }) {
  return (
    <div className="flex flex-col gap-2 text-dim text-sm" role="status">
      {chooser.heldUnverified && (
        <p>
          You have invitations waiting, but your email isn’t verified — open the emailed invitation
          link, which proves the mailbox and finishes right here.
        </p>
      )}
      {multi ? (
        <p>
          You don’t have a workspace here yet — ask a teammate for an invitation; it lands in your
          mailbox and finishes right here.
        </p>
      ) : (
        <>
          <p>
            You don’t have a seat in this workspace yet — ask an owner to invite you; the invitation
            lands here.
          </p>
          {chooser.bootUnclaimed && (
            <p>
              This workspace hasn’t been claimed yet — its printed setup link creates the first
              owner.
            </p>
          )}
        </>
      )}
    </div>
  );
}

const RADIO_ROW =
  "flex cursor-pointer items-start gap-3 rounded-md border border-line-soft bg-panel px-3 py-2.5 has-[:checked]:border-accent has-[:checked]:ring-1 has-[:checked]:ring-accent/40";

/** The workspace chooser: seats, then pending invitations, then — where open — create. */
function WorkspaceChooser({
  chooser,
  pick,
  setPick,
  origin,
  createRefusal,
}: {
  chooser: ChooserData;
  pick: Pick | null;
  setPick: (pick: Pick) => void;
  origin: string;
  createRefusal: { error: string; displayName: string; slug: string } | null;
}) {
  // The zero-standing create lead: no radio chrome around a single obvious form.
  const createLeads =
    chooser.seats.length === 0 && chooser.invitations.length === 0 && chooser.createAllowed;
  return (
    <fieldset className="flex flex-col gap-2">
      <legend className="mb-1 font-medium text-dim text-sm">Connect it to</legend>
      {chooser.heldUnverified && (
        <p className="text-dim text-sm" role="status">
          You have invitations waiting, but your email isn’t verified — open the emailed invitation
          link, which proves the mailbox.
        </p>
      )}
      {chooser.seats.map((seat) => (
        <label key={seat.workspaceId} className={RADIO_ROW}>
          <input
            type="radio"
            name="pick"
            value={`seat:${seat.name}`}
            checked={pick === `seat:${seat.name}`}
            onChange={() => setPick(`seat:${seat.name}`)}
            className="mt-1 accent-accent"
          />
          <span className="flex flex-col">
            <span className="text-ink text-sm">{seat.displayName}</span>
            <span className="font-mono text-faint text-xs">{seat.name}</span>
            {seat.awaitsApproval && (
              <span className="text-faint text-xs">
                Session approval is on — the session waits for an owner.
              </span>
            )}
          </span>
        </label>
      ))}
      {chooser.invitations.map((invitation) => (
        <label key={invitation.id} className={RADIO_ROW}>
          <input
            type="radio"
            name="pick"
            value={`invitation:${invitation.id}`}
            checked={pick === `invitation:${invitation.id}`}
            onChange={() => setPick(`invitation:${invitation.id}`)}
            className="mt-1 accent-accent"
          />
          <span className="flex flex-col">
            <span className="text-ink text-sm">
              You’re invited to {invitation.workspaceDisplay} — accept and connect
            </span>
            <span className="font-mono text-faint text-xs">{invitation.workspaceName}</span>
            {invitation.awaitsApproval && (
              <span className="text-faint text-xs">
                Session approval is on — the session waits for an owner.
              </span>
            )}
          </span>
        </label>
      ))}
      {chooser.createAllowed &&
        (createLeads ? (
          <>
            <input type="hidden" name="pick" value="create" />
            <CreateFields origin={origin} active createRefusal={createRefusal} />
          </>
        ) : (
          <>
            <label className={RADIO_ROW}>
              <input
                type="radio"
                name="pick"
                value="create"
                checked={pick === "create"}
                onChange={() => setPick("create")}
                className="mt-1 accent-accent"
              />
              <span className="text-ink text-sm">Create a new workspace</span>
            </label>
            {/* The expansion sits BESIDE the radio row, never inside its label — a label click
                activates the labeled control, which would steal every click aimed at these
                text inputs. */}
            {pick === "create" && (
              <div className="rounded-md border border-line-soft bg-panel px-3 py-3">
                <CreateFields origin={origin} active createRefusal={createRefusal} />
              </div>
            )}
          </>
        ))}
    </fieldset>
  );
}

/**
 * The inline create pair — the /new-shaped form: display name deriving an editable address
 * slug, with the live availability probe under it (this route's own `?check=` arm).
 */
function CreateFields({
  origin,
  active,
  createRefusal,
}: {
  origin: string;
  active: boolean;
  createRefusal: { error: string; displayName: string; slug: string } | null;
}) {
  const [displayName, setDisplayName] = useState(createRefusal?.displayName ?? "");
  const [slug, setSlug] = useState(createRefusal?.slug ?? "");
  // Once the person edits the address by hand we stop re-deriving it from the display name.
  const [slugEdited, setSlugEdited] = useState(createRefusal !== null);
  const check = useFetcher<{ name: string; available: boolean }>();
  const checkLoad = check.load;

  // Debounced live-availability read: one request per settled slug, and the answer carries the
  // slug it is for (`name`) so a stale reply for an earlier keystroke is ignored.
  useEffect(() => {
    if (!active || !isWorkspaceNameShape(slug)) {
      return;
    }
    const id = setTimeout(() => {
      checkLoad(`/verify?check=${encodeURIComponent(slug)}`);
    }, 300);
    return () => clearTimeout(id);
  }, [slug, active, checkLoad]);

  const checkData = check.data && "available" in check.data ? check.data : undefined;
  const forCurrent = checkData !== undefined && checkData.name === slug;
  // ONE truth at a time: the server's typed refusal renders only while it still DESCRIBES the
  // values in the fields — editing either one retires it and the live probe takes over (a
  // stale "taken" beside a fresh "Available." would be two answers to different questions) —
  // and while it stands, the probe says nothing (two identical "taken" lines is noise).
  const serverErrorStands =
    createRefusal !== null &&
    createRefusal.slug === slug &&
    createRefusal.displayName === displayName;

  return (
    <div className="flex flex-col gap-3">
      <label className="block">
        <span className="mb-1 block font-medium text-dim text-sm">Workspace name</span>
        <input
          type="text"
          name="displayName"
          autoComplete="off"
          spellCheck={false}
          placeholder="Acme Engineering"
          maxLength={100}
          value={displayName}
          onChange={(e) => {
            setDisplayName(e.target.value);
            if (!slugEdited) {
              // Deriving re-reads the WHOLE display name each keystroke, so the full rule is
              // loss-free here (unlike the address field's own feedback loop below).
              setSlug(toWorkspaceSlug(e.target.value));
            }
          }}
          className={INPUT}
        />
      </label>
      <label className="block">
        <span className="mb-1 block font-medium text-dim text-sm">Address</span>
        <input
          type="text"
          name="slug"
          autoComplete="off"
          spellCheck={false}
          placeholder="acme-engineering"
          pattern="[a-z0-9][a-z0-9-]*"
          maxLength={WORKSPACE_NAME_MAX}
          value={slug}
          onChange={(e) => {
            setSlugEdited(true);
            // The DRAFT rule per keystroke — full canonicalization here would eat the hyphen
            // just typed ("stranger-" → "stranger", so "stranger-team" lands "strangerteam").
            setSlug(toWorkspaceSlugDraft(e.target.value));
          }}
          onBlur={() => setSlug(toWorkspaceSlug(slug))}
          className={`${INPUT} font-mono`}
        />
        {slug.length > 0 && (
          <div className="mt-1 space-y-1">
            <p className="text-faint text-xs">
              <span className="font-mono">
                {origin}/{slug}
              </span>
            </p>
            {serverErrorStands ? null : !isWorkspaceNameShape(slug) ? (
              <p className="text-faint text-xs">
                Use lowercase letters, numbers, and hyphens for the address.
              </p>
            ) : check.state !== "idle" ? (
              <p className="text-faint text-xs" role="status">
                Checking availability…
              </p>
            ) : forCurrent && checkData?.available === true ? (
              <p className="text-green-700 text-xs" role="status">
                Available.
              </p>
            ) : forCurrent && checkData?.available === false ? (
              <p className="text-red-600 text-xs" role="status">
                {ADDRESS_TAKEN}
              </p>
            ) : null}
          </div>
        )}
      </label>
      {serverErrorStands && createRefusal !== null && (
        <p className="text-red-600 text-sm" role="alert">
          {createRefusal.error}
        </p>
      )}
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
