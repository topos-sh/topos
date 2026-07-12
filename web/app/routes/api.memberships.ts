import type { LoaderFunctionArgs } from "react-router";
import { actorFromSession } from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import { planeMembershipsFor } from "@/lib/db/queries.server";

/**
 * The signed-in user's workspace roster as JSON — the client rail's refetch target (hit after a
 * workspace is created so the sidebar updates without a reload). Authorization is the SAME mint
 * the pages use: a UserActor only for a VERIFIED session email; anything else is a 401 (an API
 * returns a status, never a redirect to /login). The directory roster is the sole source, read
 * `no-store`.
 */
export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  const session = await getAuth().api.getSession({ headers: request.headers });
  const actor = actorFromSession(session);
  if (!actor) {
    return Response.json({ error: "unauthorized" }, { status: 401 });
  }
  const memberships = await planeMembershipsFor(actor);
  return Response.json(memberships, { headers: { "cache-control": "no-store" } });
}
