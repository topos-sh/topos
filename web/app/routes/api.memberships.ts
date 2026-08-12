import type { LoaderFunctionArgs } from "react-router";
import { actorFromSession } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { membershipsFor } from "@/lib/db/queries.server";

/**
 * The signed-in user's seats as JSON — the client rail's refetch target (hit when the rail
 * remounts or the window regains focus past its 60s staleTime; zero or one rows on this
 * single-tenant install). Authorization is the SAME mint the pages use: a UserActor from the live session;
 * anything else is a 401 (an API returns a status, never a redirect to /login). The seat table
 * is the sole source, read `no-store`.
 */
export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (!actor) {
    return Response.json({ error: "unauthorized" }, { status: 401 });
  }
  const memberships = await membershipsFor(actor);
  return Response.json(memberships, { headers: { "cache-control": "no-store" } });
}
