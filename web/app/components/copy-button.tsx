import { useCopied } from "./use-copied";

/**
 * The glass command block's copy affordance: phosphor-bordered ghost, fills on hover.
 * min-h-9 = the app's button-baseline tap target (the text alone would render ~30px).
 */
export function CopyButton({ text, ariaLabel }: { text: string; ariaLabel?: string }) {
  const { copied, copy } = useCopied();

  return (
    <button
      type="button"
      aria-label={ariaLabel}
      className="inline-flex min-h-9 shrink-0 items-center rounded-md border border-accent-phos px-3 font-mono font-medium text-[11px] text-accent-phos transition-colors hover:bg-accent-phos hover:text-glass focus-visible:outline-2 focus-visible:outline-accent-phos focus-visible:outline-offset-2 active:scale-[0.98]"
      onClick={() => copy(text)}
    >
      {copied ? "copied" : "copy"}
    </button>
  );
}
