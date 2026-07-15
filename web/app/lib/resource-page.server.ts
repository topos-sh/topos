import { redirect } from "react-router";
import { actorFromSession, notFound } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { membershipsFor } from "@/lib/db/queries.server";

/**
 * The BROWSER face of a resource address (`/{workspace}[...]`), one decision for all three
 * shapes:
 *  - anonymous (no session / unverified email): the CONSTANT teaser — the same loader data for
 *    every path, existing or not (no existence oracle; the page body must not vary by path);
 *  - a signed-in member of the addressed workspace: a redirect into the workspace surface;
 *  - anyone else — a signed-in non-member, a nonexistent address: the house 404 (a miss is
 *    never distinguishable from a denial).
 *
 * Admission is the person's own confirmed seats (the same roster read the rail uses) matched
 * on the workspace ADDRESS — never a workspace probe an anonymous or unentitled caller could
 * observe.
 */
export async function resourceTeaser(
  request: Request,
  address: string,
  target: (workspaceId: string) => string,
): Promise<{ face: "teaser" }> {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (!actor) {
    return { face: "teaser" };
  }
  const memberships = await membershipsFor(actor);
  const seat = memberships.find((m) => m.address === address && m.navigable);
  if (seat) {
    throw redirect(target(seat.id));
  }
  notFound();
}
