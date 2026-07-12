import { Form } from "react-router";
import { buttonClasses } from "@/components/ui";
import type { VerificationContext } from "@/lib/plane/wire";
import { DeviceIdentity } from "./DeviceIdentity";
import { DomainBadge } from "./DomainBadge";
import { SkillOfferList } from "./SkillOfferList";

/**
 * The signed-in ENROLL approve: joining the named workspace. A consent screen (like `gh` / `claude`
 * login) — the workspace being joined and the Approve button are the focus; the device identity is
 * compact, secondary context below. The action reads the session's verified email itself — this form
 * posts only the intent + the user code (the route reads the code from its own param too). A
 * navigation form: the action redirects back to the verify page with a display-only outcome.
 */
export function ApproveEnrollCard({
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
          Join {context.workspace_display_name}
        </h1>
        <DomainBadge domain={context.verified_domain} status={context.verified_domain_status} />
        <SkillOfferList skills={context.offered_skills} />
      </div>
      <Form method="post" className="w-full">
        <input type="hidden" name="intent" value="approve-enroll" />
        <input type="hidden" name="user_code" value={userCode} />
        <button type="submit" className={`${buttonClasses("primary")} min-h-11 w-full`}>
          Approve — join as {sessionEmail}
        </button>
      </Form>
      <DeviceIdentity machineName={context.machine_name} fingerprint={context.device_fingerprint} />
      <p className="border-line-soft border-t pt-4 text-center text-dim text-sm">
        Not you? Ignore this page — nothing happens.
      </p>
    </div>
  );
}
