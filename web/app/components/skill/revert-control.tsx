import { useFetcher } from "react-router";
import { ConfirmButton } from "@/components/confirm";

/** The action's typed reply on the skill-history route (intent=revert). */
interface RevertActionData {
  status: "reverted" | "conflict" | "denied" | "error";
  /** On `denied`, the display copy the control renders. */
  reason?: string;
}

/**
 * The per-row "Roll back to this version" control on a non-current history row, rendered only for
 * a viewer whose seat can decide (owner|reviewer). It wears a lightweight in-place confirm so a
 * roll back is deliberate, never a stray click — the first click arms ("Roll back — confirm?"),
 * the second posts. The hidden `expected_generation` binds the LIVE current generation the history
 * page rendered against — the server refuses a moved pointer instead of rolling back over
 * something the reviewer didn't see, and that same CAS makes an accidental double-submit refuse
 * honestly (the first one moved the pointer). `good_version_id` is this row's target (the history
 * route reads ws + skill from the URL). A success or conflict revalidates the page; the state
 * below is the fallback copy for outcomes that leave this control mounted.
 */
export function RevertControl({
  good,
  expectedGeneration,
}: {
  /** The GOOD target — this row's version id (a full 64-char lowercase-hex version). */
  good: string;
  /** The live current generation the history page rendered against (the revert's CAS binding). */
  expectedGeneration: string;
}) {
  const fetcher = useFetcher<RevertActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;

  return (
    <div>
      <fetcher.Form method="post" className="flex flex-col gap-2">
        <input type="hidden" name="intent" value="revert" />
        <input type="hidden" name="expected_generation" value={expectedGeneration} />
        <input type="hidden" name="good_version_id" value={good} />
        <p className="text-xs text-dim">
          Roll back moves the team&apos;s current pointer to this version&apos;s bytes — a forward
          move, nothing is deleted; you can roll forward again.
        </p>
        <div>
          <ConfirmButton
            label="Roll back to this version"
            confirmLabel="Roll back — confirm?"
            tone="quiet"
            pendingLabel="Rolling back…"
            pending={pending}
          />
        </div>
      </fetcher.Form>
      {state?.status === "reverted" && (
        <p className="mt-2 text-sm text-dim" role="status">
          Rolled back — this version&apos;s bytes are the team&apos;s current version.
        </p>
      )}
      {state?.status === "conflict" && (
        <p className="mt-2 text-sm text-ink" role="alert">
          The pointer moved while you were here — nothing was rolled back. Reload to roll back
          against today&apos;s current.
        </p>
      )}
      {state?.status === "denied" && (
        <p className="mt-2 text-red-600 text-sm" role="alert">
          {state.reason}
        </p>
      )}
      {state?.status === "error" && (
        <p className="mt-2 text-red-600 text-sm" role="alert">
          That didn&apos;t go through — nothing was rolled back. A retry is safe.
        </p>
      )}
    </div>
  );
}
