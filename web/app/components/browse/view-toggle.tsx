import { Link } from "react-router";
import { buttonClasses } from "@/components/ui";

/**
 * Rendered | Raw for a file that has a rendered form. Two quiet-button links; the active one
 * reads pressed (ink on panel2). Raw appends `?view=raw`, Rendered drops the query — Rendered is
 * the default (no query at all). The active variant is spelled out rather than layered on top of
 * the quiet class: `buttonClasses("quiet")` already sets `text-dim`, and two same-property color
 * utilities on one element resolve by stylesheet source order, not class order — not a guarantee
 * to lean on.
 */
const QUIET = buttonClasses("quiet");
const ACTIVE =
  "inline-flex min-h-9 items-center justify-center gap-1.5 rounded-md border border-line " +
  "bg-panel2 px-3 font-mono text-[13px] text-ink focus-visible:outline-2 " +
  "focus-visible:outline-accent focus-visible:outline-offset-2";

export function ViewToggle({ basePath, raw }: { basePath: string; raw: boolean }) {
  return (
    <nav className="inline-flex items-center gap-1.5" aria-label="View mode">
      <Link to={basePath} aria-current={raw ? undefined : "true"} className={raw ? QUIET : ACTIVE}>
        Rendered
      </Link>
      <Link
        to={`${basePath}?view=raw`}
        aria-current={raw ? "true" : undefined}
        className={raw ? ACTIVE : QUIET}
      >
        Raw
      </Link>
    </nav>
  );
}
