import { useState } from "react";
import { useFetcher } from "react-router";
import { type LastSetLine, LastSetNote } from "@/components/policy/last-set-line";
import { SaveControls } from "@/components/policy/save-controls";
import { msToDaysString } from "@/components/policy/staleness-window-panel";
import { Card, SectionHeading } from "@/components/ui";

interface SessionMaxAgeFetcherData {
  error?: string;
}

const FIELD_CLASSES =
  "block h-11 w-40 rounded-md border border-line px-3 text-sm text-ink placeholder:text-faint focus:border-accent focus:outline-none focus:ring-2 focus:ring-accent/25";

/**
 * The session expiry: how long a `topos login` session stays valid before the machine must log
 * in again. Entered in DAYS (hour granularity — the action rounds to the nearest hour and
 * converts to milliseconds; the setter bounds it to 1 hour .. 366 days); an EMPTY field means
 * sessions never expire (the default). Enforcement is the session guard's — an over-age session
 * resolves to nothing from the next request, no sweep needed. An owner edits and saves (the
 * owner guard is the whole ceremony); a non-owner sees the current policy read-only.
 */
export function SessionMaxAgePanel({
  isOwner,
  sessionMaxAgeMs,
  lastSet,
}: {
  isOwner: boolean;
  sessionMaxAgeMs: number | null;
  lastSet: LastSetLine | null;
}) {
  const days = sessionMaxAgeMs === null ? "" : msToDaysString(sessionMaxAgeMs);
  return (
    <section aria-labelledby="session-max-age-heading" className="space-y-3">
      <SectionHeading>
        <span id="session-max-age-heading">Session expiry</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-3">
        <p className="text-sm text-dim">
          How long a machine&apos;s login lasts. Past this age a session stops working and the
          machine logs in again — skills already on it stay put. Default: sessions do not expire.
        </p>
        {isOwner ? (
          <SessionMaxAgeControl initialDays={days} />
        ) : (
          <p className="text-ink text-sm">
            {days === "" ? (
              <>Sessions currently do not expire.</>
            ) : (
              <>
                Sessions currently expire after <span className="font-medium">{days} days</span>.
              </>
            )}{" "}
            Only an owner can change this.
          </p>
        )}
        <LastSetNote
          lastSet={lastSet}
          describe={(v) => {
            if (v === "off") {
              return "no expiry";
            }
            const ms = Number(v);
            return Number.isFinite(ms) && ms > 0 ? `${msToDaysString(ms)} days` : (v ?? "—");
          }}
        />
      </Card>
    </section>
  );
}

function SessionMaxAgeControl({ initialDays }: { initialDays: string }) {
  const fetcher = useFetcher<SessionMaxAgeFetcherData>();
  const [value, setValue] = useState(initialDays);
  const pending = fetcher.state !== "idle";
  const dirty = value.trim() !== initialDays;
  const error = fetcher.data?.error;
  return (
    <fetcher.Form method="post" className="space-y-3">
      <input type="hidden" name="intent" value="set-session-max-age" />
      <label className="block">
        <span className="mb-1 block font-medium text-sm text-dim">Session expiry (days)</span>
        <input
          type="number"
          name="session_max_age_days"
          min="0"
          step="any"
          inputMode="decimal"
          placeholder="no expiry"
          value={value}
          disabled={pending}
          onChange={(event) => setValue(event.target.value)}
          className={FIELD_CLASSES}
        />
      </label>
      <p className="text-faint text-xs">
        Between 1 hour and 366 days; hour granularity is fine (0.5 = 12 hours). Leave empty for no
        expiry.
      </p>
      {dirty && (
        <SaveControls
          saveLabel="Save session expiry"
          pending={pending}
          error={error}
          onCancel={() => setValue(initialDays)}
        />
      )}
    </fetcher.Form>
  );
}
