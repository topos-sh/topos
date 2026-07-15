import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { laneSkillsIndex } from "@/lib/db/queries.lane.server";

/**
 * `GET /api/v1/workspaces/{ws}/skills` — the workspace catalog (every bundle holding a
 * `current`), authorized by workspace membership. Metadata only, no bytes; ordered by id.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const skills = await laneSkillsIndex(actor);
  return Response.json({ skills }, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
