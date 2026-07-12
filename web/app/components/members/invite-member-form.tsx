import { useEffect, useRef } from "react";
import { useFetcher } from "react-router";
import { buttonClasses } from "@/components/ui";

/** The settings route's typed reply for `intent=invite`. */
interface InviteActionData {
  intent: "invite";
  status: "invited" | "owner_required" | "error";
  /** Echoed on a non-success so the field keeps the typed addresses. */
  submittedEmails?: string;
  /** The addresses seated, on success. */
  invited?: string[];
  /** Whether the invite email delivered (best-effort — the seats stand regardless). */
  emailSent?: boolean;
}

/**
 * The add-by-email form. Every invitee starts as a MEMBER — roles are raised later in the roster, so
 * there is no role picker. Posts `intent=invite` with one or more addresses (space/comma separated)
 * in the `emails` field to the settings route's action (a guarded roster write; the workspace comes
 * from the route's own params). The database gates who may invite per the workspace's invite-policy —
 * the honest "owners only" copy shows where it refuses. On success the field clears and the seats
 * stand; the workspace ADDRESS (rendered separately) is the share surface a teammate follows.
 */
export function InviteMemberForm() {
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
      {state?.status === "invited" && (
        <p className="text-sm text-dim" role="status">
          Invited {state.invited?.join(", ") ?? "your teammates"} as members.{" "}
          {state.emailSent
            ? "They were emailed the workspace address."
            : "The invite email didn't send — share the workspace address below."}{" "}
          Each joins the moment they enroll a device under their invited email.
        </p>
      )}
      {state?.status === "owner_required" && (
        <p className="text-red-600 text-sm" role="alert">
          Only the workspace owner can invite here.
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
