import { getSessionCookie } from "better-auth/cookies";
import type { LoaderFunctionArgs, MiddlewareFunction } from "react-router";
import { Outlet, useLoaderData } from "react-router";
import { ShellChrome } from "@/components/shell/shell-chrome";
import { refuseShellSignedOut, requireShellActor } from "@/lib/auth/guards.server";
import { loadChrome } from "@/lib/shell/chrome.server";

/**
 * The signed-in shell — the sidebar rail + the content pane. It is CHROME, never a gate:
 * authorization lives in each child route's own guard (guards.server.ts), which this layout
 * never stands in for. Two independent layers keep it honest — the optimistic cookie check
 * below, and the real per-request seat check every child loader runs. They must AGREE about what
 * a signed-out visitor gets, because the optimistic one runs first and is therefore the one a
 * person actually meets.
 */

/**
 * THE TWO PAGES UNDER THIS LAYOUT THAT BELONG TO A PERSON, not to a workspace: their own session
 * list, and (in multi tenancy) creating a workspace. Everything else here is a workspace address.
 */
const PERSONAL_PAGES = new Set(["/account/sessions", "/new"]);

/**
 * Optimistic sign-in refusal: if no session cookie is even PRESENT, refuse an obviously
 * signed-out visitor before rendering the shell. This is UX only — the cookie is never verified
 * here and this check is NEVER authorization. Every child loader re-establishes the session and
 * re-derives admission from the seat table; a forged or stale cookie sails past this and dies at
 * the guard, as it must. What it must NOT do is answer differently from the guard behind it: this
 * runs before every loader, so if the two disagreed, this one would be the one a person met.
 */
export const middleware: MiddlewareFunction[] = [
  ({ request }) => {
    if (!getSessionCookie(request)) {
      refuseShellSignedOut(request, PERSONAL_PAGES);
    }
  },
];

export async function loader({ request }: LoaderFunctionArgs) {
  // A cookie that carried no live session meets the same fork, one layer down.
  const actor = await requireShellActor(request, PERSONAL_PAGES);
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
