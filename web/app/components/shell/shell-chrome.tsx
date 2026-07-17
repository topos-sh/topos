import type { ReactNode } from "react";
import { Link } from "react-router";
import { Providers } from "@/components/providers";
import { AppSidebar } from "@/components/shell/app-sidebar";
import { SidebarProvider, SidebarTrigger } from "@/components/ui/sidebar";
import type { ChromeData } from "@/lib/shell/chrome.server";
import { wsHref } from "@/lib/ws-path";

/**
 * The signed-in chrome — the sidebar rail + the content column — rendered VERBATIM by both
 * layouts (the login-bounce `shell.tsx` and the anonymous-tolerant `face-shell.tsx`), so the two
 * can't drift. It is presentation only; every gate lives in the child route's own loader.
 */
export function ShellChrome({
  display,
  memberships,
  nav,
  sidebarOpen,
  tenancy,
  workspace,
  children,
}: ChromeData & { children: ReactNode }) {
  // The wordmark's home target, built the same way the sidebar builds its links: the workspace
  // root under the deployment's grammar, or /app when no workspace is in scope (a seatless
  // visitor, e.g. /new onboarding in multi).
  const rootHref =
    workspace === null ? "/app" : wsHref(tenancy === "multi" ? workspace.address : null);
  return (
    // Providers seeds React Query with the rail's memberships (the loader already fetched them),
    // so the first paint carries data and a later mutation revalidates the list live.
    <Providers memberships={memberships}>
      <SidebarProvider defaultOpen={sidebarOpen}>
        <AppSidebar display={display} nav={nav} tenancy={tenancy} workspace={workspace} />
        {/* The content column: a banner landmark + the main region as SIBLINGS (not
            header-inside-main), so both `banner` and `main` stay discoverable. The banner carries
            the `topos_` wordmark (the panel's header strip holds the workspace identity now). The
            ONE obvious collapse toggle lives in the panel's header strip; the banner keeps a
            trigger only on mobile, where the panel is off-canvas and its strip toggle isn't
            reachable while closed. */}
        <div className="relative flex w-full min-w-0 flex-1 flex-col bg-ground">
          <header className="sticky top-0 z-10 flex h-12 shrink-0 items-center gap-2 border-line-soft border-b bg-ground px-4">
            <SidebarTrigger className="shrink-0 text-dim hover:text-ink md:hidden" />
            <Link
              to={rootHref}
              className="font-display font-semibold text-ink text-sm tracking-[-0.02em] focus-visible:outline-2 focus-visible:outline-accent focus-visible:outline-offset-2"
            >
              topos<span className="text-accent">_</span>
            </Link>
          </header>
          <main className="mx-auto w-full max-w-4xl flex-1 px-4 py-8 sm:px-8">{children}</main>
        </div>
      </SidebarProvider>
    </Providers>
  );
}
