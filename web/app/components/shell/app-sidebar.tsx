import { useQuery } from "@tanstack/react-query";
import {
  Check,
  ChevronsUpDown,
  Hash,
  Laptop,
  Layers,
  LogOut,
  MonitorSmartphone,
  Package,
  Plus,
  Settings,
  UserRound,
  Users,
} from "lucide-react";
import type { ElementType } from "react";
import { Link, useLocation } from "react-router";
import { PublishDialog } from "@/components/shell/publish-dialog";
import {
  DropdownMenu,
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuLabel,
  DropdownMenuSeparator,
  DropdownMenuTrigger,
} from "@/components/ui/dropdown-menu";
import {
  Sidebar,
  SidebarContent,
  SidebarFooter,
  SidebarGroup,
  SidebarGroupAction,
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarRail,
  SidebarTrigger,
} from "@/components/ui/sidebar";
import { authClient } from "@/lib/auth/client";
import { fetchMemberships, membershipsQueryKey } from "@/lib/query/memberships";
import type { SidebarWorkspace } from "@/lib/shell/chrome.server";
import { wsHref } from "@/lib/ws-path";

/**
 * One nav-registry entry as the shell renders it: NavEntry-shaped plain data whose `href` the
 * shell route's loader already RESOLVED against the request's context (so no function crosses the
 * loader's serialization boundary, and null-href entries are already dropped), and whose `icon` is
 * a NAME resolved here through the icon map below. A downstream superset build appends its own
 * entries to this list; the shell renders whatever it's handed.
 */
export interface SidebarNavEntry {
  id: string;
  label: string;
  /** The resolved href (non-null — the loader drops entries that don't apply to the context). */
  href: string;
  /** Icon name in the map below; an unknown name renders no icon. */
  icon?: string;
  section: string;
}

/** The shell's own icon map — the one place a nav `icon` NAME becomes a component. */
const NAV_ICONS: Record<string, ElementType> = {
  layers: Layers,
  settings: Settings,
  hash: Hash,
  users: Users,
  monitor: MonitorSmartphone,
  laptop: Laptop,
};

/**
 * The signed-in shell's left panel, on shadcn's collapsible Sidebar. Top to bottom: the `topos_`
 * wordmark beside the ONE collapse toggle (both live in the header strip, so the toggle stays
 * reachable in the icon-collapsed state); the workspace identity (static in single tenancy, a seat
 * DROPDOWN in multi); the workspace's Skills and Channels, each with a section-header `+ new`; the
 * workspace nav (Members · Settings) as plain items at the bottom; and the account menu in the
 * footer. The Skills/Channels/nav sections render only when a workspace is in scope — every list is
 * loader-derived (`workspace`), passed once as a prop; the seat DROPDOWN reads the React Query the
 * shell route's loader seeded (so first paint has data, and a membership change updates it live).
 * The account menu renders the resolved nav registry's non-`workspace` sections, so a downstream
 * build's appended entries appear there with no shell change.
 */
