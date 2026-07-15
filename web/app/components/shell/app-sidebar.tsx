import { useQuery } from "@tanstack/react-query";
import {
  ChevronsUpDown,
  Hash,
  Laptop,
  Layers,
  LogOut,
  MonitorSmartphone,
  Settings,
  UserRound,
  Users,
} from "lucide-react";
import type { ElementType } from "react";
import { Link, useLocation } from "react-router";
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
  SidebarGroupLabel,
  SidebarHeader,
  SidebarMenu,
  SidebarMenuButton,
  SidebarMenuItem,
  SidebarRail,
} from "@/components/ui/sidebar";
import { authClient } from "@/lib/auth/client";
import { fetchMemberships, membershipsQueryKey } from "@/lib/query/memberships";

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
 * The signed-in shell's rail — the workspace as a channel, on shadcn's collapsible Sidebar. The
 * seat list is a React Query the shell route's loader seeded into the cache (see providers.tsx),
 * so first paint has data (no loading flash); a membership change invalidates this same query and
 * the rail updates without a reload. Every seat admits (there is no half-membership), and this
 * OSS install serves ONE workspace — the rail carries no create action; a superset build appends
 * its own. `display`/`nav` are loader-derived, passed once as props — the account menu renders
 * the resolved nav registry, so a downstream build's appended entries appear there with no shell
 * change.
 */
export function AppSidebar({ display, nav }: { display: string; nav: readonly SidebarNavEntry[] }) {
  const location = useLocation();
  const activeWorkspaceId = workspaceFromPath(location.pathname);
  const { data: memberships = [] } = useQuery({
    queryKey: membershipsQueryKey,
    queryFn: fetchMemberships,
  });

  return (
    <Sidebar collapsible="icon">
      <SidebarHeader>
        <div className="flex h-8 items-center px-2">
          <Link
            to="/workspaces"
            className="font-display font-semibold text-ink text-sm tracking-[-0.02em] focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2 group-data-[collapsible=icon]:hidden"
          >
            topos<span className="text-accent">_</span>
          </Link>
        </div>
      </SidebarHeader>

      <SidebarContent>
        <SidebarGroup>
          <SidebarGroupLabel>Workspaces</SidebarGroupLabel>
          <SidebarMenu>
            {memberships.map((m) => {
              const active = m.id === activeWorkspaceId;
              return (
                <SidebarMenuItem key={m.id}>
                  <SidebarMenuButton
                    asChild
                    isActive={active}
                    tooltip={m.displayName}
                    className="data-[active=true]:bg-accent-wash! data-[active=true]:text-ink!"
                  >
                    <Link to={`/workspaces/${m.id}`}>
                      <ChannelGlyph name={m.displayName} />
                      <span className="group-data-[collapsible=icon]:hidden">{m.displayName}</span>
                    </Link>
                  </SidebarMenuButton>
                </SidebarMenuItem>
              );
            })}
          </SidebarMenu>
        </SidebarGroup>
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
                {nav.length > 0 && <DropdownMenuSeparator />}
                {nav.map((entry) => {
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
 * The channel's leading glyph: a `#` when the rail is expanded (the channel look), the workspace's
 * initial when collapsed — so the icon rail stays distinguishable instead of a column of identical
 * hashes. Both share one 16px slot; the collapse state (a `data-collapsible=icon` group attribute
 * shadcn sets on an ancestor) swaps which one shows, no JS.
 */
function ChannelGlyph({ name }: { name: string }) {
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

/** The workspace id in a `/workspaces/{id}…` path, or null (the home route). */
function workspaceFromPath(pathname: string): string | null {
  return pathname.match(/^\/workspaces\/([^/]+)/)?.[1] ?? null;
}

/** The smallest honest sign-out: the Better Auth client call, then a hard move to /login. */
async function signOut() {
  try {
    await authClient.signOut();
  } finally {
    window.location.href = "/login";
  }
}
