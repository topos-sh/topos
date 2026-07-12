import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useLoaderData } from "react-router";
import { AddressBlock } from "@/components/members/address-block";
import { InvitePolicyPanel } from "@/components/policy/invite-policy-panel";
import { ReviewRequiredPanel } from "@/components/policy/review-required-panel";
import { StalenessWindowPanel } from "@/components/policy/staleness-window-panel";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { notFound, requireMember, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { requireStepUp } from "@/lib/auth/step-up.server";
import { type AdminOutcome, recordAdminEvent } from "@/lib/db/audit.server";
import {
  type InvitePolicyOutcome,
  invitePolicyOf,
  type StalenessWindowOutcome,
  setInvitePolicy,
  setStalenessWindow,
  stalenessWindowOf,
} from "@/lib/db/queries.policy.server";
import {
  lastPolicyEvent,
  type PolicyOutcome,
  planeWorkspaceById,
  type ReviewDefaultOutcome,
  recordPolicyEvent,
  setReviewDefault,
  workspacePolicyOf,
} from "@/lib/db/queries.server";
import { followBase } from "@/lib/plane/follow-base.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Settings · ${params.ws ?? "Workspace"}` }];
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  // Management is a confirmed OWNER seat — the actor's role IS the directory's.
  const isOwner = actor.role === "owner";
  // The invite policy + staleness window come through the guarded reader functions so a workspace
  // with NO policy row shows the true defaults (members / 7 days), never a blank — the defaults
  // live once, in SQL. Review-required rides the direct policy-row read as before.
  const [lastEvent, policy, workspace, invitePolicy, stalenessWindowMs] = await Promise.all([
    lastPolicyEvent(actor, ws),
    workspacePolicyOf(actor),
    planeWorkspaceById(actor, ws),
    invitePolicyOf(actor),
    stalenessWindowOf(actor),
  ]);
  return {
    ws,
    isOwner,
    lastEvent,
    address: workspace?.name ?? ws,
    origin: followBase(request),
    // The directory holds the real value now (a bigint 0/1 flag) — the switch reflects it.
    reviewRequired: policy ? policy.reviewRequired === 1 : false,
    invitePolicy,
    stalenessWindowMs,
  };
}

/**
 * ONE action, dispatched on the hidden `intent`. Each branch RE-GUARDS itself as an owner (a
 * loader gate never extends to an action), then runs the STEP-UP ceremony — the person re-enters
 * their password inside the form and it is verified immediately before the write. A failed step-up
 * writes NOTHING and still records the refused attempt (`admin_event`, outcome denied, detail
 * `step_up`); a passing one writes, then records the outcome. Membership admin lives on its own
 * page (/workspaces/:ws/members); this page is the workspace's policy + address surface.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "set-review-required") {
    return reviewRequiredIntent(request, ws, formData);
  }
  if (intent === "set-invite-policy") {
    return invitePolicyIntent(request, ws, formData);
  }
  if (intent === "set-staleness-window") {
    return stalenessWindowIntent(request, ws, formData);
  }
  return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
}

/** The copy a role-denial (owner_role_required / member_required) surfaces — the actor's seat moved. */
const ROLE_DENIED_ERROR =
  "The server refused the change — you may no longer be an owner of this workspace.";
/** The copy a transient server fault surfaces. */
const SERVER_ERROR = "The server couldn't be reached. Try again.";

/** Map a setter's outcome (anything but 'set') to the audit outcome + the inline error copy. */
function denialOutcome(error: string): { admin: AdminOutcome; error: string } {
  return { admin: "denied", error };
}

/**
 * The review-required gate — owner-only, now step-up gated. `topos_set_review_default` re-runs
 * the owner gate inside the function; this web guard is defense-in-depth. Every attempt lands a
 * `policy_event` row (this tier's audit line) AND an `admin_event` row, so both trails stay honest.
 */
async function reviewRequiredIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const value = String(formData.get("review_required") ?? "") === "true";

  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "review_default",
      subject: ws,
      detail: "step_up",
      outcome: "denied",
    });
    return {
      intent: "set-review-required" as const,
      status: "step_up_failed" as const,
      error: stepUp.error,
    };
  }

  let policyOutcome: PolicyOutcome;
  let adminOutcome: AdminOutcome;
  let error: string | undefined;
  try {
    const set: ReviewDefaultOutcome = await setReviewDefault(owner, value);
    if (set === "set") {
      policyOutcome = "ok";
      adminOutcome = "ok";
    } else {
      policyOutcome = "denied";
      adminOutcome = "denied";
      error = ROLE_DENIED_ERROR;
    }
  } catch {
    policyOutcome = "error";
    adminOutcome = "error";
    error = SERVER_ERROR;
  }
  await recordPolicyEvent(owner, value, policyOutcome);
  await recordAdminEvent(owner, {
    kind: "review_default",
    subject: ws,
    detail: value ? "on" : "off",
    outcome: adminOutcome,
  });
  return { intent: "set-review-required" as const, status: policyOutcome, error };
}

/**
 * Who may invite — owner-only, step-up gated. The database's `topos_set_invite_policy` re-runs the
 * owner gate and validates the policy string (an unexpected value comes back `bad_policy`). Records
 * one `admin_event` per attempt whatever the outcome.
 */
