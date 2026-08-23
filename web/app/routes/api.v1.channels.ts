import type { LoaderFunctionArgs } from "react-router";
import { laneGate } from "@/lib/api/compat.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { isTokenActor, requireReadActor } from "@/lib/auth/guards.server";
import { laneChannels } from "@/lib/db/queries.lane.server";

/**
 * `GET /api/v1/workspaces/{ws}/channels` — the workspace channels (the default channel
 * included, name-sorted), each with the caller's membership, its member count, and its
 * name-sorted bundle references. Per-member and hot, never cacheable.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  // A machine token reads this lane too: a CI checkout with [channels] rows resolves their
  // member lists here. A token is nobody in particular, so `included` answers only the
  // everyone-wide assignments for it.
  const actor = await requireReadActor(request, params.ws ?? "");
  const channels = await laneChannels({
    workspaceId: actor.workspaceId,
    userId: isTokenActor(actor) ? null : actor.userId,
  });
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
