import { useEffect, useRef } from "react";
import { useFetcher } from "react-router";
import { buttonClasses } from "@/components/ui";

/** The review route's typed reply (shared approve/reject shape; this fetcher reads the reject arm). */
interface ReviewActionData {
  status:
    | "approved"
    | "rejected"
    | "conflict"
    | "self_approve"
    | "not_open"
    | "reason_required"
    | "denied"
    | "error";
  submittedReason?: string;
}

/**
 * The variant relabels the flow for the proposer's own proposal: the four-eyes gate withholds
 * approve only, so withdrawing stays live — the same row resolve under the proposer's own
 * name, verdict `withdrawn`, reason still mandatory. The intent posted is `reject` or
 * `withdraw`.
 */
const COPY = {
  reject: {
    summary: "Reject with a reason…",
    label: "Why this shouldn't become current",
    submit: "Reject proposal",
    pending: "Rejecting…",
    done: "Rejected — the reason is recorded on the proposal.",
  },
  withdraw: {
    summary: "Withdraw your proposal…",
    label: "Why you're withdrawing it",
    submit: "Withdraw proposal",
    pending: "Withdrawing…",
    done: "Withdrawn — the reason is recorded on the proposal.",
  },
} as const;

/**
 * The reject leg, folded into a collapsible so the reason is deliberate, never a stray click:
 * opening it reveals the MANDATORY reason textarea (the server records and discloses it on the
 * review surfaces). Posts `intent=reject` (or `withdraw`) to the review route's action;
 * `version_id` names the candidate — a reject moves no pointer, so no generation rides it. The
 * typed reason echoes back on a non-success so nothing is lost; a successful post resets the
 * field.
 */
export function RejectDialog({
  versionId,
  variant = "reject",
}: {
  versionId: string;
  /** "withdraw" relabels the flow for the proposer's own proposal — the resolve is the same row. */
  variant?: "reject" | "withdraw";
}) {
  const fetcher = useFetcher<ReviewActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  const copy = COPY[variant];
  const formRef = useRef<HTMLFormElement>(null);

  // React Router does not reset a fetcher form after submit; clear the field once, on a landed
  // success. A non-success keeps the typed text via the echoed submittedReason below.
  useEffect(() => {
    if (fetcher.state === "idle" && state?.status === "rejected") {
      formRef.current?.reset();
    }
  }, [fetcher.state, state]);

  return (
    <details className="group">
      <summary className="cursor-pointer select-none font-mono text-[13px] text-dim hover:text-ink">
        {copy.summary}
      </summary>
      <fetcher.Form ref={formRef} method="post" className="mt-3 flex flex-col gap-2">
        <input type="hidden" name="intent" value={variant === "withdraw" ? "withdraw" : "reject"} />
        <input type="hidden" name="version_id" value={versionId} />
        <label className="block">
          <span className="mb-1 block font-medium text-sm text-dim">{copy.label}</span>
          <textarea
            name="reason"
            required
            rows={3}
            maxLength={2000}
            placeholder="Say why — the proposer and every reviewer will see it."
            // The echoed submittedReason (keyed, so the node remounts) keeps the typed text through
            // a non-success re-render.
            key={state?.submittedReason ?? "initial"}
            defaultValue={state?.submittedReason ?? ""}
            className="block w-full rounded-md border border-line px-3 py-2 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
          />
        </label>
        <div>
          <button
            type="submit"
            disabled={pending}
            className={`${buttonClasses("danger")} min-h-11`}
          >
            {pending ? copy.pending : copy.submit}
          </button>
        </div>
      </fetcher.Form>
      {state?.status === "rejected" && (
        <p className="mt-2 text-sm text-dim" role="status">
          {copy.done}
        </p>
      )}
      {state?.status === "reason_required" && (
        <p className="mt-2 text-red-600 text-sm" role="alert">
          A rejection needs a reason — say why, in up to 2000 characters.
        </p>
      )}
      {state?.status === "not_open" && (
        <p className="mt-2 text-sm text-dim" role="status">
          This proposal is no longer open — the page has refreshed with the outcome.
        </p>
      )}
      {(state?.status === "denied" || state?.status === "self_approve") && (
        <p className="mt-2 text-red-600 text-sm" role="alert">
          The server declined this decision.
        </p>
      )}
      {state?.status === "error" && (
        <p className="mt-2 text-red-600 text-sm" role="alert">
          That didn&apos;t go through — nothing was decided. A retry is safe.
        </p>
      )}
    </details>
  );
}
