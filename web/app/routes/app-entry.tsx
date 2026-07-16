import { type LoaderFunctionArgs, redirect } from "react-router";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import { membershipsFor } from "@/lib/db/queries.server";
import { wsPathServer } from "@/lib/ws-url.server";

/**
 * The door into the product ("/app"). A signed-out visitor is sent to /login (requireSession is
 * the backstop); a signed-in visitor is resolved to where they belong. The marketing landing at
 * "/" stays the page everyone — signed in or out — sees; "/app" is the resolver.
 *
 * A seated visitor is sent to the workspace surface under the deployment's grammar: single → the
 * origin root ("/"); multi → `/<name-slug>`. A signed-in visitor with no seat gets the house 404 —
 * there is no workspace to send them to.
 *
 * Every path redirects or 404s, so the component below never paints — but it must EXIST: a route
 * module without a component is a resource route, whose thrown 404 would serialize as a raw JSON
 * body instead of bubbling to the root boundary's house 404.
 */
export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  const actor = actorFromSession(await requireSession(request));
  const seat = actor ? (await membershipsFor(actor))[0] : undefined;
  if (seat !== undefined) {
    return redirect(wsPathServer(seat.address));
  }
  notFound();
}

export default function AppEntry() {
  return null;
}
