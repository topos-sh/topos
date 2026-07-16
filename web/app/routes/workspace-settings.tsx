import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useLoaderData } from "react-router";
import { AddressBlock } from "@/components/members/address-block";
import { InvitePolicyPanel } from "@/components/policy/invite-policy-panel";
import type { LastSetLine } from "@/components/policy/last-set-line";
import { RegistrationPanel } from "@/components/policy/registration-panel";
import { ReviewRequiredPanel } from "@/components/policy/review-required-panel";
import { StalenessWindowPanel } from "@/components/policy/staleness-window-panel";
import { SettingsTabs } from "@/components/settings-tabs";
import { StepUpMethodProvider } from "@/components/step-up";
import { buttonClasses, Card, PageHeader, SectionHeading } from "@/components/ui";
import { requireMember, requireWorkspaceOwner, workspaceInScope } from "@/lib/auth/guards.server";
import { requireStepUp, stepUpMethod } from "@/lib/auth/step-up.server";
import { type AuditEventRow, lastAuditEventOfKind, recordAdminEvent } from "@/lib/db/audit.server";
import {
  setInvitePolicy,
  setRegistration,
  setStalenessWindow,
  workspacePolicyOf,
} from "@/lib/db/queries.policy.server";
import { setReviewDefault } from "@/lib/db/queries.server";
import { useWsPath } from "@/lib/ws-path";
import { workspaceAddress } from "@/lib/ws-url.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Settings · ${params.ws ?? "Workspace"}` }];
}

/** Shape one audit row into the panels' "last set by" line (null = never set from here). */
function lastSetOf(row: AuditEventRow | undefined): LastSetLine | null {
  if (row === undefined) {
    return null;
  }
  return { value: row.subject, by: row.actorDisplay, at: row.createdAt };
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const ws = workspace.id;
  const actor = await requireMember(request, ws);
  // Management is a confirmed OWNER seat — the actor's role IS the seat table's.
  const isOwner = actor.role === "owner";
  // The knobs are plain columns on the ONE workspace row; the column DEFAULTs are the canonical
  // fallbacks, so a fresh install shows the true defaults, never a blank. The "last set by"
  // lines read the audit ledger — the same rows the setters land in their own transactions.
  const [policy, lastReview, lastInvite, lastStaleness, lastRegistration] = await Promise.all([
    workspacePolicyOf(actor),
    lastAuditEventOfKind(actor, "policy_review_default"),
    lastAuditEventOfKind(actor, "policy_invite"),
    lastAuditEventOfKind(actor, "policy_staleness"),
    lastAuditEventOfKind(actor, "policy_registration"),
  ]);
  return {
    isOwner,
    slug: workspace.name,
    shareAddress: workspaceAddress(request, workspace.name),
    stepUpMethod: await stepUpMethod(actor.userId),
    reviewRequired: policy.protectionDefault === "reviewed",
    invitePolicy: policy.invitePolicy,
    stalenessWindowMs: policy.stalenessWindowMs,
    registration: policy.registration,
    lastSet: {
      review: lastSetOf(lastReview),
      invite: lastSetOf(lastInvite),
      staleness: lastSetOf(lastStaleness),
      registration: lastSetOf(lastRegistration),
    },
  };
}

/**
 * ONE action, dispatched on the hidden `intent`. Each branch RE-GUARDS itself as an owner (a
 * loader gate never extends to an action), then runs the STEP-UP ceremony — the person re-enters
 * their password inside the form and it is verified immediately before the write. A failed
 * step-up writes NOTHING and still records the refused attempt under the knob's own audit kind
 * (outcome `denied`, detail `step_up` — the "last set by" lines read `ok` rows only, so refusals
 * never pollute them); a passing one writes, and the setter lands the `ok` audit row in its own
 * transaction. Membership admin lives on its own page (the members route).
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const workspace = await workspaceInScope(params);
  const ws = workspace.id;
  // The membership FLOOR, hoisted above the intent dispatch: every intent below requires at
  // least a member (most re-check owner/reviewer themselves), and the unmatched-intent 400 must
  // never answer a non-member — in multi tenancy `:ws` is a guessable public name slug, so a
  // 400-vs-404 split would be a workspace-existence oracle the GET faces deliberately close.
  await requireMember(request, workspace.id);
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
  if (intent === "set-registration") {
    return registrationIntent(request, ws, formData);
  }
  return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
}

/** The copy a transient server fault surfaces. */
const SERVER_ERROR = "The server couldn't be reached. Try again.";

type KnobStatus = "ok" | "denied" | "error" | "step_up_failed";

/**
 * The shared ceremony frame: owner guard → step-up → the setter. A refused step-up records the
 * attempt under the knob's audit kind and returns its typed error; a bounds/vocabulary refusal
 * ("denied") records too — the setter never saw it or refused it without writing, so the route
 * is the only place that attempt can land in the trail.
 */
