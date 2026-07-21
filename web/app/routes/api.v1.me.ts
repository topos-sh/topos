import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { laneMe } from "@/lib/db/queries.lane.server";
import { workspaceAddress } from "@/lib/ws-url.server";

/**
 * `GET /api/v1/workspaces/{ws}/me` — the caller's own membership (identity + address + role +
 * inviter). Per-member and hot — never cacheable. The share ADDRESS follows the
 * deployment's grammar (bare origin in single tenancy, `<origin>/<name>` in multi) — the CLI
 * follows exactly what it emits; the request origin IS the base (the app is the door). `invited_by` is
 * OMITTED for a genesis seat (never serialized as null); `principal` carries the acting
 * person's display identity (email is a login attribute, not an authority key).
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const row = await laneMe(actor);
  if (row === null) {
    return uniformNotFound();
  }
  const body = {
    workspace_id: actor.workspaceId,
    name: row.name,
    display_name: row.displayName,
    address: workspaceAddress(request, row.name),
    principal: actor.display,
    role: row.role,
    ...(row.invitedBy !== null ? { invited_by: row.invitedBy } : {}),
  };
  return Response.json(body, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
