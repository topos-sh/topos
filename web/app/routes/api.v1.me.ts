import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { deviceMe } from "@/lib/db/queries.device.server";
import type { paths } from "@/lib/plane/contract/schema";
import { followBase } from "@/lib/plane/follow-base.server";

/**
 * `GET /api/v1/workspaces/{ws}/me` — the caller's own membership (identity + address + role + inviter
 * + invite policy), served by this tier from the directory rows (no guarded function; the vault's own
 * read is raw SQL too). Per-member and hot — never cacheable. The share ADDRESS is `<origin>/<name>`:
 * the vault builds it from its `link_base`, and here the request origin IS that base (the app is the
 * door). `invited_by` is OMITTED for a genesis/self-standup seat (never serialized as null).
 */
type WireMe =
  paths["/v1/workspaces/{ws}/me"]["get"]["responses"][200]["content"]["application/json"];

export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const row = await deviceMe(actor);
  if (row === null) {
    return uniformNotFound();
  }
  const body: WireMe = {
    workspace_id: actor.workspaceId,
    name: row.name,
    display_name: row.displayName,
    address: `${followBase(request)}/${row.name}`,
    principal: actor.person,
    role: row.role,
    invite_policy: row.invitePolicy,
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
