import type { ReactNode } from "react";
import { Link } from "react-router";

type ActiveTab = "current" | "proposals" | "history" | "settings";

/**
 * The skill's section switcher — Current / Proposals / History (+ Settings for owners) as PURE
 * LINKS (no client state).
 * Each tab is a real route, so the active tab is decided by whichever page renders this bar, every
 * tab is a shareable URL, and switching is an ordinary navigation with blocking SSR. The active tab
 * reads pressed — ink text under a 2px accent underline; the rest stay quiet (dim) until hovered.
 * Both variants carry a `border-b-2` (accent vs transparent) so the row height never shifts as
 * the active tab moves. `openProposals` decorates the Proposals label with a small count.
 * `basePath` is the catalog-name-keyed skill URL the caller built through `useWsPath` (origin-rooted
 * in single tenancy, `/<slug>`-nested in multi). `showSettings` renders the owner-only Settings
 * tab — the caller passes its loader's own owner fact; the settings ROUTE re-guards regardless
 * (the tab is discoverability, never the gate).
 */
export function SkillTabs({
  basePath,
  active,
  openProposals = 0,
  showSettings = false,
}: {
  basePath: string;
  active: ActiveTab;
  openProposals?: number;
  showSettings?: boolean;
}) {
  return (
    <nav aria-label="Skill sections" className="flex border-line-soft border-b">
      <Tab to={basePath} isActive={active === "current"}>
        Current
      </Tab>
      <Tab to={`${basePath}/proposals`} isActive={active === "proposals"}>
        Proposals
        {openProposals > 0 && (
          <span className="ml-1.5 inline-flex items-center rounded-full bg-accent-wash px-1.5 text-accent-deep text-xs">
            {openProposals}
          </span>
        )}
      </Tab>
      <Tab to={`${basePath}/history`} isActive={active === "history"}>
        History
      </Tab>
      {showSettings && (
        <Tab to={`${basePath}/settings`} isActive={active === "settings"}>
          Settings
        </Tab>
      )}
    </nav>
  );
}

function Tab({ to, isActive, children }: { to: string; isActive: boolean; children: ReactNode }) {
  // A constant `border-b-2` on both variants keeps active/inactive the same height; only its
  // color (accent vs transparent) changes. `-mb-px` pulls the tab down one pixel so its bottom
  // border lands on the nav's own border-b instead of stacking above it.
  const base =
    "-mb-px inline-flex min-h-9 items-center border-b-2 px-3 font-mono text-[13px] " +
    "focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2";
  const tone = isActive
    ? "border-accent text-ink"
    : "border-transparent text-dim transition-colors hover:text-ink";
  return (
    <Link to={to} aria-current={isActive ? "page" : undefined} className={`${base} ${tone}`}>
      {children}
    </Link>
  );
}
