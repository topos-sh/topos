import { PublishInstructions } from "@/components/shell/publish-dialog";

/**
 * The dashboard's empty state — the enroll-and-publish instruction card. No skill has been
 * published yet, so instead of a dead end it hands the exact copyable lines (the SAME ones the
 * Skills `+ new` dialog shows, via the shared component) composed for THIS workspace's address:
 * ask your agent, or follow + publish on an enrolled device.
 */
export function NoSkills({ shareAddress }: { shareAddress: string }) {
  return (
    <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-8">
      <div className="text-center">
        <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
          No skills published yet
        </h2>
        <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
          Publish your first skill from an enrolled device — ask the agent you already have, or run
          it yourself. It appears here on the next load.
        </p>
      </div>
      <div className="mx-auto mt-6 max-w-md space-y-5">
        <PublishInstructions shareAddress={shareAddress} />
      </div>
    </div>
  );
}
