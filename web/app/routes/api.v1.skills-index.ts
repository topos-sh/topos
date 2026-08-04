import type { LoaderFunctionArgs } from "react-router";
import { laneGate } from "@/lib/api/compat.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { laneSkillsIndex } from "@/lib/db/queries.lane.server";

/**
 * `GET /api/v1/workspaces/{ws}/skills` — the workspace catalog (every bundle holding a
 * `current`), authorized by workspace membership. Metadata only, no bytes; ordered by id.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const skills = await laneSkillsIndex(actor);
  return Response.json({ skills }, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
