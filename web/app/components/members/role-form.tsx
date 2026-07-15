import { useEffect, useState } from "react";
import { useFetcher } from "react-router";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses } from "@/components/ui";

/** The members route's typed reply for `intent=set-role`. */
interface RoleActionData {
  intent: "set-role";
  status: "ok" | "sole_owner" | "missing" | "step_up" | "error";
  /** The step-up failure copy — rendered inline on a wrong password / rate limit. */
  error?: string;
}

type SeatRole = "owner" | "reviewer" | "member";

/** Owner outranks reviewer outranks member — the select lists them low-to-high. */
const ROLE_OPTIONS: SeatRole[] = ["member", "reviewer", "owner"];

/**
 * The per-seat role control — a STEP-UP ceremony keyed by the seat's USER ID (the one
 * identity; the display name is what a human reads). Collapsed, it is a quiet button;
 * expanded, it is a small panel with a role select (preselected to the seat's current role)
 * and the acting owner's password re-entry. A landed change revalidates the row and the role
 * chip re-renders, so the panel closes itself. The data layer refuses demoting the sole owner
 * (`last_owner`) — surfaced honestly here as "the workspace must keep an owner", never
 * swallowed.
 */
export function RoleForm({
  userId,
  display,
  role,
}: {
  userId: string;
  display: string;
  role: SeatRole;
}) {
  const fetcher = useFetcher<RoleActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (fetcher.state === "idle" && state?.status === "ok") {
      setOpen(false);
    }
  }, [fetcher.state, state]);

  if (!open) {
    return (
      <button type="button" onClick={() => setOpen(true)} className={buttonClasses("quiet")}>
        Change role
      </button>
    );
  }

  return (
    <fetcher.Form
      method="post"
      className="w-full max-w-sm space-y-3 rounded-md border border-line-soft bg-panel2 p-3"
    >
      <input type="hidden" name="intent" value="set-role" />
      <input type="hidden" name="user_id" value={userId} />
      <label className="block" htmlFor={`role-${userId}-select`}>
        <span className="mb-1 block font-medium text-sm text-dim">
          Role for <span className="text-ink">{display}</span>
        </span>
        <select
          id={`role-${userId}-select`}
          name="role"
          // Keyed by the current role so a landed change re-seeds the default cleanly on the next
          // open; uncontrolled so an unsaved pick survives a wrong-password re-render.
          key={role}
          defaultValue={role}
          className="block h-11 w-full rounded-md border border-line px-3 text-sm text-ink focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
        >
          {ROLE_OPTIONS.map((option) => (
            <option key={option} value={option}>
              {option}
            </option>
          ))}
        </select>
      </label>
      <StepUpFields idPrefix={`role-${userId}`} />
      {state?.status === "step_up" && (
        <p className="text-red-700 text-xs" role="alert">
          {state.error}
        </p>
      )}
      {state?.status === "sole_owner" && (
        <p className="text-red-700 text-xs" role="alert">
          The workspace must keep an owner. Make another member an owner first.
        </p>
      )}
      {state?.status === "missing" && (
        <p className="text-red-700 text-xs" role="alert">
          This seat no longer exists — reload to see the current roster.
        </p>
      )}
      {state?.status === "error" && (
        <p className="text-red-700 text-xs" role="alert">
          That didn&apos;t go through.
        </p>
      )}
      <div className="flex items-center gap-2">
        <button type="submit" disabled={pending} className={buttonClasses("primary")}>
          {pending ? "Saving…" : "Save role"}
        </button>
        <button type="button" onClick={() => setOpen(false)} className={buttonClasses("quiet")}>
          Cancel
        </button>
      </div>
    </fetcher.Form>
  );
}