async function knobIntent<Outcome extends string>(
  request: Request,
  ws: string,
  formData: FormData,
  args: {
    auditKind: string;
    detail: string;
    run: (owner: Awaited<ReturnType<typeof requireWorkspaceOwner>>) => Promise<Outcome>;
    deniedError: (outcome: Outcome) => string;
  },
): Promise<{ status: KnobStatus; error?: string }> {
  const owner = await requireWorkspaceOwner(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: args.auditKind,
      subject: ws,
      detail: "step_up",
      outcome: "denied",
    });
    return { status: "step_up_failed", error: stepUp.error };
  }
  let outcome: Outcome;
  try {
    outcome = await args.run(owner);
  } catch {
    await recordAdminEvent(owner, {
      kind: args.auditKind,
      subject: ws,
      detail: args.detail,
      outcome: "error",
    });
    return { status: "error", error: SERVER_ERROR };
  }
  if (outcome === "set") {
    return { status: "ok" };
  }
  await recordAdminEvent(owner, {
    kind: args.auditKind,
    subject: ws,
    detail: args.detail,
    outcome: "denied",
  });
  return { status: "denied", error: args.deniedError(outcome) };
}

/** The review-required gate — the workspace's protection DEFAULT, as one switch. */
async function reviewRequiredIntent(request: Request, ws: string, formData: FormData) {
  const value = String(formData.get("review_required") ?? "") === "true";
  const result = await knobIntent(request, ws, formData, {
    auditKind: "policy_review_default",
    detail: value ? "reviewed" : "open",
    run: (owner) => setReviewDefault(owner, value),
    deniedError: () => SERVER_ERROR,
  });
  return { intent: "set-review-required" as const, ...result };
}

/** Who may invite — 'members' (any member) or 'owners'. */
async function invitePolicyIntent(request: Request, ws: string, formData: FormData) {
  const policy = String(formData.get("invite_policy") ?? "");
  const result = await knobIntent(request, ws, formData, {
    auditKind: "policy_invite",
    detail: policy,
    run: (owner) => setInvitePolicy(owner, policy),
    deniedError: () => "Choose members or owners.",
  });
  return { intent: "set-invite-policy" as const, ...result };
}

/** The staleness window — entered in days, converted to milliseconds at hour granularity. */
async function stalenessWindowIntent(request: Request, ws: string, formData: FormData) {
  const days = Number(formData.get("staleness_days") ?? "");
  // Round to the nearest hour, then to milliseconds. A NaN input (empty/garbage) becomes 0,
  // which the setter refuses as bad_window (honest, not a crash).
  const windowMs = Number.isFinite(days) ? Math.round(days * 24) * 3_600_000 : 0;
  const result = await knobIntent(request, ws, formData, {
    auditKind: "policy_staleness",
    detail: String(windowMs),
    run: (owner) => setStalenessWindow(owner, windowMs),
    deniedError: () => "Enter a window between 1 hour and 366 days.",
  });
  return { intent: "set-staleness-window" as const, ...result };
}

/** The registration knob — `open` disables the invitation proof; default invite_only. */
async function registrationIntent(request: Request, ws: string, formData: FormData) {
  const value = String(formData.get("registration") ?? "");
  const result = await knobIntent(request, ws, formData, {
    auditKind: "policy_registration",
    detail: value,
    run: (owner) => setRegistration(owner, value),
    deniedError: () => "Choose invite-only or open.",
  });
  return { intent: "set-registration" as const, ...result };
}

export default function WorkspaceSettings() {
  const {
    isOwner,
    slug,
    shareAddress,
    stepUpMethod,
    reviewRequired,
    invitePolicy,
    stalenessWindowMs,
    registration,
    lastSet,
  } = useLoaderData<typeof loader>();
  const wsPath = useWsPath();
  return (
    <StepUpMethodProvider method={stepUpMethod}>
      <div className="space-y-8">
        <PageHeader
          title="Settings"
          meta={<code className="font-mono">{slug}</code>}
          actions={
            <Link to={wsPath("")} className={buttonClasses("quiet")}>
              Back to workspace
            </Link>
          }
        />
        <SettingsTabs active="general" />
        <MembersPointer />
        <AddressSection address={shareAddress} />
        <ReviewRequiredPanel
          isOwner={isOwner}
          reviewRequired={reviewRequired}
          lastSet={lastSet.review}
        />
        <InvitePolicyPanel isOwner={isOwner} invitePolicy={invitePolicy} lastSet={lastSet.invite} />
        <StalenessWindowPanel
          isOwner={isOwner}
          stalenessWindowMs={stalenessWindowMs}
          lastSet={lastSet.staleness}
        />
        <RegistrationPanel
          isOwner={isOwner}
          registration={registration}
          lastSet={lastSet.registration}
        />
      </div>
    </StepUpMethodProvider>
  );
}

/** Membership admin lives on its own page — settings points at it rather than duplicating it. */
function MembersPointer() {
  const wsPath = useWsPath();
  return (
    <section aria-labelledby="members-pointer-heading" className="space-y-3">
      <SectionHeading>
        <span id="members-pointer-heading">Members</span>
      </SectionHeading>
      <Card className="flex flex-wrap items-center justify-between gap-3 px-4 py-3">
        <p className="text-dim text-sm">
          Invitations, roles, and removals live on the members page.
        </p>
        <Link to={wsPath("members")} className={buttonClasses("quiet")}>
          Manage members
        </Link>
      </Card>
    </section>
  );
}

/**
 * The workspace address — its own pane section. Sharing and joining speak this address:
 * `topos follow <address>`.
 */
function AddressSection({ address }: { address: string }) {
  return (
    <section aria-labelledby="address-heading" className="space-y-3">
      <SectionHeading>
        <span id="address-heading">Workspace address</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-dim text-sm">
          Hand this to a teammate or another of your own devices — following it joins the workspace.
        </p>
        <AddressBlock address={address} />
      </Card>
    </section>
  );
}
