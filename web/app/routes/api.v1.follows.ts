import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { rowOpResponse } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { followBundle, unfollowBundle } from "@/lib/db/queries.lane.server";

/**
 * `PUT | DELETE /api/v1/workspaces/{ws}/follows/{skill}` — the person's ONE stance row: a
 * direct follow (PUT — also lifts THIS device's exclusion and clears re-entitled detach
 * records) / an unfollow (DELETE — the standing negative mask + the detach records). Bodyless,
 * naturally idempotent (no receipt). An unsupported method is the uniform 404.
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
  if (request.method === "PUT") {
    const status = await followBundle(actor, skill);
    return rowOpResponse(
      "follow",
      status,
      { followed: "followed" },
      { skill_not_active: "SKILL_NOT_ACTIVE" },
    );
  }
  const status = await unfollowBundle(actor, skill);
  return rowOpResponse("unfollow", status, { unfollowed: "unfollowed" }, {});
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
