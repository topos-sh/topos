import type { ActionFunctionArgs, LoaderFunctionArgs } from "react-router";
import { data, Link, redirect, useLoaderData } from "react-router";
import { AddressBlock } from "@/components/members/address-block";
import { InviteMemberForm } from "@/components/members/invite-member-form";
import { LeaveWorkspaceForm } from "@/components/members/leave-workspace-form";
import {
  PendingInvitations,
  type PendingInvitationView,
} from "@/components/members/pending-invitations";
import { RemoveMemberForm } from "@/components/members/remove-member-form";
import { RoleForm } from "@/components/members/role-form";
import { buttonClasses, Card, Chip, PageHeader, SectionHeading } from "@/components/ui";
import { notFound, requireMember, requireWorkspaceOwner } from "@/lib/auth/guards.server";
import { requireStepUp } from "@/lib/auth/step-up.server";
import { recordAdminEvent } from "@/lib/db/audit.server";
import { removeSeat, type SeatMutationRefusal, setSeatRole } from "@/lib/db/identity.server";
import { workspacePolicyOf } from "@/lib/db/queries.policy.server";
import {
  createInvitations,
  foldInviteEmail,
  pendingInvitationsOf,
  type RosterSeat,
  revokeInvitation,
  rosterOf,
} from "@/lib/db/queries.roster.server";
import { workspaceById } from "@/lib/db/queries.server";
import { sendInviteEmail } from "@/lib/mail/invite-mail.server";
import { mailDelivery } from "@/lib/mail/transport.server";
import { followBase } from "@/lib/plane/follow-base.server";

export function meta({ params }: { params: { ws?: string } }) {
  return [{ title: `Members · ${params.ws ?? "Workspace"}` }];
}

/** A coarse "lapses in …" label, computed in the loader so hydration re-reads no clock. */
function lapseLabel(expiresAt: Date | null, now: number): string {
  if (expiresAt === null) {
    return "does not lapse";
  }
  const remaining = expiresAt.getTime() - now;
  if (remaining <= 0) {
    return "lapsed";
  }
  const hours = Math.ceil(remaining / 3_600_000);
  if (hours < 48) {
    return hours === 1 ? "lapses in 1 hour" : `lapses in ${hours} hours`;
  }
  const days = Math.ceil(remaining / 86_400_000);
  return `lapses in ${days} days`;
}

export async function loader({ request, params }: LoaderFunctionArgs) {
  const ws = params.ws;
  if (!ws) {
    notFound();
  }
  const actor = await requireMember(request, ws);
  const isOwner = actor.role === "owner";
  const [roster, workspace, pending, policy] = await Promise.all([
    rosterOf(actor),
    workspaceById(actor, ws),
    pendingInvitationsOf(actor),
    workspacePolicyOf(actor),
  ]);
  const displayByUserId = new Map(roster.map((seat) => [seat.userId, seat.display]));
  const now = Date.now();
  const invitations: PendingInvitationView[] = pending.map((inv) => ({
    id: inv.id,
    email: inv.email,
    invitedByDisplay:
      inv.invitedBy !== null ? (displayByUserId.get(inv.invitedBy) ?? "a former member") : "—",
    lapse: lapseLabel(inv.expiresAt, now),
  }));
  return {
    ws,
    isOwner,
    selfUserId: actor.userId,
    roster,
    invitations,
    mailArmed: mailDelivery().canSend,
    invitePolicy: policy.invitePolicy,
    address: workspace?.name ?? ws,
    origin: followBase(request),
  };
}

/**
 * ONE action, dispatched on the hidden `intent`. Each branch RE-GUARDS itself (a loader gate
 * never extends to an action), and each re-guards at its own grade: inviting is a member op the
 * invite-policy gates; revoking an invitation, removing a seat, and role changes are owner-only;
 * leaving is the signed-in member's own act. Every seat mutation and the invitation revoke are
 * STEP-UP ceremonies (a fresh password re-entry, verified immediately before the act); invite
 * stays ungated (member-level, non-destructive). The data layer emits the audit row of every
 * landed act (and the last-owner refusals) inside its own transaction; the route records the
 * attempts the data layer never sees — refused step-ups, mangled forms, faults.
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
  if (intent === "revoke-invitation") {
    return revokeInvitationIntent(request, ws, formData);
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
 * Invitation is a member op gated by the workspace's invite-policy (createInvitations runs the
 * gate against the actor's role) — and it REQUIRES armed mail: the invitation's identity proof
 * is a mailbox round-trip, so an unarmed deployment refuses honestly instead of seating a claim
 * nobody can prove. Re-inviting an already-pending address upserts the row and re-arms the
 * 7-day clock — resending IS inviting again.
 */
