import { useState } from "react";
import { useFetcher } from "react-router";
import { StepUpConfirm } from "@/components/policy/step-up-confirm";
import { Switch } from "@/components/ui/switch";

/** The action's reply shape this control reads (the settings route's `set-review-required` branch). */
interface ReviewFetcherData {
  error?: string;
}

/**
 * The review-gate toggle, now a STEP-UP ceremony. Flipping the switch stages a pending value; the
 * password confirm appears, and only Save (with the right password) writes. `checked` is the
 * directory's real review-required value; the switch shows the staged value while an edit is open,
 * then settles back onto `checked` after the loader revalidates — so a denied or wrong-password
 * write snaps back to the real state, and a landed one matches it (the confirm closes because the
 * staged value now equals `checked`). Posts `intent=set-review-required` to the settings route's
 * action (the workspace comes from the route's own params).
 */
export function ReviewGateSwitch({ checked }: { checked: boolean }) {
  const fetcher = useFetcher<ReviewFetcherData>();
  const [staged, setStaged] = useState(checked);
  const pending = fetcher.state !== "idle";
  const dirty = staged !== checked;
  // Any error the action returned (wrong password, a role refusal) — shown while the edit is open.
  const error = fetcher.data?.error;
  return (
    <fetcher.Form method="post" className="space-y-3">
      <input type="hidden" name="intent" value="set-review-required" />
      <input type="hidden" name="review_required" value={staged ? "true" : "false"} />
      <div className="flex items-center gap-3">
        <Switch
          id="review-gate-switch"
          checked={staged}
          disabled={pending}
          onCheckedChange={setStaged}
        />
        <label htmlFor="review-gate-switch" className="select-none text-ink text-sm">
          Require review for every change
        </label>
      </div>
      {dirty && (
        <StepUpConfirm
          idPrefix="review-gate"
          saveLabel={staged ? "Require review" : "Stop requiring review"}
          pending={pending}
          error={error}
          onCancel={() => setStaged(checked)}
        />
      )}
    </fetcher.Form>
  );
}