export function AppSidebar({
  display,
  nav,
  tenancy,
  workspace,
}: {
  display: string;
  nav: readonly SidebarNavEntry[];
  tenancy: "single" | "multi";
  workspace: SidebarWorkspace | null;
}) {
  const location = useLocation();
  const { data: memberships = [] } = useQuery({
    queryKey: membershipsQueryKey,
    queryFn: fetchMemberships,
  });

  // The active workspace's URL segment (null → origin-rooted single-tenant links; the slug in
  // multi). Built once here, never in a child: the panel lives in the LAYOUT, so it can't read the
  // child route's `:ws` param — the loader hands it the address instead.
  const wsSegment = workspace === null ? null : tenancy === "multi" ? workspace.address : null;
  const rootHref = workspace === null ? "/app" : wsHref(wsSegment);

  // The nav registry splits by section: `workspace` items (Members · Settings) render as plain
  // bottom items; every other section (account + a superset's own) stays in the account menu.
  const workspaceNav = nav.filter((e) => e.section === "workspace");
  const accountNav = nav.filter((e) => e.section !== "workspace");

  return (
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <div className="flex h-8 items-center justify-between gap-2 px-1">
          <Link
            to={rootHref}
            className="font-display font-semibold text-ink text-sm tracking-[-0.02em] focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 group-data-[collapsible=icon]:hidden"
          >
            topos<span className="text-accent">_</span>
          </Link>
          <SidebarTrigger className="text-dim hover:text-ink" />
        </div>
      </SidebarHeader>

      <SidebarContent>
        {workspace !== null && (
          <>
            <WorkspaceIdentity tenancy={tenancy} workspace={workspace} memberships={memberships} />
            <SkillsSection
              workspace={workspace}
              wsSegment={wsSegment}
              pathname={location.pathname}
            />
            <ChannelsSection
              workspace={workspace}
              wsSegment={wsSegment}
              pathname={location.pathname}
            />
            {workspaceNav.length > 0 && (
              <SidebarGroup className="mt-auto">
                <SidebarMenu>
                  {workspaceNav.map((entry) => {
                    const Icon = entry.icon !== undefined ? NAV_ICONS[entry.icon] : undefined;
                    return (
                      <SidebarMenuItem key={entry.id}>
                        <SidebarMenuButton
                          asChild
                          tooltip={entry.label}
                          isActive={isActivePath(location.pathname, entry.href)}
                          className="data-[active=true]:bg-accent-wash! data-[active=true]:text-ink!"
                        >
                          <Link to={entry.href}>
                            {Icon !== undefined ? <Icon /> : null}
                            <span>{entry.label}</span>
                          </Link>
                        </SidebarMenuButton>
                      </SidebarMenuItem>
                    );
                  })}
                </SidebarMenu>
              </SidebarGroup>
            )}
          </>
        )}
      </SidebarContent>

      <SidebarFooter>
        <SidebarMenu>
          <SidebarMenuItem>
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <SidebarMenuButton
                  size="lg"
                  tooltip={display}
                  className="group-data-[collapsible=icon]:justify-center! data-[state=open]:bg-sidebar-accent"
                >
                  <UserRound />
                  <span className="truncate group-data-[collapsible=icon]:hidden">{display}</span>
                  <ChevronsUpDown className="ml-auto group-data-[collapsible=icon]:hidden" />
                </SidebarMenuButton>
              </DropdownMenuTrigger>
              <DropdownMenuContent side="top" align="start" className="min-w-56">
                <DropdownMenuLabel className="truncate font-normal text-faint text-xs">
                  {display}
                </DropdownMenuLabel>
                {accountNav.length > 0 && <DropdownMenuSeparator />}
                {accountNav.map((entry) => {
                  const Icon = entry.icon !== undefined ? NAV_ICONS[entry.icon] : undefined;
                  return (
                    <DropdownMenuItem key={entry.id} asChild>
                      <Link to={entry.href}>
                        {Icon !== undefined ? <Icon /> : null}
                        {entry.label}
                      </Link>
                    </DropdownMenuItem>
                  );
                })}
                <DropdownMenuSeparator />
                <DropdownMenuItem onSelect={signOut}>
                  <LogOut />
                  Sign out
                </DropdownMenuItem>
              </DropdownMenuContent>
            </DropdownMenu>
          </SidebarMenuItem>
        </SidebarMenu>
      </SidebarFooter>

      <SidebarRail />
    </Sidebar>
  );
}

/**
 * The workspace identity, item 2 of the panel. Single tenancy — the install IS its one workspace —
 * shows the display name as STATIC text (no menu; there is nowhere to switch to). Multi shows a
 * DROPDOWN of the person's seats, each navigating to that workspace's root, the active one ticked.
 */
function WorkspaceIdentity({
  tenancy,
  workspace,
  memberships,
}: {
  tenancy: "single" | "multi";
  workspace: SidebarWorkspace;
  memberships: { id: string; displayName: string; address: string }[];
}) {
  if (tenancy === "single") {
    return (
      <SidebarGroup className="pb-0">
        <div className="flex h-9 items-center gap-2 rounded-md px-2 group-data-[collapsible=icon]:justify-center group-data-[collapsible=icon]:px-0">
          <WorkspaceGlyph name={workspace.displayName} />
          <span className="min-w-0 truncate font-medium text-ink text-sm group-data-[collapsible=icon]:hidden">
            {workspace.displayName}
          </span>
        </div>
      </SidebarGroup>
    );
  }
  return (
    <SidebarGroup className="pb-0">
      <SidebarMenu>
        <SidebarMenuItem>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <SidebarMenuButton
                tooltip={workspace.displayName}
                className="data-[state=open]:bg-sidebar-accent"
              >
                <WorkspaceGlyph name={workspace.displayName} />
                <span className="min-w-0 truncate font-medium text-ink group-data-[collapsible=icon]:hidden">
                  {workspace.displayName}
                </span>
                <ChevronsUpDown className="ml-auto group-data-[collapsible=icon]:hidden" />
              </SidebarMenuButton>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="start" className="min-w-56">
              <DropdownMenuLabel className="font-normal text-faint text-xs">
                Your workspaces
              </DropdownMenuLabel>
              {memberships.map((m) => (
                <DropdownMenuItem key={m.id} asChild>
                  <Link to={wsHref(m.address)}>
                    <WorkspaceGlyph name={m.displayName} />
                    <span className="min-w-0 flex-1 truncate">{m.displayName}</span>
                    {m.id === workspace.id && <Check className="ml-auto size-4 text-accent" />}
                  </Link>
                </DropdownMenuItem>
              ))}
              {/* Multi-tenant only by construction — this dropdown renders nowhere else. */}
              <DropdownMenuSeparator />
              <DropdownMenuItem asChild>
                <Link to="/new">
                  <Plus />
                  New workspace
                </Link>
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </SidebarMenuItem>
      </SidebarMenu>
    </SidebarGroup>
  );
}

