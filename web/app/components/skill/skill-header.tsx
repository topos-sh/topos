import { Breadcrumbs } from "@/components/shell/breadcrumbs";
import { kindEntry } from "@/lib/bundle-base";

/**
 * The shared title block every skill tab page renders (Current / Proposals / History): the
 * display-font name over the mono locator line. The title is the catalog's `displayName` when the
 * catalog recorded one (an advisory folder name), falling back to the catalog NAME; the mono
 * locator keeps the name — it's the URL path. Pure presentation — the caller has already guarded
 * and probed. `currentShort` is the current version's 12-char short hash sliced from the catalog
 * row; it is ABSENT when the catalog entry has no current pointer yet (nothing published), which
 * the locator states honestly instead of assuming a version.
 *
 * A kind with no file bytes has no pointer to report and says nothing here: what a connected
 * server delivers is on the page below, and "nothing published yet" would be a claim about a
 * version history it does not have.
 */
export function SkillHeader({
  ws,
  skill,
  currentShort,
  displayName,
  kind,
}: {
  ws: string;
  /** The catalog NAME — the URL key and the honest title fallback. */
  skill: string;
  /** The current version's short hash, or absent/empty while nothing is published. */
  currentShort?: string | null;
  displayName?: string | null;
  /** The bundle kind — `"skill"` today; display metadata only, absent on a pre-kind read. */
  kind?: string | null;
}) {
  return (
    <div>
      <h1 className="font-display font-semibold text-lg tracking-[-0.02em] text-ink">
        {displayName ?? skill}
      </h1>
      <Breadcrumbs className="mt-1" />
      <p className="mt-0.5 font-mono text-xs text-faint">
        {ws} / {skill}
        {kind ? ` · ${kind}` : ""}
        {/* A kind with no files has no commit to name — neither a version nor an absence of
            one is a fact about it, and the page below says what it does deliver. */}
        {!kindEntry(kind).isFileBundle
          ? ""
          : currentShort
            ? ` · current ${currentShort}`
            : " · nothing published yet"}
      </p>
    </div>
  );
}
