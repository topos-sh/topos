import { Form } from "react-router";
import { buttonClasses } from "@/components/ui";
import type { VerificationContext } from "@/lib/plane/wire";
import { DeviceIdentity } from "./DeviceIdentity";

/**
 * The signed-in STANDUP approve: approving CREATES the workspace and makes this device its OWNER. A
 * consent screen (like `gh` / `claude` login) — the action and its button are the focus; the device
 * identity is compact, secondary context below. The name field is editable, prefilled from the signed-in
 * email's local part; approving with the default untouched is a complete standup (rename later in
 * settings). The action reads the session's verified email itself — the form posts the intent, the user
 * code, and the name. A navigation form: the action redirects back with a display-only outcome.
 */
export function ApproveStandupCard({
  userCode,
  context,
  sessionEmail,
  defaultName,
}: {
  userCode: string;
  context: VerificationContext;
  sessionEmail: string;
  defaultName: string;
}) {
  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-col gap-1 text-center">
        <p className="font-medium text-faint text-xs uppercase tracking-wide">
          Device verification
        </p>
        <h1 className="font-display font-semibold text-ink text-xl tracking-[-0.02em]">
          Create your workspace
        </h1>
        <p className="text-dim text-sm">
          This device becomes its <span className="font-medium text-ink">owner</span>.
        </p>
      </div>
      <Form method="post" className="flex w-full flex-col gap-3">
        <input type="hidden" name="intent" value="approve-standup" />
        <input type="hidden" name="user_code" value={userCode} />
        <label className="block text-left">
          <span className="mb-1 block font-medium text-dim text-sm">Workspace name</span>
          <input
            type="text"
            name="display_name"
            defaultValue={defaultName}
            maxLength={120}
            className="block h-11 w-full rounded-md border border-line px-3 text-ink text-sm focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25"
          />
        </label>
        <button type="submit" className={`${buttonClasses("primary")} min-h-11 w-full`}>
          Approve — create workspace as {sessionEmail}
        </button>
      </Form>
      <DeviceIdentity machineName={context.machine_name} fingerprint={context.device_fingerprint} />
      <p className="border-line-soft border-t pt-4 text-center text-dim text-sm">
        Not you? Ignore this page — nothing happens.
      </p>
    </div>
  );
}
