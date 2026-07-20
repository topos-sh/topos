import { CheckIcon, CopyIcon } from "lucide-react";
import { useCopied } from "./use-copied";

/**
 * The command block's copy affordance: a bordered ghost that fills on hover — phosphor on the
 * dark glass (the default), accent ink on a light surface (`tone="light"`). Regular size is
 * min-h-9, the app's button-baseline tap target; `compact` drops to the inline-tab size for
 * header rows where it sits beside small triggers.
 */
const TONE = {
  glass:
    "border-accent-phos text-accent-phos hover:bg-accent-phos hover:text-glass focus-visible:outline-accent-phos",
  light:
    "border-accent text-accent hover:bg-accent hover:text-on-accent focus-visible:outline-accent",
};

export function CopyButton({
  text,
  ariaLabel,
  tone = "glass",
  compact = false,
}: {
  text: string;
  ariaLabel?: string;
  tone?: keyof typeof TONE;
  compact?: boolean;
}) {
  const { copied, copy } = useCopied();

  return (
    <button
      type="button"
      aria-label={ariaLabel}
      className={`inline-flex shrink-0 items-center gap-1.5 rounded-md border font-mono font-medium text-[11px] transition-colors focus-visible:outline-2 focus-visible:outline-offset-2 active:scale-[0.98] ${
        compact ? "px-2 py-0.5" : "min-h-9 px-3"
      } ${TONE[tone]}`}
      onClick={() => copy(text)}
    >
      {copied ? (
        <CheckIcon aria-hidden className="h-3 w-3" />
      ) : (
        <CopyIcon aria-hidden className="h-3 w-3" />
      )}
      {copied ? "copied" : "copy"}
    </button>
  );
}
