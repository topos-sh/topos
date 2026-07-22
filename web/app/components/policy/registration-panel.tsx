import { useState } from "react";
import { useFetcher } from "react-router";
import { type LastSetLine, LastSetNote } from "@/components/policy/last-set-line";
import { SaveControls } from "@/components/policy/save-controls";
import { Card, SectionHeading } from "@/components/ui";

interface RegistrationFetcherData {
  error?: string;
}

type Registration = "invite_only" | "open";

/**
 * The registration knob — who may create an account on this install. `invite_only` (the
 * default) admits only addresses holding a pending invitation; `open` disables that proof
 * entirely, and the copy says so without softening: anyone who can reach this origin may create
 * an account (a seat still requires an invitation or a claim — registration is the account
 * rung, not admission). Owner-only.
 */
export function RegistrationPanel({
  isOwner,
  registration,
  lastSet,
}: {
  isOwner: boolean;
  registration: Registration;
  lastSet: LastSetLine | null;
}) {
  return (
    <section aria-labelledby="registration-heading" className="space-y-3">
      <SectionHeading>
        <span id="registration-heading">Registration</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-sm text-dim">
          Whether creating an account here requires an invitation. Open registration disables the
          invitation proof — anyone who can reach this origin may create an account.
        </p>
        {isOwner ? (
          <RegistrationControl current={registration} />
        ) : (
          <p className="text-ink text-sm">
            Registration is currently{" "}
            <span className="font-medium">{registration === "open" ? "open" : "invite-only"}</span>.
            Only an owner can change this.
          </p>
        )}
        <LastSetNote lastSet={lastSet} describe={(v) => (v === "open" ? "open" : "invite-only")} />
      </Card>
    </section>
  );
}

function RegistrationControl({ current }: { current: Registration }) {
  const fetcher = useFetcher<RegistrationFetcherData>();
  const [staged, setStaged] = useState<Registration>(current);
  const pending = fetcher.state !== "idle";
  const dirty = staged !== current;
  const error = fetcher.data?.error;
  return (
    <fetcher.Form method="post" className="space-y-3">
      <input type="hidden" name="intent" value="set-registration" />
      <fieldset className="space-y-2">
        <legend className="sr-only">Registration</legend>
        <label className="flex items-center gap-2 text-ink text-sm">
          <input
            type="radio"
            name="registration"
            value="invite_only"
            checked={staged === "invite_only"}
            disabled={pending}
            onChange={() => setStaged("invite_only")}
            className="accent-accent"
          />
          Invite-only — sign-up requires a pending invitation
        </label>
        <label className="flex items-center gap-2 text-ink text-sm">
          <input
            type="radio"
            name="registration"
            value="open"
            checked={staged === "open"}
            disabled={pending}
            onChange={() => setStaged("open")}
            className="accent-accent"
          />
          Open — anyone reaching this origin may create an account
        </label>
      </fieldset>
      {dirty && (
        <SaveControls
          saveLabel={staged === "open" ? "Open registration" : "Require an invitation"}
          pending={pending}
          error={error}
          onCancel={() => setStaged(current)}
        />
      )}
    </fetcher.Form>
  );
}
