import { Form } from "react-router";
import { buttonClasses } from "@/components/ui";
import type { VerificationContext } from "@/lib/plane/wire";
import { DeviceIdentity } from "./DeviceIdentity";

/**
 * The signed-in LOGIN approve: a device re-minting its credentials for every workspace this
 * account holds a confirmed seat in — a sign-in, not a join. The consent copy must say the real
 * operation ("what a human approves is what happens"): no workspace name is anchored here (the
 * session is workspace-less by design), and no skills are offered. The approve itself is the same
 * identity confirmation the enroll leg uses, so the form posts the same intent.
 */
export function ApproveLoginCard({
  userCode,
  context,
  sessionEmail,
}: {
  userCode: string;
  context: VerificationContext;
  sessionEmail: string;
}) {
  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-col gap-2 text-center">
        <p className="font-medium text-faint text-xs uppercase tracking-wide">
          Device verification
        </p>
        <h1 className="font-display font-semibold text-ink text-xl tracking-[-0.02em]">
          Sign this device in
        </h1>
        <p className="text-dim text-sm">
          Approving signs the device below in as you: it receives fresh credentials for every
          workspace where you hold a confirmed seat. No workspace is created and nothing is
          installed by this step.
        </p>
      </div>
      <Form method="post" className="w-full">
        <input type="hidden" name="intent" value="approve-enroll" />
        <input type="hidden" name="user_code" value={userCode} />
        <button type="submit" className={`${buttonClasses("primary")} min-h-11 w-full`}>
          Approve — sign in as {sessionEmail}
        </button>
      </Form>
      <DeviceIdentity machineName={context.machine_name} fingerprint={context.device_fingerprint} />
      <p className="border-line-soft border-t pt-4 text-center text-dim text-sm">
        Not you? Ignore this page — nothing happens.
      </p>
    </div>
  );
}
