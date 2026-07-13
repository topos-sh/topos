import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { nowUtc, rowOpResponse } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceChannelPlace, deviceChannelUnplace } from "@/lib/db/queries.device.server";

/**
 * `PUT | DELETE /api/v1/workspaces/{ws}/channels/{ch}/skills/{skill}` — curation: place (PUT) /
 * remove (DELETE) a skill reference in a channel. `{ch}` is the channel NAME (created member-level on
 * first placement — a bad new name is a 200 DENIED `BAD_NAME`); `{skill}` is the immutable id.
 * Bodyless, naturally idempotent. Curation on a `curated` channel takes reviewer+
 * (`CURATED_ROLE_REQUIRED`); an inactive skill is `SKILL_NOT_ACTIVE`.
 */
const CURATION_DENIED = {
  curated_role_required: "CURATED_ROLE_REQUIRED",
  bad_name: "BAD_NAME",
  skill_not_active: "SKILL_NOT_ACTIVE",
} as const;

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
  const skill = params.skill ?? "";
  const { createdAt } = nowUtc();
  if (request.method === "PUT") {
    const status = await deviceChannelPlace(actor, channel, skill, createdAt);
    return rowOpResponse(
      "channel",
      status,
      { placed: "placed", created: "created" },
      CURATION_DENIED,
    );
  }
  const status = await deviceChannelUnplace(actor, channel, skill, createdAt);
  return rowOpResponse(
    "channel",
    status,
    { removed: "removed", not_placed: "not_placed" },
    CURATION_DENIED,
  );
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
