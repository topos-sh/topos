import { Plug } from "lucide-react";
import { mcpBrandMark } from "@/lib/mcp/brand-marks";
import { cn } from "@/lib/utils";

/**
 * THE MARK BESIDE A SERVER'S NAME — the brand's own logo where the vendored set has one
 * (app/lib/mcp/brand-marks.ts), and the app's MCP glyph where it does not.
 *
 * Decoration, strictly: the row's title is the name of the thing and the accessible label, so the
 * mark is `aria-hidden` and adds nothing a screen reader has to hear twice. What it buys is scan
 * speed — a picker is a shelf, and a shelf is read by silhouette before it is read by word.
 *
 * THE FALLBACK IS THE `Plug`, not a monogram or an empty square. Three reasons, in order: it is
 * already this app's word for "MCP server" (the sidebar and the channel curation list draw the
 * same glyph), so a markless row says what it IS rather than admitting what it lacks; a monogram
 * would be a logo this project drew for a brand that did not ask for one; and an empty slot would
 * read as a failed image. It sits a step quieter (`faint` against the marks' `dim`) so the eye
 * skips it on the way to a brand it recognizes — which is exactly the behaviour a picker wants.
 *
 * ONE SIZE by default (`size-5`) for both arms: the grid's rhythm comes from every row starting at
 * the same x, and a fallback that measured differently would break the column before it said
 * anything useful.
 */
export function McpMark({ logo, className }: { logo?: string; className?: string }) {
  const mark = mcpBrandMark(logo);
  if (mark === undefined) {
    return <Plug aria-hidden="true" className={cn("size-4 shrink-0 text-faint", className)} />;
  }
  return (
    <svg
      viewBox="0 0 24 24"
      // The vendored paths carry no fill of their own, so the mark takes the colour of the text it
      // sits beside and steps with the row like any other glyph.
      fill="currentColor"
      aria-hidden="true"
      focusable="false"
      className={cn("size-4 shrink-0 text-dim", className)}
    >
      <path d={mark.path} />
    </svg>
  );
}
