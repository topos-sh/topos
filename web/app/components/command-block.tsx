import { CopyButton } from "@/components/copy-button";

/**
 * The glass command block — the design system's install-command pattern (see ../../DESIGN.md):
 * one command on the dark terminal-glass surface, a faint `$` marker, and the phosphor copy
 * affordance. The one dark object a light page may carry.
 */
export function CommandBlock({ command, copyLabel }: { command: string; copyLabel?: string }) {
  return (
    <div className="flex max-w-full flex-wrap items-center gap-3.5 rounded-md bg-glass px-4 py-3 font-mono text-[13.5px] text-glass-ink">
      <span className="select-none text-glass-faint">$</span>
      <span className="min-w-0 flex-auto break-words">{command}</span>
      <CopyButton text={command} ariaLabel={copyLabel} />
    </div>
  );
}
