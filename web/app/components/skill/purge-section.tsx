import { useFetcher } from "react-router";
import type { HistorySectionData } from "@/components/skill/history-section";
import { StepUpFields } from "@/components/step-up";
import { buttonClasses, Card, SectionHeading, ShortId } from "@/components/ui";

/** The skill-history route's action reply for an intent=purge submit (matched per version id). */
export interface PurgeActionData {
  intent: "purge";
  status: "purged" | "denied" | "error";
  versionId: string;
  /** On `denied`: the display copy (a step-up/typed-name failure, or the vault's mapped reason). */
  message?: string;
}

/**
 * The OWNER-only purge affordance on the History tab — the "leak tool": it drops ONE past version's
 * bytes server-side while its hash stays in history as a tombstone. It never touches the CURRENT
 * version (that row is filtered out here, and the vault refuses `is_current` regardless). Each purge
 * is a deliberate ceremony: re-enter the password (step-up) AND type the skill's name. The control
 * lives in its own section rather than inside the history rows, so the shared history component stays
 * untouched; it lists the same non-current versions the walk found.
 */
export function PurgeSection({
  skill,
  data,
  canPurge,
}: {
  skill: string;
  data: HistorySectionData;
  canPurge: boolean;
}) {
  if (!canPurge || !data.published) {
    return null;
  }
  const purgeable = data.steps.filter((step) => step.versionId !== data.head);
  if (purgeable.length === 0) {
    return null;
  }
  return (
    <section aria-labelledby="purge-heading" className="space-y-3">
      <SectionHeading>
        <span id="purge-heading">Purge version bytes</span>
      </SectionHeading>
      <Card className="space-y-3 px-4 py-4">
        <p className="text-dim text-sm">
          Purging drops one past version&apos;s file bytes from the server. Its hash stays in
          history as a tombstone; only bytes no live version still needs are dropped. Use it to
          scrub a leak.
        </p>
        <ul className="space-y-2">
          {purgeable.map((step) => (
            <li
              key={step.versionId}
              className="border-line-soft border-t pt-2 first:border-t-0 first:pt-0"
            >
              <PurgeControl skill={skill} versionId={step.versionId} />
            </li>
          ))}
        </ul>
      </Card>
    </section>
  );
}

function PurgeControl({ skill, versionId }: { skill: string; versionId: string }) {
  const fetcher = useFetcher<PurgeActionData>();
  const pending = fetcher.state !== "idle";
  const state = fetcher.data?.versionId === versionId ? fetcher.data : undefined;
  return (
    <details>
      <summary className="flex cursor-pointer select-none items-center gap-2 font-mono text-red-700 text-xs hover:text-red-800">
        Purge <ShortId value={versionId} />…
      </summary>
      <fetcher.Form method="post" className="mt-2 space-y-3">
        <input type="hidden" name="intent" value="purge" />
        <input type="hidden" name="version_id" value={versionId} />
        <StepUpFields idPrefix={`purge-${versionId.slice(0, 12)}`} typedName={skill} />
        {state?.status === "purged" && (
          <p className="text-dim text-sm" role="status">
            Purged — this version&apos;s bytes are gone from the server; its hash stays as a
            tombstone.
          </p>
        )}
        {state?.status === "denied" && state.message !== undefined && (
          <p className="text-red-600 text-sm" role="alert">
            {state.message}
          </p>
        )}
        {state?.status === "error" && (
          <p className="text-red-600 text-sm" role="alert">
            That didn&apos;t go through — nothing was purged. A retry is safe.
          </p>
        )}
        <div>
          <button type="submit" disabled={pending} className={buttonClasses("danger")}>
            {pending ? "Purging…" : "Purge this version"}
          </button>
        </div>
      </fetcher.Form>
    </details>
  );
}
