import { Link } from "react-router";
import { firstLine, relativeTime, utcStamp } from "@/components/format";
import { RevertControl } from "@/components/skill/revert-control";
import { Card, SectionHeading, ShortId } from "@/components/ui";
import { bundlePath, useBundleBase } from "@/lib/bundle-base";
import { useWsPath } from "@/lib/ws-path";

/**
 * One row of first-parent history, shaped by the route loader from the vault's immutable
 * version metadata: WHEN it landed, who is recorded as its author, its message, and its file
 * count. Commit messages and authors render as text nodes only.
 */
export interface HistoryStepView {
  versionId: string;
  author: string;
  message: string;
  /** When this version was committed (epoch milliseconds), as the vault recorded it. */
  createdAtMs: number;
  /**
   * The COMPLETE parent set; the spine follows parents[0]. Every version a client can publish
   * commits ONE parent — a resolved merge lands as a single forward commit on the team's version
   * — so this is a one-element list in practice, and history renders as a straight line.
   */
  parents: string[];
  fileCount: number;
  /**
   * Server-computed: the row sits outside the workspace's history window. Still listed —
   * nothing is deleted under a window — but the roll-back affordance is withheld (the action
   * refuses it regardless), and the row says why.
   */
  locked?: boolean;
}

/**
 * The History tab's data, resolved entirely in the loader: either nothing has published to this
 * skill yet (`published: false`, no `current` base, so no history), or the first-parent walk plus
 * the live current generation every roll-back binds. A CAS on `current` means every non-head row
 * shares the same generation target; `canRevert` is owner|reviewer-only (the action's guard is
 * the authority — this is the matching render lock).
 */
export type HistorySectionData =
  | { published: false }
  | {
      published: true;
      /** The current version id (head of the walk) — non-head rows show the roll-back affordance. */
      head: string;
      canRevert: boolean;
      /** The live current generation the page rendered against (the revert's CAS binding). */
      expectedGeneration: string;
      steps: HistoryStepView[];
      /** The next first-parent id to resume from (`?from=`), or null at genesis / on truncation. */
      cursor: string | null;
      /** True when a mid-walk fetch failed: `steps` is what was reachable. */
      truncated: boolean;
    };

/**
 * First-parent history over the vault's immutable version metadata — the loader walked it and
 * hands the page here (`skill` is the catalog NAME; every link is name-keyed, the workspace prefix
 * from `useWsPath`). A skill with nothing published yet renders an honest empty state; a mid-walk
 * failure ended in a truncation row.
 */
export function HistorySection({ skill, data }: { skill: string; data: HistorySectionData }) {
  const wsPath = useWsPath();
  const base = useBundleBase();
  const basePath = wsPath(bundlePath(base, skill));
  // The version links point at the version file page (`…/versions/{id}`); the PAGING links
  // (second-parent, Show-older) carry the `?from=` cursor on the History tab route itself.
  const historyPath = `${basePath}/history`;
  return (
    <section aria-labelledby="history-heading" className="space-y-2">
      <SectionHeading>
        <span id="history-heading">History</span>
      </SectionHeading>
      {!data.published ? (
        <Card className="px-4 py-3">
          <p className="text-sm text-faint">
            Nothing published yet — this skill has no versions, so there&apos;s no history to show.
          </p>
        </Card>
      ) : (
        <Card>
          <ol>
            {data.steps.map((step) => (
              <li
                key={step.versionId}
                className="flex min-h-12 flex-wrap items-center gap-x-4 gap-y-1 border-line-soft border-b px-4 py-2.5 last:border-b-0"
              >
                <Link
                  to={`${basePath}/versions/${step.versionId}`}
                  className="rounded focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
                >
                  <ShortId value={step.versionId} />
                </Link>
                <span className="font-mono text-xs text-faint">{step.author}</span>
                <span className="min-w-0 flex-1 truncate text-sm text-dim">
                  {firstLine(step.message)}
                </span>
                <time
                  dateTime={new Date(step.createdAtMs).toISOString()}
                  title={utcStamp(step.createdAtMs)}
                  className="text-xs text-faint"
                >
                  {relativeTime(new Date(step.createdAtMs))}
                </time>
                <span className="text-xs text-faint">
                  {step.fileCount === 1 ? "1 file" : `${step.fileCount} files`}
                </span>
                {step.locked === true && (
                  <span className="rounded border border-line-soft px-1.5 py-0.5 text-[11px] text-faint">
                    Outside history window
                  </span>
                )}
                {data.canRevert && step.versionId !== data.head && step.locked !== true && (
                  <div className="w-full">
                    <RevertControl
                      good={step.versionId}
                      expectedGeneration={data.expectedGeneration}
                    />
                  </div>
                )}
              </li>
            ))}
            {data.steps.length === 0 && !data.truncated && (
              <li className="px-4 py-3 text-sm text-faint">No history to show.</li>
            )}
            {data.truncated && (
              <li className="border-line-soft border-t px-4 py-3 text-sm text-faint">
                history continues but the server didn&apos;t answer
              </li>
            )}
            {data.cursor !== null && (
              <li className="border-line-soft border-t px-4 py-2">
                <Link
                  to={`${historyPath}?from=${data.cursor}`}
                  className="inline-flex min-h-9 items-center rounded-md px-2 font-mono text-[13px] text-dim underline decoration-hairline hover:text-ink focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
                >
                  Show older
                </Link>
              </li>
            )}
          </ol>
        </Card>
      )}
    </section>
  );
}
