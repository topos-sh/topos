import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceReach } from "@/lib/db/queries.device.server";
import type { paths } from "@/lib/plane/contract/schema";

/**
 * `GET /api/v1/workspaces/{ws}/skills/{skill}/reach` — a skill's audience (confirmed members entitled
 * to it + their non-revoked devices). `{skill}` is the immutable id, validated against the catalog at
 * ANY status; an unknown id is the uniform 404 (never an existence oracle). Pure counts over the
 * entitlement predicate; per-member and hot, never cacheable.
 */
type WireReach =
  paths["/v1/workspaces/{ws}/skills/{skill}/reach"]["get"]["responses"][200]["content"]["application/json"];

export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const reach = await deviceReach(actor, params.skill ?? "");
  if (reach === null) {
    return uniformNotFound();
  }
  const body: WireReach = { persons: reach.persons, devices: reach.devices };
  return Response.json(body, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
