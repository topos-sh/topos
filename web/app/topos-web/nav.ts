/**
 * The nav-slot registry — the second composition seam. The shell sidebar renders exactly the
 * entries the composition provides; a downstream superset build APPENDS its own entries and
 * sections without touching the shell. Entries are plain data: no
 * component types cross this seam (icons are named, resolved by the shell's own icon map).
 */
export interface NavContext {
  /** The active workspace id, when the current page is workspace-scoped. */
  workspaceId: string | null;
  /** The signed-in actor's normalized email (sessionless pages render no nav). */
  email: string;
}

export interface NavEntry {
  /** Stable id — the collision guard's unit, and the shell's React key. */
  id: string;
  label: string;
  /** Builds the href; entries returning null for a context are not rendered. */
  href: (ctx: NavContext) => string | null;
  /** Name in the shell's icon map (lucide name); unknown names render no icon. */
  icon?: string;
  /** Display group. OSS ships `workspace` and `account`; a superset may add its own. */
  section: string;
}

/** The OSS app's own entries — the base every composition starts from. */
export const ossNav: NavEntry[] = [
  {
    id: "workspaces",
    label: "Workspaces",
    href: () => "/workspaces",
    icon: "layers",
    section: "workspace",
  },
  {
    id: "workspace-settings",
    label: "Settings",
    href: (ctx) => (ctx.workspaceId ? `/workspaces/${ctx.workspaceId}/settings` : null),
    icon: "settings",
    section: "workspace",
  },
];
