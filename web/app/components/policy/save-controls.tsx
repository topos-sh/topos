import { buttonClasses } from "@/components/ui";

/**
 * The save tail every policy knob reveals once its value differs from the saved one: any typed
 * error the last attempt returned, and the Save / Cancel pair — a PLAIN submit, no added
 * friction (settings saves are ordinary owner acts; the guard is the ceremony). It only
 * renders while the control is DIRTY, so a knob at rest shows no buttons — the pair appears
 * exactly when there is a change to commit, and Cancel returns the control to the saved value.
 */
export function SaveControls({
  saveLabel,
  pending,
  error,
  onCancel,
}: {
  saveLabel: string;
  pending: boolean;
  /** The last attempt's typed error (a bounds refusal, a server fault) — inline, next to the fix. */
  error?: string;
  onCancel: () => void;
}) {
  return (
    <div className="space-y-3 border-line-soft border-t pt-3">
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
