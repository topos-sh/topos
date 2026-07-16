import { getSessionCookie } from "better-auth/cookies";
import type { LoaderFunctionArgs, MiddlewareFunction } from "react-router";
import { Outlet, redirect, useLoaderData } from "react-router";
import { ShellChrome } from "@/components/shell/shell-chrome";
import { actorFromSession, requireSession } from "@/lib/auth/guards.server";
import { loadChrome } from "@/lib/shell/chrome.server";

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

export async function loader({ request }: LoaderFunctionArgs) {
  const actor = actorFromSession(await requireSession(request));
  if (!actor) {
    throw redirect("/login");
  }
  // The nav slots resolve here per the ACTIVE workspace (derived from the URL under the tenancy
  // grammar — a layout loader has no child `:ws` param). Shared with face-shell so the two chromes
  // cannot drift.
  return loadChrome(request, actor);
}

export default function Shell() {
  const chrome = useLoaderData<typeof loader>();
  return (
    <ShellChrome {...chrome}>
      <Outlet />
    </ShellChrome>
  );
}
