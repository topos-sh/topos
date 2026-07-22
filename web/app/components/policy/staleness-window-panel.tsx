import { useState } from "react";
import { useFetcher } from "react-router";
import { type LastSetLine, LastSetNote } from "@/components/policy/last-set-line";
import { SaveControls } from "@/components/policy/save-controls";
import { Card, SectionHeading } from "@/components/ui";

interface StalenessFetcherData {
  error?: string;
}

const MS_PER_DAY = 86_400_000;

/** Render a window (ms) as a clean day count — trimmed to hour precision, no trailing zeros. */
export function msToDaysString(ms: number): string {
  return String(Math.round((ms / MS_PER_DAY) * 24) / 24);
}

const FIELD_CLASSES =
  "block h-11 w-40 rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

/**
 * The fleet clock: how long a device may go without reporting before the fleet page calls it
 * stale. Entered in DAYS (hour granularity is fine — the action rounds to the nearest hour and
 * converts to milliseconds; the database bounds it to 1ms .. 366 days). An owner edits it and
 * saves (the owner guard is the whole ceremony); a non-owner sees the current window read-only.
 */
export function StalenessWindowPanel({
  isOwner,
  stalenessWindowMs,
  lastSet,
}: {
  isOwner: boolean;
  stalenessWindowMs: number;
  lastSet: LastSetLine | null;
}) {
  const days = msToDaysString(stalenessWindowMs);
  return (
    <section aria-labelledby="staleness-heading" className="space-y-3">
      <SectionHeading>
        <span id="staleness-heading">Staleness window</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-sm text-dim">
          A device is called stale on the fleet page once it goes this long without reporting.
          Default is 7 days.
        </p>
        {isOwner ? (
          <StalenessWindowControl initialDays={days} />
        ) : (
          <p className="text-ink text-sm">
            The window is currently <span className="font-medium">{days} days</span>. Only an owner
            can change this.
          </p>
        )}
        <LastSetNote
          lastSet={lastSet}
          describe={(v) => {
            const ms = Number(v);
            return Number.isFinite(ms) && ms > 0 ? `${msToDaysString(ms)} days` : (v ?? "—");
          }}
        />
      </Card>
    </section>
  );
}

function StalenessWindowControl({ initialDays }: { initialDays: string }) {
  const fetcher = useFetcher<StalenessFetcherData>();
  const [value, setValue] = useState(initialDays);
  const pending = fetcher.state !== "idle";
  const dirty = value.trim() !== initialDays;
  const error = fetcher.data?.error;
  return (
    <fetcher.Form method="post" className="space-y-3">
      <input type="hidden" name="intent" value="set-staleness-window" />
      <label className="block">
        <span className="mb-1 block font-medium text-sm text-dim">Staleness window (days)</span>
        <input
          type="number"
          name="staleness_days"
          required
          min="0"
          step="any"
          inputMode="decimal"
          value={value}
          disabled={pending}
          onChange={(event) => setValue(event.target.value)}
          className={FIELD_CLASSES}
        />
      </label>
      <p className="text-faint text-xs">
        Between 1 hour and 366 days. Hour granularity is fine (0.5 = 12 hours).
      </p>
      {dirty && (
        <SaveControls
          saveLabel="Save staleness window"
          pending={pending}
          error={error}
          onCancel={() => setValue(initialDays)}
        />
      )}
    </fetcher.Form>
  );
}
