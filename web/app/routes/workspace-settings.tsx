import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, useLoaderData } from "react-router";
import { AddressBlock } from "@/components/members/address-block";
import { InviteMemberForm } from "@/components/members/invite-member-form";
import { RemoveMemberForm } from "@/components/members/remove-member-form";
import { ReviewRequiredPanel } from "@/components/policy/review-required-panel";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import {
  normalizeEmail,
  notFound,
  requireMember,
  requireWorkspaceOwner,
} from "@/lib/auth/guards.server";
import {
  inviteMembers,
  lastPolicyEvent,
  type PlaneMemberRow,
  type PolicyOutcome,
  planeWorkspaceById,
  recordPolicyEvent,
  rosterOf,
  setReviewDefault,
  workspacePolicyOf,
} from "@/lib/db/queries.server";
import { inviteMailDelivery, sendInviteEmail } from "@/lib/mail/invite-mail.server";
import { vaultFetch } from "@/lib/plane/client.server";
import { followBase } from "@/lib/plane/follow-base.server";
import type { RemoveMemberBody, RemoveMemberOutcome } from "@/lib/plane/wire";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Settings · ${params.ws ?? "Workspace"}` }];
}

/** A canonical UUID (what a per-form render mints); anything else is a mangled form. */
const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** The vault's static remove-denial reason for the last-owner lockout (rendered specially). */
const LAST_OWNER_REASON = "would remove the last owner";

function plausibleEmail(email: string): boolean {
  return email.includes("@") && email.length >= 3 && !/\s/.test(email);
}

/**
 * Printable ASCII, checked on the RAW input BEFORE lowercase-normalization — the same
 * discipline as the actor mint: directory principals are ASCII-canonical, and a Unicode
 * lookalike (U+212A KELVIN folds to `k`) must never silently seat an address the inviter
 * didn't type; a genuinely non-ASCII address would seat a principal no session could ever
 * become, a permanently dead row.
 */
const PRINTABLE_ASCII_EMAIL_RE = /^[\x20-\x7e]+$/;

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  // Management is a confirmed OWNER seat — the actor's role IS the directory's.
  const isOwner = actor.role === "owner";
  const [lastEvent, roster, policy, workspace] = await Promise.all([
    lastPolicyEvent(actor, ws),
    rosterOf(actor),
    workspacePolicyOf(actor),
    planeWorkspaceById(actor, ws),
  ]);
  return {
    ws,
    isOwner,
    lastEvent,
    roster,
    address: workspace?.name ?? ws,
    origin: followBase(request),
    // The directory holds the real value now (a bigint 0/1 flag) — the switch reflects it.
    reviewRequired: policy ? policy.reviewRequired === 1 : false,
  };
}

/**
 * ONE action, dispatched on the hidden `intent`. Each branch RE-GUARDS itself (a loader gate
 * never extends to an action), and each re-guards at its own grade: inviting is a member op the
 * DATABASE gates per invite-policy; removing and the review-gate toggle are owner-only.
 */
export async function action({ request, params }: ActionFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const formData = await request.formData();
  const intent = String(formData.get("intent") ?? "");
  if (intent === "invite") {
    return inviteIntent(request, ws, formData);
  }
  if (intent === "remove") {
    return removeIntent(request, ws, formData);
  }
  if (intent === "set-review-required") {
    return reviewRequiredIntent(request, ws, formData);
  }
  return data({ intent: "unknown" as const, status: "error" as const }, { status: 400 });
}

/**
 * Invitation is a member op, and the DATABASE is the gate: `topos_invite` enforces member-or-owner
 * per the workspace's invite-policy itself. The web guard is only `requireMember` — a plain member
 * may invite where policy allows, and the honest "owners only" copy shows where it doesn't.
 */
async function inviteIntent(request: Request, ws: string, formData: FormData) {
  const actor = await requireMember(request, ws);
  const raw = String(formData.get("emails") ?? "");
  const parts = raw.split(/[\s,]+/).filter((part) => part.length > 0);
  if (!parts.every((part) => PRINTABLE_ASCII_EMAIL_RE.test(part))) {
    return { intent: "invite" as const, status: "error" as const, submittedEmails: raw };
  }
  const emails = parts.map(normalizeEmail);
  if (emails.length === 0 || !emails.every(plausibleEmail)) {
    return { intent: "invite" as const, status: "error" as const, submittedEmails: raw };
  }

  const outcome = await inviteMembers(actor, emails);
  if (outcome === "invited") {
    // Best-effort mail: the seats stand whether or not delivery succeeds — a mail fault is logged,
    // never fatal, and the address block below already shares the same address by hand.
    // Honest by capability: the OSS default wires NO outbound delivery, so a successful
    // invite must never claim a mail went out — owners share the address by hand instead.
    let emailSent = inviteMailDelivery().canSend;
    try {
      const workspace = await planeWorkspaceById(actor, ws);
      const address = `${followBase(request)}/${workspace?.name ?? ws}`;
      const workspaceName = workspace?.displayName ?? ws;
      for (const email of emails) {
        await sendInviteEmail({
          to: email,
          address,
          workspaceDisplayName: workspaceName,
          invitedBy: actor.email,
        });
      }
    } catch {
      emailSent = false;
    }
    return { intent: "invite" as const, status: "invited" as const, invited: emails, emailSent };
  }
  if (outcome === "owner_role_required") {
    return { intent: "invite" as const, status: "owner_required" as const, submittedEmails: raw };
  }
  if (outcome === "member_required") {
    // A race: the acting seat lapsed between guard and call. The house miss posture, never a claim.
    notFound();
  }
  // unknown_channel (or anything unexpected) with no channels sent — honestly a rejected input.
  return { intent: "invite" as const, status: "error" as const, submittedEmails: raw };
}

/** Removing a seat is owner-only; the web guard is the matching lock behind the vault's own gate. */
async function removeIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const requestId = String(formData.get("request_id") ?? "").trim();
  if (!UUID_RE.test(requestId)) {
    return { intent: "remove" as const, status: "error" as const };
  }
  // The directory's principal identity is case-EXACT and the form binds a VERBATIM seat email —
  // trim only, so a mixed-case seat is matched, not silently no-op-removed.
  const email = String(formData.get("email") ?? "").trim();
  const response = await vaultFetch({
    method: "POST",
    template: "/internal/v1/workspaces/{ws}/roster/remove",
    params: { ws },
    actingEmail: owner.email,
    body: { request_id: requestId, email } satisfies RemoveMemberBody,
  });
  if (!response.ok) {
    return { intent: "remove" as const, status: "error" as const };
  }
  let outcome: RemoveMemberOutcome;
  try {
    outcome = (await response.json()) as RemoveMemberOutcome;
  } catch {
    return { intent: "remove" as const, status: "error" as const };
  }
  if (outcome.outcome === "removed") {
    return { intent: "remove" as const, status: "removed" as const };
  }
  // denied: distinguish the last-owner lockout (the promised honest state) from the acting gate.
  return {
    intent: "remove" as const,
    status: outcome.reason === LAST_OWNER_REASON ? ("last_owner" as const) : ("denied" as const),
  };
}

/**
 * The review-required gate — owner-only. The LOCK is the database's: `topos_set_review_default`
 * re-runs the owner gate inside the function, so this web guard is defense-in-depth, never the
 * only check. Every attempt lands a `policy_event` row whatever the outcome, so the panel's
 * audit line stays honest; the loader revalidates and re-reads it after the action.
 */
async function reviewRequiredIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const value = String(formData.get("review_required") ?? "") === "true";
  let outcome: PolicyOutcome;
  try {
    const set = await setReviewDefault(owner, value);
    outcome = set === "set" ? "ok" : "denied";
  } catch {
    outcome = "error";
  }
  await recordPolicyEvent(owner, value, outcome);
  return { intent: "set-review-required" as const, status: outcome };
}

export default function WorkspaceSettings() {
  const { ws, isOwner, lastEvent, roster, address, origin, reviewRequired } =
    useLoaderData<typeof loader>();
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
      <MembersSection roster={roster} isOwner={isOwner} />
      <AddressSection address={address} origin={origin} />
      {isOwner && (
        <ReviewRequiredPanel
          lastEvent={lastEvent}
          isOwner={isOwner}
          reviewRequired={reviewRequired}
        />
      )}
    </div>
  );
}

/**
 * The roster panel: the directory's own seats. Every member may attempt an invite (the database
 * gates it per invite-policy); removal controls render only for a confirmed OWNER, and never for
 * the workspace's last owner (the honest lockout the vault also enforces).
 */
function MembersSection({ roster, isOwner }: { roster: PlaneMemberRow[]; isOwner: boolean }) {
  const ownerCount = roster.filter((s) => s.role === "owner").length;
  return (
    <section aria-labelledby="members-heading" className="space-y-3">
      <div className="space-y-1">
        <SectionHeading>
          <span id="members-heading">Members</span>
        </SectionHeading>
        <p className="text-dim text-sm">
          Seats live on the workspace roster — who can enroll a device and publish here. Reviewers
          can also approve proposals.
        </p>
      </div>
      <Card className="overflow-hidden">
        <ul>
          {roster.map((seat) => (
            <li
              key={seat.principal}
              className="flex min-h-12 flex-wrap items-center gap-x-4 gap-y-1 border-line-soft border-b px-4 py-3 last:border-b-0"
            >
              <span className="text-ink text-sm">{seat.principal}</span>
              <Chip tone={seat.role === "owner" ? "accent" : "neutral"}>{seat.role}</Chip>
              <span className="text-faint text-xs">{seat.status}</span>
              <span className="ml-auto">
                {isOwner &&
                  (seat.role === "owner" && ownerCount <= 1 ? (
                    <span className="text-faint text-xs">workspace owner</span>
                  ) : (
                    <RemoveMemberForm email={seat.principal} />
                  ))}
              </span>
            </li>
          ))}
          {roster.length === 0 && <li className="px-4 py-3 text-faint text-sm">No seats yet.</li>}
        </ul>
      </Card>
      <InviteMemberForm />
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
