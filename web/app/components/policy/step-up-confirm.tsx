import { StepUpFields } from "@/components/step-up";
import { buttonClasses } from "@/components/ui";

/**
 * The confirm tail every policy knob reveals once its value differs from the saved one: the
 * password re-entry (the step-up ceremony's visible half), any typed error the last attempt
 * returned, and the Save / Cancel pair. It only renders while the control is DIRTY, so a knob at
 * rest shows no password prompt — the ceremony appears exactly when there is a change to confirm.
 * Submitting posts the enclosing knob's `<fetcher.Form>`; the matching server gate verifies the
 * password immediately before the write, so a wrong password writes NOTHING and returns the error
 * shown here.
 */
export function StepUpConfirm({
  idPrefix,
  saveLabel,
  pending,
  error,
  onCancel,
}: {
  idPrefix: string;
  saveLabel: string;
  pending: boolean;
  /** The last attempt's typed error (wrong password, a bounds refusal) — inline, next to the fix. */
  error?: string;
  onCancel: () => void;
}) {
  return (
    <div className="space-y-3 border-line-soft border-t pt-3">
      <StepUpFields idPrefix={idPrefix} />
      {error !== undefined && (
        <p className="text-red-600 text-sm" role="alert">
          {error}
        </p>
      )}
      <div className="flex flex-wrap gap-2">
        <button type="submit" disabled={pending} className={buttonClasses("primary")}>
          {pending ? "Saving…" : saveLabel}
        </button>
        <button
          type="button"
          disabled={pending}
          onClick={onCancel}
          className={buttonClasses("quiet")}
        >
          Cancel
        </button>
      </div>
    </div>
  );
}
