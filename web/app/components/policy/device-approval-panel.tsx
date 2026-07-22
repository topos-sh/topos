import { useState } from "react";
import { useFetcher } from "react-router";
import { type LastSetLine, LastSetNote } from "@/components/policy/last-set-line";
import { SaveControls } from "@/components/policy/save-controls";
import { Card, SectionHeading } from "@/components/ui";

interface DeviceApprovalFetcherData {
  error?: string;
}

type DeviceApproval = "off" | "on";

/**
 * The device-approval knob — whether a non-owner's NEWLY LINKED device waits for an owner. Off
 * (the default): a member's device links and receives immediately. On: new device links are born
 * pending until an owner approves them, on the Linked devices page (Settings → Devices); nothing
 * is delivered over a pending link. A link an owner creates is always its own approval — active
 * immediately. Owner-only, a plain dirty-reveal save like every policy knob.
 */
export function DeviceApprovalPanel({
  isOwner,
  deviceApproval,
  lastSet,
}: {
  isOwner: boolean;
  deviceApproval: DeviceApproval;
  lastSet: LastSetLine | null;
}) {
  return (
    <section aria-labelledby="device-approval-heading" className="space-y-3">
      <SectionHeading>
        <span id="device-approval-heading">Device approval</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-sm text-dim">
          Whether a member&apos;s newly linked device needs an owner&apos;s approval before this
          workspace delivers to it. When on, a new device link waits for an owner&apos;s approval on
          the Linked devices page before anything is delivered; a link an owner creates is active
          immediately.
        </p>
        {isOwner ? (
          <DeviceApprovalControl current={deviceApproval} />
        ) : (
          <p className="text-ink text-sm">
            Device approval is currently{" "}
            <span className="font-medium">{deviceApproval === "on" ? "required" : "off"}</span>.
            Only an owner can change this.
          </p>
        )}
        <LastSetNote lastSet={lastSet} describe={(v) => (v === "on" ? "required" : "off")} />
      </Card>
    </section>
  );
}

function DeviceApprovalControl({ current }: { current: DeviceApproval }) {
  const fetcher = useFetcher<DeviceApprovalFetcherData>();
  const [staged, setStaged] = useState<DeviceApproval>(current);
  const pending = fetcher.state !== "idle";
  const dirty = staged !== current;
  const error = fetcher.data?.error;
  return (
    <fetcher.Form method="post" className="space-y-3">
      <input type="hidden" name="intent" value="set-device-approval" />
      <fieldset className="space-y-2">
        <legend className="sr-only">Device approval</legend>
        <label className="flex items-center gap-2 text-ink text-sm">
          <input
            type="radio"
            name="device_approval"
            value="off"
            checked={staged === "off"}
            disabled={pending}
            onChange={() => setStaged("off")}
            className="accent-accent"
          />
          Off — a member&apos;s newly linked device receives immediately
        </label>
        <label className="flex items-center gap-2 text-ink text-sm">
          <input
            type="radio"
            name="device_approval"
            value="on"
            checked={staged === "on"}
            disabled={pending}
            onChange={() => setStaged("on")}
            className="accent-accent"
          />
          Required — newly linked devices wait for an owner&apos;s approval on the Linked devices
          page
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
