/**
 * The nav-slot registry — the second composition seam. The shell sidebar renders exactly the
 * entries the composition provides; a downstream superset build APPENDS its own entries and
 * sections without touching the shell. Entries are plain data: no
 * component types cross this seam (icons are named, resolved by the shell's own icon map).
 */
export interface NavContext {
  /**
   * The active workspace's URL base, when the current page is workspace-scoped: `""` for a
   * single-tenant in-workspace page (origin-rooted), `"/<slug>"` in multi, and null when no
   * workspace is in scope (a workspace-scoped entry returns null and is dropped). An entry builds
   * its href as `${wsBase}/sub`, which stays correct in both grammars.
   */
  wsBase: string | null;
  /** The active workspace id, when workspace-scoped — kept for a downstream composition's use. */
  workspaceId: string | null;
  /**
   * The signed-in actor's display identity (sessionless pages render no nav). The field keeps
   * its historical name — it is a seam a downstream composition already reads.
   */
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
    id: "workspace-members",
    label: "Members",
    href: (ctx) => (ctx.wsBase === null ? null : `${ctx.wsBase}/members`),
    icon: "users",
    section: "workspace",
  },
  {
    id: "workspace-settings",
    label: "Settings",
    href: (ctx) => (ctx.wsBase === null ? null : `${ctx.wsBase}/settings`),
    icon: "settings",
    section: "workspace",
  },
  {
    id: "your-sessions",
    label: "Your sessions",
    href: () => "/account/sessions",
    icon: "laptop",
    section: "account",
  },
];
