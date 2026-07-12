import { useFetcher } from "react-router";
import { buttonClasses } from "@/components/ui";

/** The settings route's typed reply for `intent=remove` (a landed removal revalidates the row away). */
interface RemoveActionData {
  intent: "remove";
  status: "removed" | "missing" | "last_owner" | "denied" | "error";
}

/**
 * The per-seat Remove control. Posts `intent=remove` with the target `email` and a fresh
 * `request_id` (a client-minted UUID, generated at click so SSR never bakes one — no hydration
 * mismatch) to the settings route's action. A successful removal revalidates the page and the row
 * disappears; the server's refusals render inline — the honest denial (not the workspace owner /
 * would orphan the last owner), never a crash.
 */
export function RemoveMemberForm({ email }: { email: string }) {
  const fetcher = useFetcher<RemoveActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;

  if (state?.status === "denied") {
    return (
      <span className="text-red-700 text-xs" role="alert">
        Only the workspace owner can manage members.
      </span>
    );
  }
  if (state?.status === "last_owner") {
    return (
      <span className="text-red-700 text-xs" role="alert">
        Can&apos;t remove the last owner.
      </span>
    );
  }
  if (state?.status === "missing") {
    return <span className="text-faint text-xs">not on the roster</span>;
  }
  return (
    <span className="inline-flex items-center gap-2">
      {state?.status === "error" && (
        <span className="text-red-700 text-xs" role="alert">
          that didn&apos;t go through
        </span>
      )}
      <button
        type="button"
        disabled={pending}
        onClick={() =>
          fetcher.submit(
            { intent: "remove", email, request_id: crypto.randomUUID() },
            { method: "post" },
          )
        }
        className={buttonClasses("danger")}
      >
        {pending ? "Removing…" : "Remove"}
      </button>
    </span>
  );
}
