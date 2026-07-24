import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { rowOpResponse } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { profileIncludeChannel, profileRemoveChannel } from "@/lib/db/queries.lane.server";

/**
 * `PUT | DELETE /api/v1/workspaces/{ws}/profile/channels/{channel}` — edit the caller's OWN
 * per-workspace profile: PUT writes the channel include line (`add -g @ws/channels/x`; on the
 * implicit DEFAULT channel it clears any exclude — the baseline needs no include line),
 * DELETE removes it (`remove -g`) — the default channel, having no include line, takes an
 * EXCLUDE line instead (the one negative state). `{channel}` is the channel NAME. Bodyless,
 * naturally idempotent.
 */
export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "PUT" && request.method !== "DELETE") {
    return uniformNotFound();
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const channel = params.channel ?? "";
  if (request.method === "PUT") {
    const status = await profileIncludeChannel(actor, channel);
    return rowOpResponse("add", status, { included: "included" }, {});
  }
  const status = await profileRemoveChannel(actor, channel);
  return rowOpResponse(
    "remove",
    status,
    { removed: "removed", excluded: "excluded", not_in_profile: "not_in_profile" },
    {},
  );
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