async function invitePolicyIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  // The form offers 'members' | 'owners' only; pass it through and let the DB validate.
  const policy = String(formData.get("invite_policy") ?? "") as "members" | "owners";

  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "invite_policy",
      subject: ws,
      detail: "step_up",
      outcome: "denied",
    });
    return {
      intent: "set-invite-policy" as const,
      status: "step_up_failed" as const,
      error: stepUp.error,
    };
  }

  const { status, admin, error } = await runSetter<InvitePolicyOutcome>(
    () => setInvitePolicy(owner, policy),
    (outcome) =>
      outcome === "bad_policy"
        ? denialOutcome("Choose members or owners.")
        : denialOutcome(ROLE_DENIED_ERROR),
  );
  await recordAdminEvent(owner, {
    kind: "invite_policy",
    subject: ws,
    detail: policy,
    outcome: admin,
  });
  return { intent: "set-invite-policy" as const, status, error };
}

/**
 * The staleness window — owner-only, step-up gated. The days input is converted to milliseconds at
 * hour granularity here; the database bounds it (1ms .. 366 days) and refuses anything else as
 * `bad_window`. Records one `admin_event` per attempt.
 */
async function stalenessWindowIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const days = Number(formData.get("staleness_days") ?? "");
  // Round to the nearest hour, then to milliseconds — the UI's "hour granularity". A NaN input
  // (empty/garbage) becomes 0, which the database refuses as bad_window (honest, not a crash).
  const windowMs = Number.isFinite(days) ? Math.round(days * 24) * 3_600_000 : 0;

  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "staleness_window",
      subject: ws,
      detail: "step_up",
      outcome: "denied",
    });
    return {
      intent: "set-staleness-window" as const,
      status: "step_up_failed" as const,
      error: stepUp.error,
    };
  }

  const { status, admin, error } = await runSetter<StalenessWindowOutcome>(
    () => setStalenessWindow(owner, windowMs),
    (outcome) =>
      outcome === "bad_window"
        ? denialOutcome("Enter a window between 1 hour and 366 days.")
        : denialOutcome(ROLE_DENIED_ERROR),
  );
  await recordAdminEvent(owner, {
    kind: "staleness_window",
    subject: ws,
    detail: String(windowMs),
    outcome: admin,
  });
  return { intent: "set-staleness-window" as const, status, error };
}

/**
 * Run a guarded setter and fold its outcome into a UI status + audit outcome + inline error. A
 * `'set'` is the success; any other code (a role refusal, a bounds refusal) is a denial mapped by
 * `onDenied`; a thrown fault is the transient error case.
 */
async function runSetter<Outcome extends string>(
  call: () => Promise<Outcome>,
  onDenied: (outcome: Outcome) => { admin: AdminOutcome; error: string },
): Promise<{ status: "ok" | "denied" | "error"; admin: AdminOutcome; error?: string }> {
  try {
    const outcome = await call();
    if (outcome === "set") {
      return { status: "ok", admin: "ok" };
    }
    const denied = onDenied(outcome);
    return { status: "denied", admin: denied.admin, error: denied.error };
  } catch {
    return { status: "error", admin: "error", error: SERVER_ERROR };
  }
}

export default function WorkspaceSettings() {
  const {
    ws,
    isOwner,
    lastEvent,
    address,
    origin,
    reviewRequired,
    invitePolicy,
    stalenessWindowMs,
  } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Settings"
        meta={<code className="font-mono">{address}</code>}
        actions={
          <Link to={`/workspaces/${ws}`} className={buttonClasses("quiet")}>
            Back to workspace
          </Link>
        }
      />
      <MembersPointer ws={ws} />
      <AddressSection address={address} origin={origin} />
      <ReviewRequiredPanel
        lastEvent={lastEvent}
        isOwner={isOwner}
        reviewRequired={reviewRequired}
      />
      <InvitePolicyPanel isOwner={isOwner} invitePolicy={invitePolicy} />
      <StalenessWindowPanel isOwner={isOwner} stalenessWindowMs={stalenessWindowMs} />
    </div>
  );
}

/** Membership admin lives on its own page — settings points at it rather than duplicating it. */
function MembersPointer({ ws }: { ws: string }) {
  return (
    <section aria-labelledby="members-pointer-heading" className="space-y-3">
      <SectionHeading>
        <span id="members-pointer-heading">Members</span>
      </SectionHeading>
      <Card className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
        <p className="text-dim text-sm">
          Invitations, roles, and removals live on the members page.
        </p>
        <Link to={`/workspaces/${ws}/members`} className={buttonClasses("quiet")}>
          Manage members
        </Link>
      </Card>
    </section>
  );
}

/**
 * The workspace address — its own pane section (replacing the old door link). Sharing and joining
 * speak this address: `topos follow <origin>/<address>`.
 */
function AddressSection({ address, origin }: { address: string; origin: string }) {
  return (
    <section aria-labelledby="address-heading" className="space-y-3">
      <SectionHeading>
        <span id="address-heading">Workspace address</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-dim text-sm">
          Hand this to a teammate or another of your own devices — following it joins the workspace.
        </p>
        <AddressBlock address={address} origin={origin} />
      </Card>
    </section>
  );
}
