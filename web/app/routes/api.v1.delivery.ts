import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deliveryFor, emptyDeliveryFor } from "@/lib/db/queries.lane.server";

/**
 * `GET /api/v1/workspaces/{ws}/delivery` — the auto-update answer for ONE enrolled device,
 * assembled in ONE snapshot transaction (the entitled/detached/notices sets can never straddle
 * a subscription change). Per-device, hot, never cacheable. One of the exactly TWO
 * pending-tolerant routes: a PENDING link answers the shape-complete EMPTY body with
 * `link_status` "pending" — no data flows over a pending link, but the client learns its
 * standing instead of a phantom 404.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireDeviceActor(request, params.ws ?? "", { allowPending: true });
  const body =
    actor.linkStatus === "pending" ? await emptyDeliveryFor(actor) : await deliveryFor(actor);
  return Response.json(body, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
