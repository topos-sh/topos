import { useEffect, useState } from "react";
import { useFetcher } from "react-router";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses } from "@/components/ui";

/** The members route's typed reply for `intent=remove` (a landed removal revalidates the row away). */
interface RemoveActionData {
  intent: "remove";
  status: "removed" | "last_owner" | "missing" | "error" | "step_up";
  /** The step-up failure copy (wrong password / rate limited) — rendered inline. */
  error?: string;
}

/**
 * The per-seat Remove control — a STEP-UP ceremony keyed by the seat's USER ID. Collapsed, it
 * is one danger button; expanded, it becomes a small confirm panel that re-asks for the acting
 * owner's password (the visible half of the server's `requireStepUp`) before the removal
 * posts. Removal deletes the seat in one fenced transaction — delivery to the person's devices
 * ends with it, and the copies they already hold freeze in place (the fleet page chases them).
 * A successful removal revalidates the page and the row disappears; the server's refusals — a
 * wrong password, the honest last-owner lockout — render inline, never a crash.
 */
export function RemoveMemberForm({ userId, display }: { userId: string; display: string }) {
  const fetcher = useFetcher<RemoveActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  const [open, setOpen] = useState(false);

  // A landed removal revalidates the row away; collapse the panel so the returning state is the
  // clean row, not a stale ceremony.
  useEffect(() => {
    if (fetcher.state === "idle" && state?.status === "removed") {
      setOpen(false);
    }
  }, [fetcher.state, state]);

  if (!open) {
    return (
      <button type="button" onClick={() => setOpen(true)} className={buttonClasses("danger")}>
        Remove
      </button>
    );
  }

  return (
    <fetcher.Form
      method="post"
      className="w-full max-w-sm space-y-3 rounded-md border border-line-soft bg-panel2 p-3"
    >
      <input type="hidden" name="intent" value="remove" />
      <input type="hidden" name="user_id" value={userId} />
      <p className="text-dim text-sm">
        Remove <span className="font-medium text-ink">{display}</span>? Their seat is deleted
        immediately; the local copies their devices already hold stay theirs.
      </p>
      <StepUpFields idPrefix={`remove-${userId}`} />
      {state?.status === "step_up" && (
        <p className="text-red-700 text-xs" role="alert">
          {state.error}
        </p>
      )}
      {state?.status === "last_owner" && (
        <p className="text-red-700 text-xs" role="alert">
          The workspace must keep an owner — you can&apos;t remove the last one.
        </p>
      )}
      {state?.status === "missing" && (
        <p className="text-red-700 text-xs" role="alert">
          This seat is already gone — reload to see the current roster.
        </p>
      )}
      {state?.status === "error" && (
        <p className="text-red-700 text-xs" role="alert">
          That didn&apos;t go through.
        </p>
      )}
      <div className="flex items-center gap-2">
        <button type="submit" disabled={pending} className={buttonClasses("danger")}>
          {pending ? "Removing…" : "Remove"}
        </button>
        <button type="button" onClick={() => setOpen(false)} className={buttonClasses("quiet")}>
          Cancel
        </button>
      </div>
    </fetcher.Form>
  );
}
