import type { ReactNode } from "react";
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
 * The "+ new" affordance on the Skills section — a dialog of copyable lines, never a form. The web
 * app NEVER authors a bundle: publishing runs on the enrolled DEVICE that holds the bytes. The
 * dialog hands the exact lines, composed for THIS workspace with its real address (loader-provided,
 * never computed client-side), and leaves the skill name / directory as placeholders the person
 * fills. `<skill>` and `<path-to-skill-directory>` are the honest stand-ins.
 */
export function PublishDialog({
  shareAddress,
  trigger,
}: {
  shareAddress: string;
  trigger: ReactNode;
}) {
  const agentPrompt = "Share my <skill> skill with the team on Topos — publish it.";
  const followCommand = `topos follow ${shareAddress}`;
  const publishCommand = "topos publish <path-to-skill-directory>";
  return (
    <Dialog>
      <DialogTrigger asChild>{trigger}</DialogTrigger>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>Publish from your agent</DialogTitle>
          <DialogDescription>
            Topos never writes a skill here — publishing runs on the enrolled device that holds the
            bytes. Ask the agent you already have, or run it yourself.
          </DialogDescription>
        </DialogHeader>

        <section aria-labelledby="publish-agent-heading" className="space-y-2">
          <SectionHeading>
            <span id="publish-agent-heading">Ask your agent</span>
          </SectionHeading>
          <CommandBlock command={agentPrompt} copyLabel="Copy the agent prompt" />
        </section>

        <section aria-labelledby="publish-cli-heading" className="space-y-2">
          <SectionHeading>
            <span id="publish-cli-heading">Or run it yourself</span>
          </SectionHeading>
          <p className="text-dim text-sm">
            If this device isn&apos;t enrolled yet, follow the workspace once:
          </p>
          <CommandBlock command={followCommand} copyLabel="Copy the follow command" />
          <p className="text-dim text-sm">Then publish the skill&apos;s directory:</p>
          <CommandBlock command={publishCommand} copyLabel="Copy the publish command" />
        </section>
      </DialogContent>
    </Dialog>
  );
}
