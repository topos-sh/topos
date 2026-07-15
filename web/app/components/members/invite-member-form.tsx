import { useEffect, useRef } from "react";
import { useFetcher } from "react-router";
import { buttonClasses } from "@/components/ui";

/** The members route's typed reply for `intent=invite`. */
interface InviteActionData {
  intent: "invite";
  status: "invited" | "owner_required" | "mail_unarmed" | "error";
  /** Echoed on a non-success so the field keeps the typed addresses. */
  submittedEmails?: string;
  /** The addresses invited, on success. */
  invited?: string[];
  /** Whether the notice mail went out (the invitation row stands regardless). */
  emailSent?: boolean;
}

/**
 * The invite-by-email form. An invitation is a claim on a FUTURE user: the invitee proves the
 * mailbox at sign-up and the claim becomes a seat — so inviting REQUIRES armed mail, and an
 * unarmed deployment renders this form DISABLED with the honest configure-mail prompt instead
 * of pretending. Every invitee starts as a MEMBER (roles are raised later in the roster, so
 * there is no role picker); invitations lapse after 7 days, and re-inviting the same address
 * re-arms the clock — resending IS inviting again. Posts `intent=invite` with one or more
 * addresses (space/comma separated) in the `emails` field.
 */
export function InviteMemberForm({
  mailArmed,
  invitePolicy,
  isOwner,
}: {
  mailArmed: boolean;
  invitePolicy: "members" | "owners";
  isOwner: boolean;
}) {
  const fetcher = useFetcher<InviteActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  const formRef = useRef<HTMLFormElement>(null);

  // React Router does not reset a fetcher form after submit; clear the field once, on a landed
  // invite. A non-success keeps the typed addresses via the echoed submittedEmails below.
  useEffect(() => {
    if (fetcher.state === "idle" && state?.status === "invited") {
      formRef.current?.reset();
    }
  }, [fetcher.state, state]);

  if (!mailArmed) {
    return (
      <p className="text-dim text-sm">
        Configure mail (TOPOS_MAIL_SMTP_*) to invite your team — the invitation&apos;s identity
        proof is a mailbox round-trip.
      </p>
    );
  }
  if (invitePolicy === "owners" && !isOwner) {
    return (
      <p className="text-dim text-sm">
        Inviting is restricted to owners in this workspace (the invite-policy knob in settings).
      </p>
    );
  }

  return (
    <div className="space-y-3">
      <fetcher.Form ref={formRef} method="post" className="flex flex-wrap items-end gap-2">
        <input type="hidden" name="intent" value="invite" />
        <label className="block flex-1">
          <span className="mb-1 block font-medium text-sm text-dim">Invite by email</span>
          <input
            type="text"
            name="emails"
            required
            autoComplete="off"
            placeholder="teammate@company.com, another@company.com"
            // The echoed submittedEmails (keyed, so the node remounts) keeps the typed addresses
            // through a denial/error re-render.
            key={state?.submittedEmails ?? "initial"}
            defaultValue={state?.submittedEmails ?? ""}
            className="block h-11 w-full min-w-56 rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
          />
        </label>
        <button type="submit" disabled={pending} className={`${buttonClasses("quiet")} min-h-11`}>
          {pending ? "Inviting…" : "Invite"}
        </button>
      </fetcher.Form>
      <p className="text-faint text-xs">
        Invitations lapse after 7 days. Inviting an address again re-sends the mail and re-arms the
        clock.
      </p>
      {state?.status === "invited" && (
        <p className="text-sm text-dim" role="status">
          Invited {state.invited?.join(", ") ?? "your teammates"} as members.{" "}
          {state.emailSent
            ? "They were emailed the workspace address; each joins by signing up under the invited email."
            : "The invitation stands, but the mail didn't send — invite the address again to resend."}
        </p>
      )}
      {state?.status === "owner_required" && (
        <p className="text-red-600 text-sm" role="alert">
          Only a workspace owner can invite here.
        </p>
      )}
      {state?.status === "mail_unarmed" && (
        <p className="text-red-600 text-sm" role="alert">
          Mail isn&apos;t configured on this deployment — set TOPOS_MAIL_SMTP_* to invite.
        </p>
      )}
      {state?.status === "error" && (
        <p className="text-red-600 text-sm" role="alert">
          That didn&apos;t go through — check the addresses and try again.
        </p>
      )}
    </div>
  );
}
