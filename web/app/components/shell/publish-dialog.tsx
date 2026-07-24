import { type ReactNode, useId } from "react";
import { CommandBlock } from "@/components/command-block";
import { SectionHeading } from "@/components/ui";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
  DialogTrigger,
} from "@/components/ui/dialog";

/**
 * The copyable publish lines, composed for THIS workspace with its real address (loader-provided,
 * never computed client-side). The web app NEVER authors a bundle — publishing runs on the
 * enrolled DEVICE that holds the bytes — so these are honest stand-ins the person fills:
 * `<skill>` and `<path-to-skill-directory>`. ONE component so the dialog and the dashboard's
 * empty-state card can never drift.
 */
/** The one publish agent-prompt line — shared with the dashboard's onboarding checklist. */
export const AGENT_PUBLISH_PROMPT = "Share my <skill> skill with the team on Topos — publish it.";
/** The manual publish command that pairs with it. */
export const PUBLISH_COMMAND = "topos publish <path-to-skill-directory>";

export function PublishInstructions({ shareAddress }: { shareAddress: string }) {
  const agentHeadingId = useId();
  const cliHeadingId = useId();
  const agentPrompt = AGENT_PUBLISH_PROMPT;
  const followCommand = `topos login ${shareAddress}`;
  const publishCommand = PUBLISH_COMMAND;
  return (
    <>
      <section aria-labelledby={agentHeadingId} className="space-y-2">
        <SectionHeading>
          <span id={agentHeadingId}>Ask your agent</span>
        </SectionHeading>
        <CommandBlock command={agentPrompt} copyLabel="Copy the agent prompt" />
      </section>

      <section aria-labelledby={cliHeadingId} className="space-y-2">
        <SectionHeading>
          <span id={cliHeadingId}>Or run it yourself</span>
        </SectionHeading>
        <p className="text-dim text-sm">
          If this device isn&apos;t enrolled yet, follow the workspace once:
        </p>
        <CommandBlock command={followCommand} copyLabel="Copy the follow command" />
        <p className="text-dim text-sm">Then publish the skill&apos;s directory:</p>
        <CommandBlock command={publishCommand} copyLabel="Copy the publish command" />
      </section>
    </>
  );
}

/**
 * The "+ new" affordance on the Skills section — a dialog wrapping the shared publish lines, never
 * a form. The dialog frames the same instructions the empty-state card shows inline.
 */
export function PublishDialog({
  shareAddress,
  trigger,
}: {
  shareAddress: string;
  trigger: ReactNode;
}) {
  return (
    <Dialog>
      <DialogTrigger asChild>{trigger}</DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Publish from your agent</DialogTitle>
          <DialogDescription>
            Topos never writes a skill here — publishing runs on the logged-in machine that holds
            the bytes. Ask the agent you already have, or run it yourself.
          </DialogDescription>
        </DialogHeader>
        <PublishInstructions shareAddress={shareAddress} />
      </DialogContent>
    </Dialog>
  );
}
