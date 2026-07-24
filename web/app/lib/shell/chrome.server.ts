import { composition } from "@/composition.server";
import { requireMember, type UserActor } from "@/lib/auth/guards.server";
import { channelsOf } from "@/lib/db/queries.channels.server";
import { membershipsFor, skillIndexOf, type WorkspaceMembership } from "@/lib/db/queries.server";
import { destinationPathname } from "@/lib/destination-path";
import { workspaceAddress } from "@/lib/ws-url.server";
import type { NavContext } from "@/topos-web/nav";

/**
 * The signed-in chrome data shared by BOTH layouts — the login-bounce `shell.tsx` and the
 * anonymous-tolerant `face-shell.tsx` — so the rail, the nav registry, and the sidebar's collapse
 * state can never drift between them. The active workspace is derived from the URL under the
 * deployment's tenancy grammar (single → the sole seat; multi → the seat whose address matches the
 * first path segment, else the last-active fallback below), never by probing the DB with an
 * arbitrary segment.
 */

/** One nav slot with its href already resolved for the request (the shell renders these verbatim). */
export interface ResolvedNavEntry {
  id: string;
  label: string;
  icon?: string;
  section: string;
  href: string;
}

/** One catalog entry the sidebar's Skills section renders — the name it routes on + its label. */
export interface SidebarSkill {
  /** The catalog NAME the skill face routes on (`skills/<name>`). */
  name: string;
  /** The advisory display label (the author's folder name, else the name). */
  label: string;
}

/** One channel the sidebar's Channels section renders. */
export interface SidebarChannel {
  name: string;
  /** The implicit `everyone` channel — surfaced first, as the DAL returns it. */
  isDefault: boolean;
}

/**
 * The active workspace's sidebar context — present only when a workspace is in scope (a signed-in
 * member on a workspace-scoped page); null off-workspace (a signed-in non-member, an anonymous
 * face is handled a layer up). The address grammar is resolved server-side so the sidebar never
 * computes it.
 */
export interface SidebarWorkspace {
  id: string;
  /** The workspace's display name — the sidebar's static identity in single tenancy. */
  displayName: string;
  /** The address slug — the `:ws` segment the sidebar nests its links under in multi tenancy. */
  address: string;
  /** The FULL shareable address the publish dialog composes its `topos login` line from
   *  (single → the bare origin, multi → `<origin>/<name>`). */
  shareAddress: string;
  skills: SidebarSkill[];
  channels: SidebarChannel[];
}

export interface ChromeData {
  display: string;
  memberships: WorkspaceMembership[];
  nav: ResolvedNavEntry[];
  sidebarOpen: boolean;
  /** The address grammar the sidebar builds its membership links under. */
  tenancy: "single" | "multi";
  /** The active workspace's Skills + Channels sections + identity — null when off-workspace. */
  workspace: SidebarWorkspace | null;
}

/**
 * The active workspace SEAT for the request's destination path (`destinationPathname` — the
 * raw pathname of a client-side arrival reads `/acme.data`, not `/acme`): single → the one
 * seat (the URL carries no segment); multi → the seat whose `address` equals the first path
 * segment (memberships already carry the slug, so no DB probe on an arbitrary segment).
 * A multi-tenancy URL miss — a person-scoped page like /account/devices — falls back to the
 * seat the sidebar remembered in the `topos_active_ws` cookie, else the first seat, so leaving
 * the workspace URL space keeps the panel as it was instead of blanking it. The fallback only
 * ever SELECTS from `memberships` (rows this request already proved), so a stale cookie naming
 * a workspace the person no longer holds a seat in can never steer `loadChrome`'s
 * `requireMember` — it just misses and the first seat wins. Returns null only with no seats at
 * all. Exported for the regression test — a `.data` loader URL must resolve the same seat its
 * destination does, or every client-side navigation into a workspace dashboard strips the
 * panel down to logo + account.
 */
export function activeMembership(
  request: Request,
  memberships: WorkspaceMembership[],
): WorkspaceMembership | null {
  if (composition.tenancy === "single") {
    return memberships[0] ?? null;
  }
  const first = destinationPathname(request).split("/")[1] ?? "";
  const bySegment = memberships.find((m) => m.address === first);
  if (bySegment !== undefined) {
    return bySegment;
  }
  const cookie = request.headers.get("cookie") ?? "";
  const remembered = /(?:^|;\s*)topos_active_ws=([^;\s]+)/.exec(cookie)?.[1];
  return memberships.find((m) => m.id === remembered) ?? memberships[0] ?? null;
}

/** Load the rail memberships, resolve every nav slot's href for the active workspace, fetch the
 * active workspace's Skills + Channels lists, read the persisted collapse state. One place, both
 * layouts. */
export async function loadChrome(request: Request, actor: UserActor): Promise<ChromeData> {
  const memberships = await membershipsFor(actor);
  const active = activeMembership(request, memberships);
  const wsBase =
    active === null ? null : composition.tenancy === "multi" ? `/${active.address}` : "";
  const ctx: NavContext = {
    wsBase,
    workspaceId: active?.id ?? null,
    email: actor.display,
  };
  const nav = composition.nav.flatMap<ResolvedNavEntry>((entry) => {
    const href = entry.href(ctx);
    return href === null
      ? []
      : [{ id: entry.id, label: entry.label, icon: entry.icon, section: entry.section, href }];
  });

  // The workspace-scoped sections load only when a workspace is in scope. `requireMember` re-derives
  // the seat from the roster per request (the sanctioned MemberActor mint), so the DAL reads run
  // under a proof of admission — never off a cached membership row.
  let workspace: SidebarWorkspace | null = null;
  if (active !== null) {
    const member = await requireMember(request, active.id);
    const [skills, channels] = await Promise.all([
      skillIndexOf(member, active.id),
      channelsOf(member),
    ]);
    workspace = {
      id: active.id,
      displayName: active.displayName,
      address: active.address,
      shareAddress: workspaceAddress(request, active.address),
      skills: skills.map((s) => ({ name: s.name, label: s.displayName ?? s.name })),
      channels: channels.map((c) => ({ name: c.name, isDefault: c.isDefault })),
    };
  }

  // Honor the rail's persisted collapsed/expanded choice server-side so the first paint matches it.
  const cookie = request.headers.get("cookie") ?? "";
  const sidebarOpen = !/(?:^|;\s*)sidebar_state=false(?:;|$)/.test(cookie);
  return {
    display: actor.display,
    memberships,
    nav,
    sidebarOpen,
    tenancy: composition.tenancy,
    workspace,
  };
}
