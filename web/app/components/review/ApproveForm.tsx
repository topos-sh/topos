import { useFetcher } from "react-router";
import { buttonClasses } from "@/components/ui";

/**
 * The review route's typed reply. Approve and reject share the route action (dispatched on the
 * `intent` field) but occupy SEPARATE fetchers, so each reads only its own outcome; the union is
 * shared so the action returns one shape.
 */
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
 * The approve form — one primary action, posting `intent=approve` to the review route's action.
 * The hidden `expected_generation` is the SAME render's current generation — the exact base the
 * diff above was computed against, so the server refuses a moved pointer instead of approving
 * something the reviewer didn't see; the CAS itself makes a retried approve idempotent (a move
 * that already landed answers success). The hidden `version_id` names the candidate. A success,
 * a conflict, or a resolved race re-renders the page (the state below is the fallback copy for
 * outcomes that leave this panel mounted).
 */
export function ApproveForm({
  versionId,
  expectedGeneration,
}: {
  versionId: string;
  expectedGeneration: string;
}) {
  const fetcher = useFetcher<ReviewActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;

  return (
    <div className="flex flex-col gap-2">
      <fetcher.Form method="post">
        <input type="hidden" name="intent" value="approve" />
        <input type="hidden" name="version_id" value={versionId} />
        <input type="hidden" name="expected_generation" value={expectedGeneration} />
        <button type="submit" disabled={pending} className={`${buttonClasses("primary")} min-h-11`}>
          {pending ? "Approving…" : "Approve — make this current"}
        </button>
      </fetcher.Form>
      {state?.status === "approved" && (
        <p className="text-sm text-dim" role="status">
          Approved — this candidate is the team&apos;s current version.
        </p>
      )}
      {state?.status === "conflict" && (
        <p className="text-sm text-ink" role="alert">
          current moved while you reviewed — nothing was approved. The page has refreshed against
          today&apos;s current.
        </p>
      )}
      {state?.status === "self_approve" && (
        <p className="text-sm text-ink" role="alert">
          The server refused: under review-required, the proposer may not approve their own proposal
          — a different owner or reviewer decides.
        </p>
      )}
      {state?.status === "not_open" && (
        <p className="text-sm text-dim" role="status">
          This proposal is no longer open — the page has refreshed with the outcome.
        </p>
      )}
      {state?.status === "denied" && (
        <p className="text-red-600 text-sm" role="alert">
          The server declined this decision.
        </p>
      )}
      {state?.status === "error" && (
        <p className="text-red-600 text-sm" role="alert">
          That didn&apos;t go through — nothing was decided. A retry is safe.
        </p>
      )}
    </div>
  );
}
