import type { ReactNode } from "react";
import { Card } from "@/components/ui";

/**
 * The browse pages' outer stack. The shell layout already provides the single <main> wrap, so these
 * pages contribute only a vertical rhythm — no nested landmark.
 */
export function BrowseShell({ children }: { children: ReactNode }) {
  return <div className="space-y-6">{children}</div>;
}

/**
 * The honest empty/failure state, mirroring the review page's EmptyState card: a plain statement
 * of what isn't there, never an access claim (the server answers 404 for missing and unauthorized
 * alike, and this tier must not distort that).
 */
export function BrowseEmpty({ heading, children }: { heading: string; children: ReactNode }) {
  return (
    <Card className="flex flex-col gap-2 p-6">
      <h1 className="font-display font-semibold text-lg text-ink tracking-[-0.02em]">{heading}</h1>
      <p className="text-dim text-sm">{children}</p>
    </Card>
  );
}
