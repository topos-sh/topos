import { Link } from "react-router";
import { cn } from "@/lib/utils";
import { useWsPath } from "@/lib/ws-path";

/**
 * The Settings section's tab header, shared VERBATIM by the settings route modules so they
 * can't drift: General (the workspace policy page), Devices (the read-only workspace fleet), and
 * Archive (the archived-skills list with the delete ceremony). The URLs — `settings`,
 * `settings/sessions`, `settings/archive` — are built through the tenancy hook, so the tabs are
 * origin-rooted in single and `/:ws`-nested in multi with no per-page branching. The active tab
 * carries `aria-current="page"`.
 */
export function SettingsTabs({ active }: { active: "general" | "sessions" | "archive" }) {
  const wsPath = useWsPath();
  const tabs = [
    { id: "general", label: "General", href: wsPath("settings") },
    { id: "sessions", label: "Sessions", href: wsPath("settings/sessions") },
    { id: "archive", label: "Archive", href: wsPath("settings/archive") },
  ] as const;
  return (
    <nav aria-label="Settings sections" className="flex gap-1 border-line-soft border-b">
      {tabs.map((tab) => {
        const isActive = tab.id === active;
        return (
          <Link
            key={tab.id}
            to={tab.href}
            aria-current={isActive ? "page" : undefined}
            className={cn(
              "-mb-px border-b-2 px-3 py-2 font-mono text-[13px] transition-colors focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2",
              isActive ? "border-accent text-ink" : "border-transparent text-dim hover:text-ink",
            )}
          >
            {tab.label}
          </Link>
        );
      })}
    </nav>
  );
}
