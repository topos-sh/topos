import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { laneChannels } from "@/lib/db/queries.lane.server";

/**
 * `GET /api/v1/workspaces/{ws}/channels` — the workspace channels (the default channel
 * included, name-sorted), each with the caller's membership, its member count, and its
 * name-sorted bundle references. Per-member and hot, never cacheable.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const channels = await laneChannels(actor);
  const body = {
    channels: channels.map((c) => ({
      name: c.name,
      mode: c.mode,
      builtin: c.builtin,
      included: c.included,

      skills: c.skills.map((s) => ({ skill_id: s.skillId, name: s.name })),
    })),
  };
  return Response.json(body, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
