import { Link } from "react-router";
import { ShortId } from "@/components/ui";

const CRUMB_LINK =
  "text-dim underline decoration-hairline transition-colors hover:text-ink " +
  "focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2";

/**
 * The file view's location line, all mono: skill → version (short id, linking back to the file
 * list) → the path, its parent segments faint and the file name in ink. Segments are display
 * text nodes only — the path itself is never a link target here, only a label. `skill` is the
 * catalog NAME (the URL key).
 */
export function PathBreadcrumb({
  ws,
  skill,
  versionId,
  segments,
}: {
  ws: string;
  skill: string;
  versionId: string;
  segments: readonly string[];
}) {
  const skillHref = `/workspaces/${ws}/skills/${skill}`;
  const listingHref = `/workspaces/${ws}/skills/${skill}/versions/${versionId}`;
  return (
    <nav
      aria-label="Breadcrumb"
      className="flex flex-wrap items-center gap-x-1.5 gap-y-1 font-mono text-[13px]"
    >
      <Link to={skillHref} className={CRUMB_LINK}>
        {skill}
      </Link>
      <span className="text-faint">/</span>
      <Link
        to={listingHref}
        className="rounded focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
      >
        <ShortId value={versionId} />
      </Link>
      {segments.map((segment, i) => {
        const isLast = i === segments.length - 1;
        // A cumulative-path key stays unique even when two sibling segments share a name.
        const key = segments.slice(0, i + 1).join("/");
        return (
          <span key={key} className="flex items-center gap-x-1.5">
            <span className="text-faint">/</span>
            <span className={isLast ? "text-ink" : "text-faint"}>{segment}</span>
          </span>
        );
      })}
    </nav>
  );
}
