import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { nowUtc, rowOpResponse } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceFollowSkill, deviceUnfollowSkill } from "@/lib/db/queries.device.server";

/**
 * `PUT | DELETE /api/v1/workspaces/{ws}/follows/{skill}` — a person-scoped direct follow (PUT) /
 * unfollow (DELETE) of a skill by its immutable id. Bodyless, naturally idempotent (no receipt).
 * `follow` may answer a 200 DENIED `SKILL_NOT_ACTIVE`; `unfollow` only ever yields `unfollowed` or
 * the uniform 404 (the guarded function has no `skill_not_active` arm). An unsupported method is the
 * uniform 404 (the door's no-method-oracle posture, matching `api.v1.report`).
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
  const skill = params.skill ?? "";
  const { createdAt, nowMs } = nowUtc();
  if (request.method === "PUT") {
    const status = await deviceFollowSkill(actor, skill, createdAt);
    return rowOpResponse(
      "follow",
      status,
      { followed: "followed" },
      {
        skill_not_active: "SKILL_NOT_ACTIVE",
      },
    );
  }
  const status = await deviceUnfollowSkill(actor, skill, nowMs, createdAt);
  return rowOpResponse("unfollow", status, { unfollowed: "unfollowed" }, {});
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
