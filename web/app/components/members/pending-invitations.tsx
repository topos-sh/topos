import { useState } from "react";
import { useFetcher } from "react-router";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses, Card, Chip, SectionHeading } from "@/components/ui";

/** One open invitation as the members page renders it (loader-shaped; lapse pre-computed). */
export interface PendingInvitationView {
  id: string;
  email: string;
  /** 'pending' (a live claim) or 'declined' (the recorded "no thanks" — re-invitable). */
  status: "pending" | "declined";
  invitedByDisplay: string;
  /** "lapses in 6 days" / "lapsed" — computed in the loader so hydration re-reads no clock. */
  lapse: string;
}

/** The members route's typed reply for `intent=revoke-invitation`. */
interface RevokeInvitationActionData {
  intent: "revoke-invitation";
  status: "revoked" | "missing" | "step_up" | "error";
  invitationId: string;
  error?: string;
}

/**
 * The claims-in-flight panel: invitations that no user has bound yet, plus DECLINED ones — the
 * recorded "no thanks" the inviter should see (re-inviting the address supersedes it). Every
 * member may see the list (who was invited is roster-adjacent fact, not a secret); the REVOKE
 * arm is owner-only, pending-only, and a step-up ceremony. Re-inviting an address is just
 * inviting it again — the pending row upserts, a fresh link mails, and the 7-day clock re-arms,
 * so there is no separate resend control here.
 */
export function PendingInvitations({
  invitations,
  isOwner,
}: {
  invitations: PendingInvitationView[];
  isOwner: boolean;
}) {
  if (invitations.length === 0) {
    return null;
  }
  return (
    <section aria-labelledby="invitations-heading" className="space-y-3">
      <div className="space-y-1">
        <SectionHeading>
          <span id="invitations-heading">Pending invitations</span>
        </SectionHeading>
        <p className="text-dim text-sm">
          Each becomes a seat the moment its address accepts the mailed link (or signs up and
          verifies the mailbox). Invite an address again to mail a fresh link and re-arm its clock.
        </p>
      </div>
      <Card className="overflow-hidden">
        <ul>
          {invitations.map((invitation) => (
            <li
              key={invitation.id}
              className="flex min-h-12 flex-wrap items-center gap-x-4 gap-y-2 border-line-soft border-b px-4 py-3 last:border-b-0"
            >
              <span className="text-ink text-sm">{invitation.email}</span>
              {invitation.status === "pending" ? (
                <Chip tone="pending">pending</Chip>
              ) : (
                <Chip tone="unverified">declined</Chip>
              )}
              <span className="text-faint text-xs">
                invited by {invitation.invitedByDisplay}
                {invitation.status === "pending" ? ` · ${invitation.lapse}` : ""}
              </span>
              {isOwner && invitation.status === "pending" && (
                <span className="ml-auto">
                  <RevokeInvitationForm invitationId={invitation.id} email={invitation.email} />
                </span>
              )}
            </li>
          ))}
        </ul>
      </Card>
    </section>
  );
}

/**
 * The per-invitation revoke — owner + step-up: the un-invite before anyone binds the claim. A
 * landed revoke revalidates the row away; refusals render inline.
 */
function RevokeInvitationForm({ invitationId, email }: { invitationId: string; email: string }) {
  const fetcher = useFetcher<RevokeInvitationActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data?.invitationId === invitationId ? fetcher.data : undefined;
  const [open, setOpen] = useState(false);

  if (!open) {
    return (
      <button type="button" onClick={() => setOpen(true)} className={buttonClasses("danger")}>
        Revoke
      </button>
    );
  }

  return (
    <fetcher.Form
      method="post"
      className="w-full max-w-sm space-y-3 rounded-md border border-line-soft bg-panel2 p-3"
    >
      <input type="hidden" name="intent" value="revoke-invitation" />
      <input type="hidden" name="invitation_id" value={invitationId} />
      <p className="text-dim text-sm">
        Revoke the invitation for <span className="font-medium text-ink">{email}</span>? Signing up
        under it will no longer seat them.
      </p>
      <StepUpFields idPrefix={`revoke-invitation-${invitationId}`} />
      {state?.status === "step_up" && (
        <p className="text-red-700 text-xs" role="alert">
          {state.error}
        </p>
      )}
      {state?.status === "missing" && (
        <p className="text-red-700 text-xs" role="alert">
          This invitation is no longer pending — reload to see the current list.
        </p>
      )}
      {state?.status === "error" && (
        <p className="text-red-700 text-xs" role="alert">
          That didn&apos;t go through.
        </p>
      )}
      <div className="flex items-center gap-2">
        <button type="submit" disabled={pending} className={buttonClasses("danger")}>
          {pending ? "Revoking…" : "Revoke invitation"}
        </button>
        <button type="button" onClick={() => setOpen(false)} className={buttonClasses("quiet")}>
          Cancel
        </button>
      </div>
    </fetcher.Form>
  );
}
