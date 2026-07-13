import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { nowUtc, rowOpResponse } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceExcludeDevice } from "@/lib/db/queries.device.server";

/**
 * `PUT /api/v1/workspaces/{ws}/exclusions/{skill}` — exclude a followed skill from THIS device (the
 * `remove` verb's row; `follow` lifts it). Bodyless, naturally idempotent. Command is `remove`; the
 * only outcomes are `excluded` (OK) or the uniform 404 — the guarded function has no denial arm.
 */
export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "PUT") {
    return uniformNotFound();
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const { createdAt } = nowUtc();
  const status = await deviceExcludeDevice(actor, params.skill ?? "", createdAt);
  return rowOpResponse("remove", status, { excluded: "excluded" }, {});
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
