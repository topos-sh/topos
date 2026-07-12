import { CommandBlock } from "@/components/command-block";
import { Card, SectionHeading } from "@/components/ui";
import {
  agentHandoffText,
  buildApproveCommand,
  buildDiffCommand,
  buildRejectCommand,
} from "@/lib/diff/command";
import { CopyCommand } from "./CopyCommand";
import type { ReviewStatus } from "./ProposalStatusBanner";

/**
 * The CLI hand-off: the same decision on an enrolled device, as copyable commands (the device
 * authenticates the action — nothing signs). The browser decision panel is the primary surface
 * now — the page renders this collapsed ("Prefer the CLI?") on reviewable states and in full only
 * when the proposal's status couldn't be confirmed. Degrades per status. `skill` is the catalog
 * NAME (commands address a skill by its user-facing name).
 */
export function ApproveHandoff({
  skill,
  versionId,
  status,
}: {
  skill: string;
  versionId: string;
  /** The page renders this only for pending/stale/unknown; the type is the full status for the
   *  caller's convenience — only `stale` and `unknown` earn their own extra line. */
  status: ReviewStatus;
}) {
  const approveCommand = buildApproveCommand(skill, versionId);
  return (
    <Card className="flex flex-col gap-4 p-4">
      <div className="flex flex-col gap-1">
        <SectionHeading>Decide on an enrolled device</SectionHeading>
        <p className="text-sm text-dim">
          The same decision as a command — run it where the skill is enrolled.
        </p>
        {status === "stale" ? (
          <p className="text-sm text-ink">
            current moved since this was proposed — an approve will be refused as stale; a fresh
            propose from the device is the path.
          </p>
        ) : null}
        {status === "unknown" ? (
          <p className="text-sm text-ink">
            This proposal&apos;s status couldn&apos;t be confirmed — the commands below still work
            on an enrolled device; a stale approve is refused there.
          </p>
        ) : null}
      </div>
      <CommandBlock command={approveCommand} copyLabel={`Copy ${approveCommand}`} />
      <div className="flex flex-col gap-2">
        <CommandRow text={buildDiffCommand(skill, versionId)} />
        <CommandRow text={buildRejectCommand(skill, versionId)} />
      </div>
      <div className="flex items-center justify-between gap-3 border-line-soft border-t pt-3">
        <p className="text-sm text-dim">
          Working with an agent? Copy a ready-to-paste instruction instead.
        </p>
        <CopyCommand text={agentHandoffText(skill, versionId)} label="Copy for your agent" />
      </div>
      <p className="text-xs text-faint">
        After you approve on your device, this proposal closes — refresh to see it.
      </p>
    </Card>
  );
}

function CommandRow({ text }: { text: string }) {
  return (
    <div className="flex items-center gap-2">
      <code className="min-w-0 flex-1 overflow-x-auto whitespace-nowrap rounded-md border border-line-soft bg-ground px-3 py-2.5 font-mono text-xs text-dim">
        {text}
      </code>
      <CopyCommand text={text} ariaLabel={`Copy ${text}`} />
    </div>
  );
}
