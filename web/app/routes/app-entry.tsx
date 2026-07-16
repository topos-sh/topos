import { type LoaderFunctionArgs, redirect } from "react-router";
import { composition } from "@/composition.server";
import { actorFromSession, notFound, requireSession } from "@/lib/auth/guards.server";
import { membershipsFor } from "@/lib/db/queries.server";
import { wsPathServer } from "@/lib/ws-url.server";

/**
 * The door into the product ("/app"). A signed-out visitor is sent to /login (requireSession is
 * the backstop); a signed-in visitor is resolved to where they belong. The marketing landing at
 * "/" stays the page everyone — signed in or out — sees; "/app" is the resolver.
 *
 * A seated visitor is sent to the workspace surface under the deployment's grammar: single → the
 * origin root ("/"); multi → `/<name-slug>`. A signed-in visitor with NO seat: in multi tenancy
 * there is a workspace to make, so they go to self-serve creation ("/new"); single tenancy has no
 * workspace to send them to (the install IS its one workspace), so the house 404 stands.
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
  if (composition.tenancy === "multi") {
    return redirect("/new");
  }
  notFound();
}

export default function AppEntry() {
  return null;
}
