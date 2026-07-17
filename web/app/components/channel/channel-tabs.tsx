import type { ReactNode } from "react";
import { Link } from "react-router";

type ActiveTab = "skills" | "members" | "history" | "settings";

/**
 * The channel's section switcher — Skills / Members / History / Settings as PURE LINKS (no client
 * state), mirroring the skill view's SkillTabs. Each tab is a real route, so the active tab is
 * decided by whichever page renders this bar, every tab is a shareable URL, and switching is an
 * ordinary navigation with blocking SSR. Skills is the channel FACE itself (`basePath`); Members,
 * History, and Settings are member-only sub-routes. The active tab reads pressed — ink text under a
 * 2px accent underline; the rest stay quiet (dim) until hovered. Both variants carry a `border-b-2`
 * (accent vs transparent) so the row height never shifts as the active tab moves. `basePath` is the
 * name-keyed channel URL the caller built through `useWsPath` (origin-rooted in single tenancy,
 * `/<slug>`-nested in multi).
 */
export function ChannelTabs({ basePath, active }: { basePath: string; active: ActiveTab }) {
  return (
    <nav aria-label="Channel sections" className="flex border-line-soft border-b">
      <Tab to={basePath} isActive={active === "skills"}>
        Skills
      </Tab>
      <Tab to={`${basePath}/members`} isActive={active === "members"}>
        Members
      </Tab>
      <Tab to={`${basePath}/history`} isActive={active === "history"}>
        History
      </Tab>
      <Tab to={`${basePath}/settings`} isActive={active === "settings"}>
        Settings
      </Tab>
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
