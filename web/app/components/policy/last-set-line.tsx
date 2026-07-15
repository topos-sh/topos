import { relativeTime } from "@/components/format";

/**
 * One knob's "last set by" facts, shaped by the settings loader from the newest `ok` audit row
 * of that knob's kind (the setters land those rows in their own transactions; refused attempts
 * record as `denied` and never surface here). `value` is the audit subject — the value that was
 * set, in the setter's own vocabulary — which each panel maps to its own words.
 */
export interface LastSetLine {
  value: string | null;
  by: string;
  at: string | Date;
}

/**
 * The shared history line under a policy knob. `describe` turns the audit subject into the
 * panel's own words ("ON", "owners only", "14 days"); an unmapped subject falls through
 * verbatim — honest, never invented.
 */
export function LastSetNote({
  lastSet,
  describe,
}: {
  lastSet: LastSetLine | null;
  describe: (value: string | null) => string;
}) {
  return (
    <p className="text-faint text-xs">
      {lastSet === null
        ? "Not set from this dashboard yet."
        : `Last set: ${describe(lastSet.value)}, by ${lastSet.by}, ${relativeTime(new Date(lastSet.at))}`}
    </p>
  );
}