async function inviteIntent(request: Request, ws: string, formData: FormData) {
  const actor = await requireMember(request, ws);
  if (!mailDelivery().canSend) {
    return { intent: "invite" as const, status: "mail_unarmed" as const };
  }
  const raw = String(formData.get("emails") ?? "");
  const parts = raw.split(/[\s,]+/).filter((part) => part.length > 0);
  const emails: string[] = [];
  for (const part of parts) {
    const folded = foldInviteEmail(part);
    if (folded === null || !folded.includes("@")) {
      return { intent: "invite" as const, status: "error" as const, submittedEmails: raw };
    }
    emails.push(folded);
  }
  if (emails.length === 0) {
    return { intent: "invite" as const, status: "error" as const, submittedEmails: raw };
  }

  const policy = await workspacePolicyOf(actor);
  const outcome = await createInvitations(actor, emails, policy.invitePolicy);
  if (outcome === "owner_role_required") {
    await recordAdminEvent(actor, {
      kind: "invitation_created",
      subject: emails.join(", "),
      detail: "owner_role_required",
      outcome: "denied",
    });
    return { intent: "invite" as const, status: "owner_required" as const, submittedEmails: raw };
  }
  if (outcome !== "invited") {
    return { intent: "invite" as const, status: "error" as const, submittedEmails: raw };
  }

  // The notice mail, per address. A send fault never loses the invitation — the row stands and
  // re-inviting resends — but the reply says honestly when nothing went out.
  const workspace = await workspaceById(actor, ws);
  const address = `${followBase(request)}/${workspace?.name ?? ws}`;
  const workspaceName = workspace?.displayName ?? ws;
  let emailSent = true;
  try {
    for (const email of emails) {
      await sendInviteEmail({
        to: email,
        address,
        workspaceDisplayName: workspaceName,
        invitedBy: actor.display,
      });
    }
  } catch {
    emailSent = false;
  }
  return { intent: "invite" as const, status: "invited" as const, invited: emails, emailSent };
}

/** Revoking a pending invitation — owner + step-up; the un-invite before anyone binds it. */
async function revokeInvitationIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const invitationId = String(formData.get("invitation_id") ?? "").trim();
  if (invitationId.length === 0) {
    return { intent: "revoke-invitation" as const, status: "error" as const, invitationId };
  }
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "invitation_revoked",
      subject: invitationId,
      detail: "step_up",
      outcome: "denied",
    });
    return {
      intent: "revoke-invitation" as const,
      status: "step_up" as const,
      invitationId,
      error: stepUp.error,
    };
  }
  let outcome: "revoked" | "missing";
  try {
    outcome = await revokeInvitation(owner, invitationId);
  } catch {
    await recordAdminEvent(owner, {
      kind: "invitation_revoked",
      subject: invitationId,
      outcome: "error",
    });
    return { intent: "revoke-invitation" as const, status: "error" as const, invitationId };
  }
  return { intent: "revoke-invitation" as const, status: outcome, invitationId };
}

/**
 * Removing a seat is owner-only and a STEP-UP ceremony: guard → validate the target →
 * requireStepUp → the last-owner-fenced removeSeat (which writes the detach records and the
 * audit row in the same transaction). The target is the seat's USER ID — the one identity —
 * never an email.
 */
async function removeIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const targetUserId = String(formData.get("user_id") ?? "").trim();
  if (targetUserId.length === 0) {
    await recordAdminEvent(owner, { kind: "member_removed", subject: "", outcome: "error" });
    return { intent: "remove" as const, status: "error" as const };
  }
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "member_removed",
      subject: targetUserId,
      detail: "step_up",
      outcome: "denied",
    });
    return { intent: "remove" as const, status: "step_up" as const, error: stepUp.error };
  }
  let outcome: SeatMutationRefusal | "ok";
  try {
    outcome = await removeSeat(owner, ws, targetUserId, "membership_removed");
  } catch {
    await recordAdminEvent(owner, {
      kind: "member_removed",
      subject: targetUserId,
      outcome: "error",
    });
    return { intent: "remove" as const, status: "error" as const };
  }
  if (outcome === "ok") {
    return { intent: "remove" as const, status: "removed" as const };
  }
  if (outcome === "last_owner") {
    return { intent: "remove" as const, status: "last_owner" as const };
  }
  // missing — the seat vanished between render and submit (a concurrent removal or leave).
  return { intent: "remove" as const, status: "missing" as const };
}

/**
 * Changing a seat's role is owner-only and a STEP-UP ceremony: guard → validate the role →
 * requireStepUp → the last-owner-fenced setSeatRole. Demoting the sole owner is refused inside
 * the same lock a concurrent demotion would need, and surfaces here as honest copy.
 */
