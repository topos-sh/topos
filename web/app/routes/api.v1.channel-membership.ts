import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { rowOpResponse } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { laneChannelJoin, laneChannelLeave } from "@/lib/db/queries.lane.server";

/**
 * `PUT | DELETE /api/v1/workspaces/{ws}/channels/{ch}/membership` — self-serve channel join
 * (PUT) / leave (DELETE) by channel NAME. Bodyless, naturally idempotent. The DEFAULT channel
 * is joinable/leavable too now: its membership is implicit, so leaving inserts the person's
 * opt-out row and joining back deletes it — no CHANNEL_BUILTIN refusal survives. An unknown
 * channel is the uniform 404.
 */
export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "PUT" && request.method !== "DELETE") {
    return uniformNotFound();
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const channel = params.channel ?? "";
  if (request.method === "PUT") {
    const status = await laneChannelJoin(actor, channel);
    return rowOpResponse("channel", status, { joined: "joined" }, {});
  }
  const status = await laneChannelLeave(actor, channel);
  return rowOpResponse("channel", status, { left: "left", not_member: "not_member" }, {});
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
