import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { rowOpResponse } from "@/lib/api/row-envelopes.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { excludeOnDevice } from "@/lib/db/queries.lane.server";

/**
 * `PUT /api/v1/workspaces/{ws}/exclusions/{skill}` — exclude a followed bundle from THIS
 * device (the `remove` verb's row; `follow` lifts it). The device fence is construction: the
 * only device the op can name is the actor's own credential-resolved one. Bodyless, naturally
 * idempotent.
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
  const status = await excludeOnDevice(actor, params.skill ?? "");
  return rowOpResponse("remove", status, { excluded: "excluded" }, {});
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