async function setRoleIntent(request: Request, ws: string, formData: FormData) {
  const owner = await requireWorkspaceOwner(request, ws);
  const targetUserId = String(formData.get("user_id") ?? "").trim();
  const role = String(formData.get("role") ?? "");
  if (targetUserId.length === 0 || (role !== "owner" && role !== "reviewer" && role !== "member")) {
    await recordAdminEvent(owner, {
      kind: "role_change",
      subject: targetUserId,
      detail: role,
      outcome: "error",
    });
    return { intent: "set-role" as const, status: "error" as const };
  }
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(owner, {
      kind: "role_change",
      subject: targetUserId,
      detail: "step_up",
      outcome: "denied",
    });
    return { intent: "set-role" as const, status: "step_up" as const, error: stepUp.error };
  }
  let outcome: SeatMutationRefusal | "ok";
  try {
    outcome = await setSeatRole(owner, ws, targetUserId, role);
  } catch {
    await recordAdminEvent(owner, {
      kind: "role_change",
      subject: targetUserId,
      detail: role,
      outcome: "error",
    });
    return { intent: "set-role" as const, status: "error" as const };
  }
  if (outcome === "ok") {
    return { intent: "set-role" as const, status: "ok" as const };
  }
  if (outcome === "last_owner") {
    return { intent: "set-role" as const, status: "sole_owner" as const };
  }
  return { intent: "set-role" as const, status: "missing" as const };
}

/**
 * The signed-in member leaving their OWN seat — a STEP-UP ceremony gated only by membership
 * (any member may leave themselves): guard → requireStepUp → removeSeat on the actor's own
 * user id. On success the seat is gone and the person is sent to the workspaces index. The
 * sole owner is refused honestly.
 */
async function leaveIntent(request: Request, ws: string, formData: FormData) {
  const actor = await requireMember(request, ws);
  const stepUp = await requireStepUp(request, formData);
  if (!stepUp.ok) {
    await recordAdminEvent(actor, {
      kind: "leave",
      subject: actor.userId,
      detail: "step_up",
      outcome: "denied",
    });
    return { intent: "leave" as const, status: "step_up" as const, error: stepUp.error };
  }
  let outcome: SeatMutationRefusal | "ok";
  try {
    outcome = await removeSeat(actor, ws, actor.userId, "membership_removed");
  } catch {
    await recordAdminEvent(actor, { kind: "leave", subject: actor.userId, outcome: "error" });
    return { intent: "leave" as const, status: "error" as const };
  }
  if (outcome === "last_owner") {
    return { intent: "leave" as const, status: "sole_owner" as const };
  }
  // ok, or missing (the seat is already gone — a race): either way the person is out. Home.
  throw redirect("/workspaces");
}

export default function WorkspaceMembers() {
  const { ws, isOwner, selfUserId, roster, invitations, mailArmed, invitePolicy, address, origin } =
    useLoaderData<typeof loader>();
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
      <MembersSection
        roster={roster}
        isOwner={isOwner}
        selfUserId={selfUserId}
        mailArmed={mailArmed}
        invitePolicy={invitePolicy}
      />
      <PendingInvitations invitations={invitations} isOwner={isOwner} />
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
 * The roster panel: the seat table rendered as people — display name, login address, role.
 * Every member may attempt an invite (the invite-policy gates it); role and removal controls
 * render only for a confirmed OWNER, keyed by the seat's USER ID. The SOLE owner's own seat
 * carries neither (nothing safe to do — you can't remove or demote the last owner; the honest
 * lockout the data layer also enforces). Ownership transfers by promoting another seat first.
 */
function MembersSection({
  roster,
  isOwner,
  selfUserId,
  mailArmed,
  invitePolicy,
}: {
  roster: RosterSeat[];
  isOwner: boolean;
  selfUserId: string;
  mailArmed: boolean;
  invitePolicy: "members" | "owners";
}) {
  const ownerCount = roster.filter((s) => s.role === "owner").length;
  return (
    <section aria-labelledby="members-heading" className="space-y-3">
      <div className="space-y-1">
        <SectionHeading>
          <span id="members-heading">Members</span>
        </SectionHeading>
        <p className="text-dim text-sm">
          A seat is membership — who can sign in here, enroll devices, and publish. Reviewers can
          also approve proposals.
        </p>
      </div>
      <Card className="overflow-hidden">
        <ul>
          {roster.map((seat) => {
            const soleOwner = seat.role === "owner" && ownerCount <= 1;
            return (
              <li
                key={seat.userId}
                className="flex min-h-12 flex-wrap items-center gap-x-4 gap-y-2 border-line-soft border-b px-4 py-3 last:border-b-0"
              >
                <span className="text-ink text-sm">
                  {seat.display}
                  {seat.userId === selfUserId && <span className="text-faint text-xs"> (you)</span>}
                </span>
                <span className="text-faint text-xs">{seat.email}</span>
                <Chip tone={seat.role === "owner" ? "accent" : "neutral"}>{seat.role}</Chip>
                {isOwner &&
                  (soleOwner ? (
                    <span className="ml-auto text-faint text-xs">workspace owner</span>
                  ) : (
                    <span className="ml-auto flex flex-wrap items-center justify-end gap-2">
                      <RoleForm userId={seat.userId} display={seat.display} role={seat.role} />
                      <RemoveMemberForm userId={seat.userId} display={seat.display} />
                    </span>
                  ))}
              </li>
            );
          })}
          {roster.length === 0 && <li className="px-4 py-3 text-faint text-sm">No seats yet.</li>}
        </ul>
      </Card>
      <InviteMemberForm mailArmed={mailArmed} invitePolicy={invitePolicy} isOwner={isOwner} />
    </section>
  );
}
