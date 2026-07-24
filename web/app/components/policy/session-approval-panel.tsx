import { useState } from "react";
import { useFetcher } from "react-router";
import { type LastSetLine, LastSetNote } from "@/components/policy/last-set-line";
import { SaveControls } from "@/components/policy/save-controls";
import { Card, SectionHeading } from "@/components/ui";

interface SessionApprovalFetcherData {
  error?: string;
}

type SessionApproval = "off" | "on";

/**
 * The session-approval knob — whether a non-owner's NEW LOGIN waits for an owner. Off
 * (the default): a member's device links and receives immediately. On: new device links are born
 * pending until an owner approves them, on the Sessions page (Settings → Sessions); nothing
 * is delivered over a pending link. A link an owner creates is always its own approval — active
 * immediately. Owner-only, a plain dirty-reveal save like every policy knob.
 */
export function SessionApprovalPanel({
  isOwner,
  sessionApproval,
  lastSet,
}: {
  isOwner: boolean;
  sessionApproval: SessionApproval;
  lastSet: LastSetLine | null;
}) {
  return (
    <section aria-labelledby="session-approval-heading" className="space-y-3">
      <SectionHeading>
        <span id="session-approval-heading">Session approval</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-sm text-dim">
          Whether a member&apos;s new login needs an owner&apos;s approval before this workspace
          delivers to it. When on, a new session waits for an owner&apos;s approval on the Sessions
          page before anything is delivered; a session an owner mints is active immediately.
        </p>
        {isOwner ? (
          <SessionApprovalControl current={sessionApproval} />
        ) : (
          <p className="text-ink text-sm">
            Session approval is currently{" "}
            <span className="font-medium">{sessionApproval === "on" ? "required" : "off"}</span>.
            Only an owner can change this.
          </p>
        )}
        <LastSetNote lastSet={lastSet} describe={(v) => (v === "on" ? "required" : "off")} />
      </Card>
    </section>
  );
}

function SessionApprovalControl({ current }: { current: SessionApproval }) {
  const fetcher = useFetcher<SessionApprovalFetcherData>();
  const [staged, setStaged] = useState<SessionApproval>(current);
  const pending = fetcher.state !== "idle";
  const dirty = staged !== current;
  const error = fetcher.data?.error;
  return (
    <fetcher.Form method="post" className="space-y-3">
      <input type="hidden" name="intent" value="set-session-approval" />
      <fieldset className="space-y-2">
        <legend className="sr-only">Session approval</legend>
        <label className="flex items-center gap-2 text-ink text-sm">
          <input
            type="radio"
            name="session_approval"
            value="off"
            checked={staged === "off"}
            disabled={pending}
            onChange={() => setStaged("off")}
            className="accent-accent"
          />
          Off — a member&apos;s new login receives immediately
        </label>
        <label className="flex items-center gap-2 text-ink text-sm">
          <input
            type="radio"
            name="session_approval"
            value="on"
            checked={staged === "on"}
            disabled={pending}
            onChange={() => setStaged("on")}
            className="accent-accent"
          />
          Required — new logins wait for an owner&apos;s approval on the Sessions page
        </label>
      </fieldset>
      {dirty && (
        <SaveControls
          saveLabel={staged === "on" ? "Require approval" : "Turn approval off"}
          pending={pending}
          error={error}
          onCancel={() => setStaged(current)}
        />
      )}
    </fetcher.Form>
  );
}
