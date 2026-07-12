import { useFetcher } from "react-router";
import { Switch } from "@/components/ui/switch";

/**
 * The review-gate toggle: one Switch posting `intent=set-review-required` to the settings route's
 * action (the workspace comes from the route's own params). `checked` is the directory's real
 * review-required value. While a toggle is in flight the position moves optimistically off the
 * submitted form data; when the action's revalidate lands, the switch settles on the fresh
 * `checked` — so a denied or failed write snaps back to the real state rather than stranding on a
 * value that never took.
 */
export function ReviewGateSwitch({ checked }: { checked: boolean }) {
  const fetcher = useFetcher();
  const pending = fetcher.state !== "idle";
  // Optimistic: while a submit is in flight, show the value being sent; otherwise the real value.
  const optimistic = fetcher.formData
    ? fetcher.formData.get("review_required") === "true"
    : checked;
  return (
    <div className="flex items-center gap-3">
      <Switch
        id="review-gate-switch"
        checked={optimistic}
        disabled={pending}
        onCheckedChange={(next) =>
          fetcher.submit(
            { intent: "set-review-required", review_required: next ? "true" : "false" },
            { method: "post" },
          )
        }
      />
      <label htmlFor="review-gate-switch" className="select-none text-ink text-sm">
        Require review for every change
      </label>
    </div>
  );
}
