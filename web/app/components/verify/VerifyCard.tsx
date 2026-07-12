import type { VerificationContext } from "@/lib/plane/wire";
import { DeviceIdentity } from "./DeviceIdentity";
import { DomainBadge } from "./DomainBadge";
import { SkillOfferList } from "./SkillOfferList";

/**
 * The verification disclosure a human reviews before signing in — the confused-deputy guard, shown in the
 * signed-OUT state. Every value here is server-reported and renders as text; the page carries zero client
 * JS. The heading + what-happens copy lead; the device identity (machine + fingerprint) is compact,
 * secondary context below — matching the signed-in consent cards. The copy branches on the session's
 * intent: an enroll session JOINS the named workspace; a standup session would CREATE one.
 */
export function VerifyCard({ context }: { context: VerificationContext }) {
  const standup = context.intent === "standup";
  return (
    <div className="flex flex-col gap-6">
      <div className="flex flex-col gap-2 text-center">
        <p className="font-medium text-faint text-xs uppercase tracking-wide">
          Device verification
        </p>
        <h1 className="font-display font-semibold text-ink text-xl tracking-[-0.02em]">
          Is this your device?
        </h1>
        {standup ? (
          <p className="text-dim text-sm">
            This device wants to <span className="font-medium">create a new workspace</span> — sign
            in to name and approve it.
          </p>
        ) : (
          <div className="flex flex-col items-center gap-2">
            <p className="text-dim text-sm">
              This device will join{" "}
              <span className="font-medium">{context.workspace_display_name}</span>
            </p>
            <DomainBadge domain={context.verified_domain} status={context.verified_domain_status} />
          </div>
        )}
        <SkillOfferList skills={context.offered_skills} />
      </div>
      <DeviceIdentity machineName={context.machine_name} fingerprint={context.device_fingerprint} />
      <p className="border-line-soft border-t pt-4 text-center text-dim text-sm">
        Not you? Ignore this page — nothing happens.
      </p>
    </div>
  );
}