/** The workspace's catalog (active skills, name-sorted) with a `+ new` that opens the publish
 * dialog. Names only — the dashboard stays the fuller index. */
function SkillsSection({
  workspace,
  wsSegment,
  pathname,
}: {
  workspace: SidebarWorkspace;
  wsSegment: string | null;
  pathname: string;
}) {
  return (
    <SidebarGroup>
      <SidebarGroupLabel>Skills</SidebarGroupLabel>
      <PublishDialog
        shareAddress={workspace.shareAddress}
        trigger={
          <SidebarGroupAction aria-label="Publish a skill from your agent">
            <Plus />
          </SidebarGroupAction>
        }
      />
      <SidebarMenu>
        {workspace.skills.length === 0 ? (
          <li className="px-2 py-1 text-faint text-xs group-data-[collapsible=icon]:hidden">
            No skills yet.
          </li>
        ) : (
          workspace.skills.map((skill) => {
            const href = wsHref(wsSegment, `skills/${skill.name}`);
            return (
              <SidebarMenuItem key={skill.name}>
                <SidebarMenuButton
                  asChild
                  tooltip={skill.label}
                  isActive={isActivePath(pathname, href)}
                  className="data-[active=true]:bg-accent-wash! data-[active=true]:text-ink!"
                >
                  <Link to={href}>
                    <Package />
                    <span>{skill.label}</span>
                  </Link>
                </SidebarMenuButton>
              </SidebarMenuItem>
            );
          })
        )}
      </SidebarMenu>
    </SidebarGroup>
  );
}

/** The workspace's channels (`everyone` first, as the DAL returns) with a `+ new` linking to the
 * relocated create form. */
function ChannelsSection({
  workspace,
  wsSegment,
  pathname,
}: {
  workspace: SidebarWorkspace;
  wsSegment: string | null;
  pathname: string;
}) {
  return (
    <SidebarGroup>
      <SidebarGroupLabel>Channels</SidebarGroupLabel>
      <SidebarGroupAction asChild aria-label="New channel">
        <Link to={wsHref(wsSegment, "channels/new")}>
          <Plus />
        </Link>
      </SidebarGroupAction>
      <SidebarMenu>
        {workspace.channels.map((channel) => {
          const href = wsHref(wsSegment, `channels/${channel.name}`);
          return (
            <SidebarMenuItem key={channel.name}>
              <SidebarMenuButton
                asChild
                tooltip={channel.name}
                isActive={isActivePath(pathname, href)}
                className="data-[active=true]:bg-accent-wash! data-[active=true]:text-ink!"
              >
                <Link to={href}>
                  <Hash />
                  <span>{channel.name}</span>
                </Link>
              </SidebarMenuButton>
            </SidebarMenuItem>
          );
        })}
      </SidebarMenu>
    </SidebarGroup>
  );
}

/**
 * The workspace's leading glyph: a `#` when expanded, the workspace's initial when the rail is
 * icon-collapsed — so the icon rail stays distinguishable. Both share one 16px slot; the collapse
 * state (a `data-collapsible=icon` group attribute shadcn sets on an ancestor) swaps which shows.
 */
function WorkspaceGlyph({ name }: { name: string }) {
  const initial = name.trim()[0]?.toUpperCase() ?? "#";
  return (
    <span aria-hidden="true" className="grid size-4 shrink-0 place-items-center">
      <span className="col-start-1 row-start-1 text-faint group-data-[collapsible=icon]:hidden">
        #
      </span>
      <span className="col-start-1 row-start-1 hidden font-semibold text-[11px] leading-none group-data-[collapsible=icon]:block">
        {initial}
      </span>
    </span>
  );
}

/** A path is active when it IS the href or sits under it (so a skill stays lit on its subpages). */
function isActivePath(pathname: string, href: string): boolean {
  return pathname === href || pathname.startsWith(`${href}/`);
}

/** The smallest honest sign-out: the Better Auth client call, then a hard move to /login. */
async function signOut() {
  try {
    await authClient.signOut();
  } finally {
    window.location.href = "/login";
  }
}
