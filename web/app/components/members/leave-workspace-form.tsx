import { useState } from "react";
import { useFetcher } from "react-router";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses, Card, SectionHeading } from "@/components/ui";

/** The members route's typed reply for `intent=leave` (a landed leave redirects, carrying no data). */
interface LeaveActionData {
  intent: "leave";
  status: "sole_owner" | "step_up" | "error";
  /** The step-up failure copy — rendered inline on a wrong password / rate limit. */
  error?: string;
}

/**
 * The signed-in person's own "Leave this workspace" ceremony — a STEP-UP act on THEIR seat. On
 * success the action redirects to the workspaces index (the fetcher follows it), so the workspace
 * simply drops off the rail. A sole owner is refused honestly — transfer ownership first, the
 * workspace must always have an owner — rather than being allowed to orphan the workspace.
 */
export function LeaveWorkspaceForm() {
  const fetcher = useFetcher<LeaveActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data;
  const [open, setOpen] = useState(false);

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
        {open ? (
          <fetcher.Form method="post" className="max-w-sm space-y-3">
            <input type="hidden" name="intent" value="leave" />
            <StepUpFields idPrefix="leave" />
            {state?.status === "step_up" && (
              <p className="text-red-700 text-xs" role="alert">
                {state.error}
              </p>
            )}
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
            <div className="flex items-center gap-2">
              <button type="submit" disabled={pending} className={buttonClasses("danger")}>
                {pending ? "Leaving…" : "Leave workspace"}
              </button>
              <button
                type="button"
                onClick={() => setOpen(false)}
                className={buttonClasses("quiet")}
              >
                Cancel
              </button>
            </div>
          </fetcher.Form>
        ) : (
          <button type="button" onClick={() => setOpen(true)} className={buttonClasses("danger")}>
            Leave this workspace
          </button>
        )}
      </Card>
    </section>
  );
}
