import { useEffect, useState } from "react";
import { useFetcher } from "react-router";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses } from "@/components/ui";

/** The settings route's typed reply for `intent=remove` (a landed removal revalidates the row away). */
interface RemoveActionData {
  intent: "remove";
  status: "removed" | "last_owner" | "denied" | "error" | "step_up";
  /** The step-up failure copy (wrong password / rate limited) — rendered inline. */
  error?: string;
}

/**
 * The per-seat Remove control — a STEP-UP ceremony. Collapsed, it is one danger button; expanded,
 * it becomes a small confirm panel that re-asks for the acting owner's password (the visible half
 * of the server's `requireStepUp`) before the instant-revoke posts. A fresh `request_id` (a
 * client-minted UUID generated at SUBMIT so SSR never bakes one — no hydration mismatch) rides the
 * post so the vault's op is idempotent on a retry. A successful removal revalidates the page and
 * the row disappears; the server's refusals — a wrong password, the honest last-owner lockout, or a
 * lapsed acting gate — render inline, never a crash.
 */
export function RemoveMemberForm({ email }: { email: string }) {
  const fetcher = useFetcher<RemoveActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  const [open, setOpen] = useState(false);

  // A landed removal revalidates the row away (in production the seat is gone); collapse the panel
  // so the returning state is the clean row, not a stale ceremony.
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
      onSubmit={(event) => {
        event.preventDefault();
        const data = new FormData(event.currentTarget);
        // Mint the idempotency key at submit, never at render — SSR must not bake a UUID.
        data.set("request_id", crypto.randomUUID());
        fetcher.submit(data, { method: "post" });
      }}
    >
      <input type="hidden" name="intent" value="remove" />
      <input type="hidden" name="email" value={email} />
      <p className="text-dim text-sm">
        Remove <span className="font-medium text-ink">{email}</span>? Their devices lose access
        immediately; the local copies they already hold stay theirs.
      </p>
      <StepUpFields idPrefix={`remove-${email}`} />
      {state?.status === "step_up" && (
        <p className="text-red-700 text-xs" role="alert">
          {state.error}
        </p>
      )}
      {state?.status === "last_owner" && (
        <p className="text-red-700 text-xs" role="alert">
          Can&apos;t remove the last owner.
        </p>
      )}
      {state?.status === "denied" && (
        <p className="text-red-700 text-xs" role="alert">
          Only the workspace owner can manage members.
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
