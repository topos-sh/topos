import { useFetcher } from "react-router";
import { ConfirmButton } from "@/components/confirm";

/** The members route's typed reply for `intent=remove` (a landed removal revalidates the row away). */
interface RemoveActionData {
  intent: "remove";
  status: "removed" | "last_owner" | "missing" | "error";
}

/**
 * The per-seat Remove control — a guard-gated audited act keyed by the seat's USER ID, worn as a
 * lightweight in-place confirm: the danger button arms on the first click ("Remove — confirm?"
 * beside a Cancel) and the second click posts, so a stray click never removes anyone and no
 * re-authentication stands between the owner and the act. Removal deletes the seat in one fenced
 * transaction — delivery to the person's devices ends with it, and the copies they already hold
 * freeze in place (the fleet page chases them). A successful removal revalidates the page and the
 * row disappears; the server's refusals — the honest last-owner lockout, a vanished seat, a fault
 * — render inline below.
 */
export function RemoveMemberForm({ userId }: { userId: string }) {
  const fetcher = useFetcher<RemoveActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;

  return (
    <fetcher.Form method="post" className="inline-flex flex-col items-end gap-1">
      <input type="hidden" name="intent" value="remove" />
      <input type="hidden" name="user_id" value={userId} />
      <ConfirmButton label="Remove" tone="danger" pendingLabel="Removing…" pending={pending} />
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
    </fetcher.Form>
  );
}
