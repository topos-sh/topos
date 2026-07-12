import type { ReactNode } from "react";

/**
 * The shared primitives — deliberately tiny. The Klein system (see ../../DESIGN.md): warm-gray
 * surfaces on a print ground, ink text, International Klein Blue as the one accent for
 * actions/focus. Semantic green/amber/gray are reserved for domain-verification states (see
 * DomainBadge) so the trust signal stays unambiguous.
 */

export function Card({ children, className = "" }: { children: ReactNode; className?: string }) {
  return (
    <div className={`rounded-lg border border-line-soft bg-panel ${className}`}>{children}</div>
  );
}

export function Chip({
  tone = "neutral",
  children,
}: {
  tone?: "neutral" | "accent" | "verified" | "pending" | "unverified";
  children: ReactNode;
}) {
  const tones: Record<string, string> = {
    neutral: "bg-panel2 text-dim",
    accent: "bg-accent-wash text-accent-deep",
    verified: "bg-green-50 text-green-800",
    pending: "bg-amber-50 text-amber-800",
    unverified: "bg-panel2 text-dim",
  };
  return (
    <span
      className={`inline-flex items-center gap-1 rounded-full px-2 py-0.5 font-medium text-xs ${tones[tone]}`}
    >
      {children}
    </span>
  );
}

/** A short identifier (version hash, fingerprint) in mono, self-labeling as truncated. */
export function ShortId({ value, length = 12 }: { value: string; length?: number }) {
  return (
    <code className="rounded bg-panel2 px-1.5 py-0.5 font-mono text-xs text-dim">
      {value.slice(0, length)}
    </code>
  );
}

/** The section-label voice: Martian Mono micro-label, uppercase, faint. */
export function SectionHeading({ children }: { children: ReactNode }) {
  return (
    <h2 className="font-display text-[10px] uppercase tracking-[0.12em] text-faint">{children}</h2>
  );
}

/**
 * The content-pane header — the "channel header" for a page in the right pane: a display title,
 * an optional quiet meta line (id / counts / topic), and optional right-aligned actions, closed by
 * a hairline that separates the header from the page body. Title/meta are ReactNode so a page can
 * compose a `#` channel prefix or a mono id inline.
 */
export function PageHeader({
  title,
  meta,
  actions,
}: {
  title: ReactNode;
  meta?: ReactNode;
  actions?: ReactNode;
}) {
  return (
    <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-2 border-line-soft border-b pb-4">
      <div className="min-w-0">
        <h1 className="truncate font-display font-semibold text-ink text-lg tracking-[-0.02em]">
          {title}
        </h1>
        {meta !== undefined && <div className="mt-1 text-faint text-xs">{meta}</div>}
      </div>
      {actions !== undefined && <div className="flex shrink-0 items-center gap-2">{actions}</div>}
    </div>
  );
}

/** The one button voice — mono labels. `kind="primary"` is the Klein-blue action; `kind="quiet"` is a bordered row action. */
export function buttonClasses(kind: "primary" | "quiet" | "danger" = "quiet"): string {
  const base =
    "inline-flex min-h-9 items-center justify-center gap-1.5 rounded-md px-3 font-mono text-[13px] " +
    "focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 " +
    "disabled:cursor-not-allowed disabled:opacity-50";
  if (kind === "primary") {
    return `${base} bg-accent text-on-accent transition-colors hover:bg-accent-deep active:scale-[0.98]`;
  }
  if (kind === "danger") {
    return `${base} border border-line text-red-700 hover:bg-red-50`;
  }
  return `${base} border border-line text-dim transition-colors hover:bg-panel2`;
}
