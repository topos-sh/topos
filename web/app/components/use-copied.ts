import { useCallback, useEffect, useRef, useState } from "react";

/**
 * The one copy-to-clipboard state machine — both copy affordances (the glass CopyButton and
 * the light CopyCommand) share it, so guard, reset timing, and cleanup can never drift apart.
 * The write is guarded (`navigator.clipboard` is absent on insecure contexts) and a denied or
 * failed write is swallowed with the state untouched: the command text is always selectable
 * right next to the button, which is the documented fallback.
 */
export function useCopied(resetMs = 1500): { copied: boolean; copy: (text: string) => void } {
  const [copied, setCopied] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

  useEffect(() => {
    return () => {
      if (timer.current !== undefined) {
        clearTimeout(timer.current);
      }
    };
  }, []);

  const copy = useCallback(
    (text: string) => {
      if (!navigator.clipboard) {
        return;
      }
      navigator.clipboard
        .writeText(text)
        .then(() => {
          setCopied(true);
          if (timer.current !== undefined) {
            clearTimeout(timer.current);
          }
          timer.current = setTimeout(() => setCopied(false), resetMs);
        })
        .catch(() => {
          // Clipboard denied/failed: leave the button as-is — the text beside it is selectable.
        });
    },
    [resetMs],
  );

  return { copied, copy };
}
