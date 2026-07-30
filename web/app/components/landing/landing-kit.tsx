import type { ReactNode } from "react";

/**
 * The three primitives every landing band shares — kept in their own module because the page
 * spine and the loop bands both set them, and neither should import the other.
 */

/** The page's one content measure: 1080px, 24px gutters (DESIGN.md's layout rule). */
export const WRAP = "mx-auto max-w-[1080px] px-6";

/**
 * The 10px uppercase Martian-Mono label that opens a band — the system's only uppercase voice,
 * at the label weight the design system specifies (500; a 400 label reads thin at this size).
 */
export function MicroLabel({ children }: { children: ReactNode }) {
  return (
    <p className="font-display font-medium text-[10px] text-faint uppercase tracking-[0.12em]">
      {children}
    </p>
  );
}

/** A band's sentence-case h2. `className` widens the measure for the longer headlines. */
export function SectionHeading({
  children,
  className = "max-w-[40ch]",
}: {
  children: ReactNode;
  className?: string;
}) {
  return (
    <h2
      className={`mt-3 font-display font-semibold text-[clamp(18px,2.2vw,23px)] text-ink leading-[1.45] tracking-[-0.02em] ${className}`}
    >
      {children}
    </h2>
  );
}
