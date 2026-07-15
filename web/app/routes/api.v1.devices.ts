import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { OP_ID } from "@/lib/api/candidate.server";
import { rowOpResponse } from "@/lib/api/row-envelopes.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { signOutDevice } from "@/lib/db/queries.devices.server";

/**
 * `DELETE /api/v1/workspaces/{ws}/devices` — the CLI's logout (`DeviceRevokeRequest`): revoke a
 * registered device's credential. SELF-ONLY since the identity unification — a device is a
 * possession of ONE user, so the target must be one of the ACTING person's own devices (a
 * foreign target reads exactly like an unknown one — the uniform 404, no oracle; the owner arm
 * died with the fleet page's revoke). The flip is instant and FINAL; re-enrolling through the
 * device flow is the recovery, and a retried logout answers `revoked` again.
 */
const BODY_CAP = 8 * 1024;

export async function action({ request, params }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "DELETE") {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "device revoke body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  const body = parsed as { op_id?: unknown; target_device_key_id?: unknown };
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof body.op_id !== "string" ||
    !OP_ID.test(body.op_id) ||
    typeof body.target_device_key_id !== "string" ||
    body.target_device_key_id.length === 0
  ) {
    return badRequest("malformed device revoke body");
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const outcome = await signOutDevice(actor, body.target_device_key_id);
  if (outcome === "unknown_device") {
    return uniformNotFound();
  }
  return rowOpResponse("logout", outcome, { revoked: "revoked" }, {});
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
