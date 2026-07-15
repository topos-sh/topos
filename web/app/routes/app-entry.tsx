import { type LoaderFunctionArgs, redirect } from "react-router";
import { actorFromSession, requireSession } from "@/lib/auth/guards.server";
import { membershipsFor } from "@/lib/db/queries.server";

/**
 * The app entry point ("/app"). A signed-out visitor is sent to /login (requireSession is the
 * backstop); a signed-in visitor is sent on into the product. The marketing landing at "/"
 * stays the page everyone — signed in or out — sees; "/app" is the door into the product.
 *
 * Single-tenant: a seat (there is at most one) goes straight to its dashboard; seatless lands
 * on /workspaces, whose loader renders the honest miss or bounces an unclaimed install home.
 *
 * Loader-only: every path redirects, so this route never renders UI.
 */
export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  const actor = actorFromSession(await requireSession(request));
  const seat = actor ? (await membershipsFor(actor))[0] : undefined;
  if (seat !== undefined) {
    return redirect(`/workspaces/${seat.id}`);
  }
  return redirect("/workspaces");
}
