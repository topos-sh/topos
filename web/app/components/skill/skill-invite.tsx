import { useEffect, useRef, useState } from "react";
import { useFetcher } from "react-router";
import { buttonClasses, Card } from "@/components/ui";

/** The skill route's typed reply for `intent=invite` (mirrors the action's return union). */
interface SkillInviteReply {
  intent: "invite";
  status: "invited" | "owner_required" | "mail_unarmed" | "error";
  /** Echoed on a non-success so the field keeps the typed address through the re-render. */
  submittedEmail?: string;
  /** The address invited, on success. */
  invited?: string;
  /** Whether the notice mail went out (the invitation row stands regardless). */
  emailSent?: boolean;
}

/**
 * "Invite a teammate to this skill" — a quiet, collapsed affordance on the skill face. A member
 * expands it to a single-email form; submitting mints an invitation whose FIRST destination is
 * THIS skill, so the mail leads with the skill and the invitee lands looking at it. Inviting is
 * OWNER-ONLY, and it REQUIRES armed mail (the invitation's identity proof is a mailbox
 * round-trip) — so when mail is unarmed, or the viewer is not an owner, the expanded panel says
 * so honestly instead of offering a form that can only fail. The action re-checks both
 * regardless of what this renders, and its refusals surface here too (a role can change between
 * render and submit).
 */
export function SkillInviteAffordance({
  mailArmed,
  isOwner,
}: {
  mailArmed: boolean;
  isOwner: boolean;
}) {
  const fetcher = useFetcher<SkillInviteReply>();
  const [open, setOpen] = useState(false);
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  const formRef = useRef<HTMLFormElement>(null);

  // React Router does not reset a fetcher form after submit; clear the field once, on a landed
  // invite, so a second teammate can be invited without re-typing over the last address. A
  // non-success keeps the typed address via the echoed submittedEmail below.
  useEffect(() => {
    if (fetcher.state === "idle" && state?.status === "invited") {
      formRef.current?.reset();
    }
  }, [fetcher.state, state]);

  if (!open) {
    return (
      <div className="space-y-2">
        <button type="button" onClick={() => setOpen(true)} className={buttonClasses("quiet")}>
          Invite a teammate
        </button>
        {state?.status === "invited" && (
          <p className="text-dim text-sm" role="status">
            Invited {state.invited} — the mail leads with this skill.
          </p>
        )}
      </div>
    );
  }

  const restricted = !isOwner;
  return (
    <Card className="space-y-3 px-4 py-3">
      <div className="flex items-center justify-between gap-2">
        <span className="font-display text-[10px] text-faint uppercase tracking-[0.12em]">
          Invite a teammate
        </span>
        <button
          type="button"
          onClick={() => setOpen(false)}
          className="text-faint text-xs hover:text-dim"
        >
          Cancel
        </button>
      </div>

      {!mailArmed ? (
        <p className="text-dim text-sm">
          Configure mail (TOPOS_MAIL_SMTP_*) to invite your team — the invitation&apos;s identity
          proof is a mailbox round-trip.
        </p>
      ) : restricted ? (
        <p className="text-dim text-sm">Only a workspace owner can invite (and revoke) members.</p>
      ) : (
        <>
          <fetcher.Form ref={formRef} method="post" className="flex flex-wrap items-end gap-2">
            <input type="hidden" name="intent" value="invite" />
            <label className="block flex-1">
              <span className="mb-1 block font-medium text-dim text-sm">Invite by email</span>
              <input
                type="text"
                name="email"
                required
                autoComplete="off"
                placeholder="teammate@company.com"
                // The echoed submittedEmail (keyed, so the node remounts) keeps the typed address
                // through a denial/error re-render.
                key={state?.submittedEmail ?? "initial"}
                defaultValue={state?.submittedEmail ?? ""}
                className="block h-11 w-full min-w-56 rounded-md border border-line px-3 text-ink text-sm placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
              />
            </label>
            <button
              type="submit"
              disabled={pending}
              className={`${buttonClasses("quiet")} min-h-11`}
            >
              {pending ? "Inviting…" : "Invite"}
            </button>
          </fetcher.Form>
          <p className="text-faint text-xs">
            The invitation leads with this skill — it is the first thing the invitee sees.
          </p>
        </>
      )}

      {state?.status === "invited" && (
        <p className="text-dim text-sm" role="status">
          Invited {state.invited} — the mail leads with this skill.{" "}
          {state.emailSent
            ? "They were emailed the invite; accepting puts this skill in front of them first."
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
          That didn&apos;t go through — check the address and try again.
        </p>
      )}
    </Card>
  );
}
