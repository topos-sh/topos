import { type LoaderFunctionArgs, redirect } from "react-router";
import { actorFromSession, requireSession } from "@/lib/auth/guards.server";
import { planeMembershipsFor } from "@/lib/db/queries.server";

/**
 * The app entry point ("/app"). A signed-out visitor is sent to /login (requireSession is the
 * verified backstop); a signed-in visitor is sent on into the product. The marketing landing at
 * "/" stays the page everyone — signed in or out — sees; "/app" is the door into the product.
 *
 * The sole-membership fast-path lives HERE, not on /workspaces: this entry means "take me to my
 * stuff", so one workspace goes straight to it. /workspaces stays the always-rendered index — a
 * deliberate visit to the collection never bounces past the list (or its New-workspace action).
 *
 * Loader-only: every path redirects, so this route never renders UI.
 */
export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  const actor = actorFromSession(await requireSession(request));
  const memberships = actor ? await planeMembershipsFor(actor) : [];
  const sole = memberships[0];
  // Fast-path only on ONE membership TOTAL: a second row — a pending invite included — must be
  // SEEN on the index, never skipped past. And an invited-only sole row isn't enterable, so it
  // lands on the index (and its join instructions) too.
  if (memberships.length === 1 && sole?.navigable) {
    return redirect(`/workspaces/${sole.id}`);
  }
  return redirect("/workspaces");
}
