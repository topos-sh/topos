import { useFetcher } from "react-router";
import { buttonClasses } from "@/components/ui";

/** The action's typed reply on the skill-history route (intent=revert). */
interface RevertActionData {
  status: "reverted" | "conflict" | "denied" | "error";
  /** On `denied`, the display copy the control renders (role-gate substitution or a verbatim reason). */
  reason?: string;
}

/**
 * The per-row "Roll back to this version" control on a non-current history row, rendered only for a
 * viewer whose seat can decide (owner|reviewer). The confirm step lives behind a collapsible so a
 * roll back is deliberate, never a stray click (the RejectDialog pattern). The hidden `request_id`
 * is render-minted by the loader (a retried submit replays the same idempotent outcome), and the
 * hidden `expected_epoch`/`seq` bind the LIVE current generation the history page rendered against —
 * the server refuses a moved pointer instead of rolling back against something the reviewer didn't
 * see. `good_version_id` is this row's target (the history route reads ws + skill from the URL). The
 * button disables while the fetcher is in flight — LOAD-BEARING: it prevents the double-submit the
 * server would otherwise handle as a benign OP_ID_REUSED. A success or conflict revalidates the
 * page; the state below is the fallback copy for outcomes that leave this control mounted.
 */
export function RevertControl({
  good,
  requestId,
  expectedEpoch,
  expectedSeq,
}: {
  /** The GOOD target — this row's version id (a full 64-char lowercase-hex accepted version). */
  good: string;
  requestId: string;
  /** The live current generation the history page rendered against (the revert's CAS binding). */
  expectedEpoch: string;
  expectedSeq: string;
}) {
  const fetcher = useFetcher<RevertActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;

  return (
    <details className="group">
      <summary className="cursor-pointer select-none font-mono text-xs text-faint hover:text-ink">
        Roll back to this version…
      </summary>
      <fetcher.Form method="post" className="mt-2 flex flex-col gap-2">
        <input type="hidden" name="intent" value="revert" />
        <input type="hidden" name="request_id" value={requestId} />
        <input type="hidden" name="expected_epoch" value={expectedEpoch} />
        <input type="hidden" name="expected_seq" value={expectedSeq} />
        <input type="hidden" name="good_version_id" value={good} />
        <p className="text-xs text-dim">
          Roll back moves the team&apos;s current pointer to this version&apos;s bytes — a forward
          move, nothing is deleted; you can roll forward again.
        </p>
        <div>
          <button type="submit" disabled={pending} className={`${buttonClasses("quiet")} min-h-9`}>
            {pending ? "Rolling back…" : "Roll back to this version"}
          </button>
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
          That didn&apos;t go through — nothing was rolled back. A retry is safe: it resumes this
          same request.
        </p>
      )}
    </details>
  );
}
