import { useState } from "react";
import { useFetcher } from "react-router";
import { StepUpConfirm } from "@/components/policy/step-up-confirm";
import { Card, SectionHeading } from "@/components/ui";

interface InviteFetcherData {
  error?: string;
}

type InvitePolicy = "members" | "owners";

const COPY: Record<InvitePolicy, string> = {
  members: "Any confirmed member can invite",
  owners: "Only owners can invite",
};

/**
 * Who may invite teammates. An owner picks between "any member" and "owners only"; the choice
 * stages until Save + the password confirm land it (`intent=set-invite-policy`). A non-owner sees
 * the current setting read-only. HONEST COPY: an invitation always seats a plain MEMBER — this knob
 * only decides who may SEND one; roles are raised later on the members page, never here.
 */
export function InvitePolicyPanel({
  isOwner,
  invitePolicy,
}: {
  isOwner: boolean;
  invitePolicy: InvitePolicy;
}) {
  return (
    <section aria-labelledby="invite-policy-heading" className="space-y-3">
      <SectionHeading>
        <span id="invite-policy-heading">Who may invite</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-sm text-dim">
          Invitations always seat a member — this only decides who may send one. Roles are raised
          later on the members page.
        </p>
        {isOwner ? (
          <InvitePolicyControl current={invitePolicy} />
        ) : (
          <p className="text-ink text-sm">
            Inviting is currently open to{" "}
            <span className="font-medium">
              {invitePolicy === "owners" ? "owners only" : "every member"}
            </span>
            . Only an owner can change this.
          </p>
        )}
      </Card>
    </section>
  );
}

function InvitePolicyControl({ current }: { current: InvitePolicy }) {
  const fetcher = useFetcher<InviteFetcherData>();
  const [staged, setStaged] = useState<InvitePolicy>(current);
  const pending = fetcher.state !== "idle";
  const dirty = staged !== current;
  const error = fetcher.data?.error;
  return (
    <fetcher.Form method="post" className="space-y-3">
      <input type="hidden" name="intent" value="set-invite-policy" />
      <fieldset className="space-y-2">
        <legend className="sr-only">Who may invite</legend>
        {(["members", "owners"] as const).map((option) => (
          <label key={option} className="flex items-center gap-2 text-ink text-sm">
            <input
              type="radio"
              name="invite_policy"
              value={option}
              checked={staged === option}
              disabled={pending}
              onChange={() => setStaged(option)}
              className="accent-accent"
            />
            {COPY[option]}
          </label>
        ))}
      </fieldset>
      {dirty && (
        <StepUpConfirm
          idPrefix="invite-policy"
          saveLabel="Save invite policy"
          pending={pending}
          error={error}
          onCancel={() => setStaged(current)}
        />
      )}
    </fetcher.Form>
  );
}
