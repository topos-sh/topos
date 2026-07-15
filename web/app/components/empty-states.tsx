/** An empty channel: no skills published yet. Deliberately no CTA — publishing is the agent's move. */
export function NoSkills() {
  return (
    <div className="rounded-lg border border-line-soft border-dashed bg-panel px-6 py-12 text-center">
      <h2 className="font-display font-semibold text-base text-ink tracking-[-0.02em]">
        No skills published yet
      </h2>
      <p className="mx-auto mt-2 max-w-md text-dim text-sm leading-relaxed">
        Publish from your agent — run{" "}
        <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-[13px]">topos publish</code>{" "}
        on an enrolled device and the skill appears here on the next load.
      </p>
    </div>
  );
}
