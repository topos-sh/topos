import { useQuery } from "@tanstack/react-query";
import {
  ChevronsUpDown,
  Hash,
  Laptop,
  Layers,
  LogOut,
  MonitorSmartphone,
  Plus,
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
  SidebarMenuBadge,
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
 * its own entries to this list; the shell renders whatever it's handed.
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
 * The signed-in shell's rail — workspaces as channels, on shadcn's collapsible Sidebar. The
 * workspace list is a React Query the shell route's loader seeded into the cache (see providers.tsx),
 * so first paint has data (no loading flash); creating a workspace invalidates this same query and
 * the rail updates without a reload. `email`/`nav` are loader-derived, passed once as props — the
 * account menu renders the resolved nav registry, so a downstream build's appended entries
 * appear there with no shell change.
 */
export function AppSidebar({ email, nav }: { email: string; nav: readonly SidebarNavEntry[] }) {
  const location = useLocation();
  const activeWorkspaceId = workspaceFromPath(location.pathname);
  const { data: memberships = [] } = useQuery({
    queryKey: membershipsQueryKey,
    queryFn: fetchMemberships,
  });
  const channels = memberships.filter((m) => m.navigable);
  const invited = memberships.filter((m) => !m.navigable);

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
            {channels.map((m) => {
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
            <SidebarMenuItem>
              <SidebarMenuButton asChild tooltip="New workspace">
                <Link to="/workspaces/new">
                  <Plus />
                  <span className="group-data-[collapsible=icon]:hidden">New workspace</span>
                </Link>
              </SidebarMenuButton>
            </SidebarMenuItem>
          </SidebarMenu>
        </SidebarGroup>

        {invited.length > 0 && (
          <SidebarGroup>
            <SidebarGroupLabel>Invited</SidebarGroupLabel>
            <SidebarMenu>
              {invited.map((m) => (
                <SidebarMenuItem key={m.id}>
                  {/* Not navigable — an invited seat can't be entered until a device enrolls. */}
                  <SidebarMenuButton
                    tooltip={`${m.displayName} — connect an agent to join`}
                    className="cursor-default text-sidebar-foreground/60 hover:bg-transparent hover:text-sidebar-foreground/60"
                  >
                    <ChannelGlyph name={m.displayName} />
                    <span className="group-data-[collapsible=icon]:hidden">{m.displayName}</span>
                  </SidebarMenuButton>
                  <SidebarMenuBadge>invited</SidebarMenuBadge>
                </SidebarMenuItem>
              ))}
            </SidebarMenu>
          </SidebarGroup>
        )}
      </SidebarContent>

      <SidebarFooter>
        <SidebarMenu>
          <SidebarMenuItem>
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <SidebarMenuButton
                  size="lg"
                  tooltip={email}
                  className="group-data-[collapsible=icon]:justify-center! data-[state=open]:bg-sidebar-accent"
                >
                  <UserRound />
                  <span className="truncate group-data-[collapsible=icon]:hidden">{email}</span>
                  <ChevronsUpDown className="ml-auto group-data-[collapsible=icon]:hidden" />
                </SidebarMenuButton>
              </DropdownMenuTrigger>
              <DropdownMenuContent side="top" align="start" className="min-w-56">
                <DropdownMenuLabel className="truncate font-normal text-faint text-xs">
                  {email}
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

/** The workspace id in a `/workspaces/{id}…` path, or null (the home / create routes). */
function workspaceFromPath(pathname: string): string | null {
  const match = pathname.match(/^\/workspaces\/([^/]+)/);
  const id = match?.[1];
  if (!id || id === "new") {
    return null;
  }
  return id;
}

/** The smallest honest sign-out: the Better Auth client call, then a hard move to /login. */
async function signOut() {
  try {
    await authClient.signOut();
  } finally {
    window.location.href = "/login";
  }
}
