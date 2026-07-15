import { getSessionCookie } from "better-auth/cookies";
import type { LoaderFunctionArgs, MiddlewareFunction } from "react-router";
import { Outlet, redirect, useLoaderData } from "react-router";
import { Providers } from "@/components/providers";
import { AppSidebar } from "@/components/shell/app-sidebar";
import { SidebarProvider, SidebarTrigger } from "@/components/ui/sidebar";
import { composition } from "@/composition.server";
import { actorFromSession, requireSession } from "@/lib/auth/guards.server";
import { membershipsFor } from "@/lib/db/queries.server";
import type { NavContext } from "@/topos-web/nav";

/**
 * The signed-in shell — the sidebar rail + the content pane. It is CHROME, never a gate:
 * authorization lives in each child route's own guard (guards.server.ts), which this layout
 * never stands in for. Two independent layers keep it honest — the optimistic cookie bounce
 * below, and the real per-request seat check every child loader runs.
 */

/**
 * Optimistic sign-in bounce: if no session cookie is even PRESENT, send an obviously
 * signed-out visitor to /login before rendering the shell. This is UX only — the cookie is
 * never verified here and this check is NEVER authorization. Every child loader re-establishes
 * the session (requireSession) and re-derives admission from the seat table; a forged or stale
 * cookie sails past this bounce and dies at the guard, as it must.
 */
export const middleware: MiddlewareFunction[] = [
  ({ request }) => {
    if (!getSessionCookie(request)) {
      throw redirect("/login");
    }
  },
];

/** The active workspace segment, when the current path is workspace-scoped. */
function activeWorkspaceId(pathname: string): string | null {
  return pathname.match(/^\/workspaces\/([^/]+)/)?.[1] ?? null;
}

interface ResolvedNavEntry {
  id: string;
  label: string;
  icon?: string;
  section: string;
  href: string;
}

export async function loader({ request }: LoaderFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    throw redirect("/login");
  }
  const memberships = await membershipsFor(actor);

  // Resolve every nav slot's href here, per the ACTIVE workspace (derived from the URL — a
  // layout loader has no child `:ws` param). Entries whose href returns null for this context
  // (a workspace-scoped slot on a non-workspace page) are dropped. The context's `email` field
  // keeps its seam spelling but carries the actor's display identity (see topos-web/nav.ts).
  const ctx: NavContext = {
    workspaceId: activeWorkspaceId(new URL(request.url).pathname),
    email: actor.display,
  };
  const nav = composition.nav.flatMap<ResolvedNavEntry>((entry) => {
    const href = entry.href(ctx);
    return href === null
      ? []
      : [{ id: entry.id, label: entry.label, icon: entry.icon, section: entry.section, href }];
  });

  // Honor the rail's persisted collapsed/expanded choice server-side so the first paint matches
  // it (no expand-then-collapse flicker). Default open when the cookie is absent.
  const cookie = request.headers.get("cookie") ?? "";
  const sidebarOpen = !/(?:^|;\s*)sidebar_state=false(?:;|$)/.test(cookie);

  return { display: actor.display, memberships, nav, sidebarOpen };
}

export default function Shell() {
  const { display, memberships, nav, sidebarOpen } = useLoaderData<typeof loader>();
  return (
    // Providers seeds React Query with the rail's memberships (the loader already fetched them),
    // so the first paint carries data and a later mutation revalidates the list live.
    <Providers memberships={memberships}>
      <SidebarProvider defaultOpen={sidebarOpen}>
        <AppSidebar display={display} nav={nav} />
        {/* The content column: a banner landmark + the main region as SIBLINGS (not
            header-inside-main), so both `banner` and `main` stay discoverable. */}
        <div className="relative flex w-full min-w-0 flex-1 flex-col bg-ground">
          <header className="sticky top-0 z-10 flex h-12 shrink-0 items-center gap-2 border-line-soft border-b bg-ground px-4">
            <SidebarTrigger className="text-dim hover:text-ink" />
          </header>
          <main className="mx-auto w-full max-w-4xl flex-1 px-4 py-8 sm:px-8">
            <Outlet />
          </main>
        </div>
      </SidebarProvider>
    </Providers>
  );
}
