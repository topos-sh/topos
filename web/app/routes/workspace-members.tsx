import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, redirect, useLoaderData } from "react-router";
import { AddressBlock } from "@/components/members/address-block";
import { InviteMemberForm } from "@/components/members/invite-member-form";
import { LeaveWorkspaceForm } from "@/components/members/leave-workspace-form";
import { RemoveMemberForm } from "@/components/members/remove-member-form";
import { RoleForm } from "@/components/members/role-form";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import {
  normalizeEmail,
  notFound,
  requireMember,
  requireWorkspaceOwner,
} from "@/lib/auth/guards.server";
import { requireStepUp } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import {
  type LeaveWorkspaceOutcome,
  leaveWorkspace,
  type SetMemberRoleOutcome,
  setMemberRole,
} from "@/lib/db/queries.roster.server";
import {
  inviteMembers,
  type PlaneMemberRow,
  planeWorkspaceById,
  rosterOf,
} from "@/lib/db/queries.server";
import { inviteMailDelivery, sendInviteEmail } from "@/lib/mail/invite-mail.server";
import { vaultFetch } from "@/lib/plane/client.server";
import { followBase } from "@/lib/plane/follow-base.server";
import type { RemoveMemberBody, RemoveMemberOutcome } from "@/lib/plane/wire";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Members · ${params.ws ?? "Workspace"}` }];
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
  const isOwner = actor.role === "owner";
  const [roster, workspace] = await Promise.all([rosterOf(actor), planeWorkspaceById(actor, ws)]);
  return {
    ws,
    isOwner,
    roster,
    selfEmail: actor.email,
    address: workspace?.name ?? ws,
    origin: followBase(request),
  };
}

/**
 * ONE action, dispatched on the hidden `intent`. Each branch RE-GUARDS itself (a loader gate
 * never extends to an action), and each re-guards at its own grade: inviting is a member op the
 * DATABASE gates per invite-policy; removing and role changes are owner-only; leaving is the
 * signed-in member's own act. Role change, remove, and leave are STEP-UP ceremonies (a fresh
 * password re-entry, verified immediately before the act); invite stays ungated (member-level,
 * non-destructive).
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
  if (intent === "set-role") {
    return setRoleIntent(request, ws, formData);
  }
  if (intent === "leave") {
    return leaveIntent(request, ws, formData);
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

/**
 * Removing a seat is owner-only and a STEP-UP ceremony: guard → validate the target → requireStepUp
 * → the instant-revoke vault call → record the admin event (whatever the outcome — a refused
 * step-up is a fact the trail must show). The web guard is the matching lock behind the vault's own
 * gate.
 */
async function removeIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const requestId = String(formData.get("request_id") ?? "").trim();
  // The directory's principal identity is case-EXACT and the form binds a VERBATIM seat email —
  // trim only, so a mixed-case seat is matched, not silently no-op-removed.
  const email = String(formData.get("email") ?? "").trim();
  if (!UUID_RE.test(requestId) || email.length === 0) {
    await recordAdminEvent(owner, { kind: "member_removed", subject: email, outcome: "error" });
    return { intent: "remove" as const, status: "error" as const };
  }
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "member_removed",
      subject: email,
      detail: "step_up",
      outcome: "denied",
    });
    return { intent: "remove" as const, status: "step_up" as const, error: stepUp.error };
  }
  const response = await vaultFetch({
    method: "POST",
    template: "/internal/v1/workspaces/{ws}/roster/remove",
    params: { ws },
    actingEmail: owner.email,
    body: { request_id: requestId, email } satisfies RemoveMemberBody,
  });
  if (!response.ok) {
    await recordAdminEvent(owner, { kind: "member_removed", subject: email, outcome: "error" });
    return { intent: "remove" as const, status: "error" as const };
  }
  let outcome: RemoveMemberOutcome;
  try {
    outcome = (await response.json()) as RemoveMemberOutcome;
  } catch {
    await recordAdminEvent(owner, { kind: "member_removed", subject: email, outcome: "error" });
    return { intent: "remove" as const, status: "error" as const };
  }
  if (outcome.outcome === "removed") {
    await recordAdminEvent(owner, { kind: "member_removed", subject: email, outcome: "ok" });
    return { intent: "remove" as const, status: "removed" as const };
  }
  // denied: distinguish the last-owner lockout (the promised honest state) from the acting gate.
  await recordAdminEvent(owner, {
    kind: "member_removed",
    subject: email,
    detail: outcome.reason,
    outcome: "denied",
  });
  return {
    intent: "remove" as const,
    status: outcome.reason === LAST_OWNER_REASON ? ("last_owner" as const) : ("denied" as const),
  };
}

/**
 * Changing a seat's role is owner-only and a STEP-UP ceremony: guard → validate the role →
 * requireStepUp → the guarded `topos_set_member_role` call → record the admin event. The database
 * re-runs the owner gate and refuses demoting the sole owner (`sole_owner`); its outcome codes ride
 * back to the honest inline copy. A lapsed acting gate (a race between guard and call) is the house
 * miss, never a claim.
 */
async function setRoleIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const email = String(formData.get("email") ?? "").trim();
  const role = String(formData.get("role") ?? "");
  if (role !== "owner" && role !== "reviewer" && role !== "member") {
    await recordAdminEvent(owner, {
      kind: "role_change",
      subject: email,
      detail: role,
      outcome: "error",
    });
    return { intent: "set-role" as const, status: "error" as const };
  }
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "role_change",
      subject: email,
      detail: "step_up",
      outcome: "denied",
    });
    return { intent: "set-role" as const, status: "step_up" as const, error: stepUp.error };
  }
  let outcome: SetMemberRoleOutcome;
  try {
    outcome = await setMemberRole(owner, email, role);
  } catch {
    await recordAdminEvent(owner, {
      kind: "role_change",
      subject: email,
      detail: role,
      outcome: "error",
    });
    return { intent: "set-role" as const, status: "error" as const };
  }
  if (outcome === "set") {
    await recordAdminEvent(owner, {
      kind: "role_change",
      subject: email,
      detail: role,
      outcome: "ok",
    });
    return { intent: "set-role" as const, status: "ok" as const };
  }
  if (outcome === "sole_owner") {
    await recordAdminEvent(owner, {
      kind: "role_change",
      subject: email,
      detail: role,
      outcome: "denied",
    });
    return { intent: "set-role" as const, status: "sole_owner" as const };
  }
  if (outcome === "member_required" || outcome === "owner_role_required") {
    // The acting owner seat lapsed between guard and call — the house miss, never a claim.
    notFound();
  }
  // bad_role (guarded above) or unknown_member (a vanished target) — an honest error.
  await recordAdminEvent(owner, {
    kind: "role_change",
    subject: email,
    detail: role,
    outcome: "error",
  });
  return { intent: "set-role" as const, status: "error" as const };
}

/**
 * The signed-in member leaving their OWN seat — a STEP-UP ceremony gated only by membership (any
 * confirmed member may leave themselves): guard → requireStepUp → the guarded `topos_leave_workspace`
 * call → record the admin event. On success the seat is gone and the person is sent to the
 * workspaces index (the workspace drops off their rail). The sole owner is refused honestly.
 */
async function leaveIntent(request: Request, ws: string, formData: FormData) {
  const actor = await requireMember(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(actor, {
      kind: "leave",
      subject: actor.email,
      detail: "step_up",
      outcome: "denied",
    });
    return { intent: "leave" as const, status: "step_up" as const, error: stepUp.error };
  }
  let outcome: LeaveWorkspaceOutcome;
  try {
    outcome = await leaveWorkspace(actor);
  } catch {
    await recordAdminEvent(actor, { kind: "leave", subject: actor.email, outcome: "error" });
    return { intent: "leave" as const, status: "error" as const };
  }
  if (outcome === "left") {
    await recordAdminEvent(actor, { kind: "leave", subject: actor.email, outcome: "ok" });
    throw redirect("/workspaces");
  }
  if (outcome === "sole_owner") {
    await recordAdminEvent(actor, { kind: "leave", subject: actor.email, outcome: "denied" });
    return { intent: "leave" as const, status: "sole_owner" as const };
  }
  // member_required — the seat is already gone (a race); the person is not a member. Send them home.
  throw redirect("/workspaces");
}

export default function WorkspaceMembers() {
  const { ws, isOwner, roster, address, origin } = useLoaderData<typeof loader>();
  return (
    <div className="space-y-8">
      <PageHeader
        title="Members"
        meta={<code className="font-mono">{address}</code>}
        actions={
          <Link to={`/workspaces/${ws}`} className={buttonClasses("quiet")}>
            Back to workspace
          </Link>
        }
      />
      <MembersSection roster={roster} isOwner={isOwner} />
      <section aria-labelledby="share-heading" className="space-y-3">
        <SectionHeading>
          <span id="share-heading">Workspace address</span>
        </SectionHeading>
        <Card className="space-y-3 px-4 py-3">
          <p className="text-dim text-sm">
            Hand this to a teammate or another of your own devices — following it joins the
            workspace.
          </p>
          <AddressBlock address={address} origin={origin} />
        </Card>
      </section>
      <LeaveWorkspaceForm />
    </div>
  );
}

/**
 * The roster panel: the directory's own seats. Every member may attempt an invite (the database
 * gates it per invite-policy); role and removal controls render only for a confirmed OWNER. The
 * SOLE owner's own seat carries neither (nothing safe to do — you can't remove or demote the last
 * owner; the honest lockout the database also enforces). Ownership transfers by promoting another
 * seat to owner first.
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
          {roster.map((seat) => {
            const soleOwner = seat.role === "owner" && ownerCount <= 1;
            return (
              <li
                key={seat.principal}
                className="flex min-h-12 flex-wrap items-center gap-x-4 gap-y-2 border-line-soft border-b px-4 py-3 last:border-b-0"
              >
                <span className="text-ink text-sm">{seat.principal}</span>
                <Chip tone={seat.role === "owner" ? "accent" : "neutral"}>{seat.role}</Chip>
                <span className="text-faint text-xs">{seat.status}</span>
                {isOwner &&
                  (soleOwner ? (
                    <span className="ml-auto text-faint text-xs">workspace owner</span>
                  ) : (
                    <span className="ml-auto flex flex-wrap items-center justify-end gap-2">
                      <RoleForm email={seat.principal} role={seat.role} />
                      <RemoveMemberForm email={seat.principal} />
                    </span>
                  ))}
              </li>
            );
          })}
          {roster.length === 0 && <li className="px-4 py-3 text-faint text-sm">No seats yet.</li>}
        </ul>
      </Card>
      <InviteMemberForm />
    </section>
  );
}
