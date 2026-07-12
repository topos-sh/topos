import { useCopied } from "@/components/use-copied";

/**
 * The light surfaces' copy affordance: copy a command (or the agent hand-off text) to the
 * clipboard, with a brief "Copied" confirmation. Inline SVG icons — no icon library.
 */
export function CopyCommand({
  text,
  label = "Copy",
  ariaLabel,
}: {
  text: string;
  label?: string;
  ariaLabel?: string;
}) {
  const { copied, copy } = useCopied();

  return (
    <button
      type="button"
      onClick={() => copy(text)}
      aria-label={ariaLabel}
      className="inline-flex min-h-11 items-center justify-center gap-1.5 rounded-md border border-line px-3 font-mono text-[13px] text-dim transition-colors hover:bg-panel2 focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
    >
      {copied ? (
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
          <path
            d="M13.5 4.5 6.75 11.25 3 7.5"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.7"
            strokeLinecap="round"
            strokeLinejoin="round"
          />
        </svg>
      ) : (
        <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
          <rect
            x="5.5"
            y="5.5"
            width="8"
            height="8"
            rx="1.5"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.4"
          />
          <path
            d="M10.5 3.5v-1a1 1 0 0 0-1-1h-6a1 1 0 0 0-1 1v6a1 1 0 0 0 1 1h1"
            fill="none"
            stroke="currentColor"
            strokeWidth="1.4"
          />
        </svg>
      )}
      {copied ? "Copied" : label}
    </button>
  );
}
