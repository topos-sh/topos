import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { nowUtc, rowOpResponse } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceChannelJoin, deviceChannelLeave } from "@/lib/db/queries.device.server";

/**
 * `PUT | DELETE /api/v1/workspaces/{ws}/channels/{ch}/membership` — self-serve channel join (PUT) /
 * leave (DELETE) by channel NAME. Bodyless, naturally idempotent. Joining or leaving the structural
 * `everyone` is a 200 DENIED `CHANNEL_BUILTIN` (membership there IS the roster). Leave answers `left`
 * or the idempotent `not_member`; an unknown channel is the uniform 404.
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
  const { createdAt, nowMs } = nowUtc();
  if (request.method === "PUT") {
    const status = await deviceChannelJoin(actor, channel, createdAt);
    return rowOpResponse("channel", status, { joined: "joined" }, { builtin: "CHANNEL_BUILTIN" });
  }
  const status = await deviceChannelLeave(actor, channel, nowMs, createdAt);
  return rowOpResponse(
    "channel",
    status,
    { left: "left", not_member: "not_member" },
    { builtin: "CHANNEL_BUILTIN" },
  );
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
