import { useFetcher } from "react-router";
import { ConfirmButton } from "@/components/confirm";
import { Card, SectionHeading } from "@/components/ui";

/** The members route's typed reply for `intent=leave` (a landed leave redirects, carrying no data). */
interface LeaveActionData {
  intent: "leave";
  status: "sole_owner" | "error";
}

/**
 * The signed-in person's own "Leave this workspace" ceremony — a guard-gated act on THEIR seat,
 * worn as a lightweight in-place confirm (the danger button arms on the first click, the second
 * posts). On success the action redirects to the workspaces index (the fetcher follows it), so the
 * workspace simply drops off the rail. A sole owner is refused honestly — transfer ownership first,
 * the workspace must always have an owner — rather than being allowed to orphan the workspace.
 */
export function LeaveWorkspaceForm() {
  const fetcher = useFetcher<LeaveActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;

  return (
    <section aria-labelledby="leave-heading" className="space-y-3">
      <SectionHeading>
        <span id="leave-heading">Leave</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-dim text-sm">
          Leaving removes your seat and cuts off every one of your devices from this workspace. The
          local copies you already hold stay yours.
        </p>
        <fetcher.Form method="post" className="space-y-3">
          <input type="hidden" name="intent" value="leave" />
          {state?.status === "sole_owner" && (
            <p className="text-red-700 text-xs" role="alert">
              You&apos;re the only owner. Make another member an owner first — the workspace must
              always have an owner.
            </p>
          )}
          {state?.status === "error" && (
            <p className="text-red-700 text-xs" role="alert">
              That didn&apos;t go through.
            </p>
          )}
          <div>
            <ConfirmButton
              label="Leave workspace"
              confirmLabel="Leave — confirm?"
              tone="danger"
              pendingLabel="Leaving…"
              pending={pending}
            />
          </div>
        </fetcher.Form>
      </Card>
    </section>
  );
}
