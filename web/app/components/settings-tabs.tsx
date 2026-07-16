import { Link } from "react-router";
import { cn } from "@/lib/utils";
import { useWsPath } from "@/lib/ws-path";

/**
 * The Settings section's tab header, shared VERBATIM by both settings route modules so the two
 * can't drift: General (the workspace policy page) and Devices (the read-only workspace fleet).
 * The URLs stay exactly as they are — `settings` and `settings/devices` — built through the tenancy
 * hook, so the tabs are origin-rooted in single and `/:ws`-nested in multi with no per-page
 * branching. The active tab carries `aria-current="page"`.
 */
export function SettingsTabs({ active }: { active: "general" | "devices" }) {
  const wsPath = useWsPath();
  const tabs = [
    { id: "general", label: "General", href: wsPath("settings") },
    { id: "devices", label: "Devices", href: wsPath("settings/devices") },
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
