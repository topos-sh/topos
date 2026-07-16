import type { ActionFunctionArgs } from "react-router";
import { composition } from "@/composition.server";
import { checkBelt } from "@/lib/api/belt.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import {
  DEVICE_AUTH_POLL_INTERVAL_SECS,
  startDeviceAuth,
  theWorkspace,
} from "@/lib/db/identity.server";
import { followBase } from "@/lib/plane/follow-base.server";

/**
 * `POST /api/v1/device/authorize` — begin the gh-style device flow toward a workspace named by
 * its address slug (`DeviceAuthStartRequest` → `DeviceAuthStartResponse`). An EMPTY `workspace`
 * names "the workspace this origin itself addresses" — honored only in single-tenant mode (the
 * origin IS its one workspace); in multi tenancy there is no origin-scoped default, so an empty
 * name is the uniform miss. A NON-empty name must equal this install's workspace in BOTH modes
 * (multi-tenant enrollment beyond the one workspace stays deferred). Whether the name exists is
 * never disclosed beyond this install's own: a name that is not it answers the uniform 404 — the
 * same body a wrong path gets.
 *
 * No credential yet: this is the flow's unauthenticated start (the belt is its only gate). The
 * response's `device_code` is the polling secret — and, on approval, the device's ONE bearer
 * credential (promoted server-side; the poll echoes it back from the field the client already
 * holds).
 */
const BODY_CAP = 8 * 1024;
const MAX_REQUESTED_NAME = 200;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "device authorize body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  const body = parsed as { requested_name?: unknown; workspace?: unknown };
  if (
    typeof parsed !== "object" ||
    parsed === null ||
    typeof body.requested_name !== "string" ||
    body.requested_name.trim().length === 0 ||
    body.requested_name.length > MAX_REQUESTED_NAME ||
    typeof body.workspace !== "string"
  ) {
    return badRequest("malformed device authorize body");
  }
  const ws = await theWorkspace();
  if (ws === null) {
    return uniformNotFound();
  }
  // An empty workspace addresses "the origin's own workspace" — single-tenant only. A non-empty
  // name must equal this install's workspace in both modes.
  const originAddressed = body.workspace === "" && composition.tenancy === "single";
  if (!originAddressed && ws.name !== body.workspace) {
    return uniformNotFound();
  }
  const flow = await startDeviceAuth(body.requested_name.trim());
  const origin = followBase(request);
  return Response.json({
    device_code: flow.deviceCode,
    user_code: flow.userCode,
    verification_uri: `${origin}/verify`,
    verification_uri_complete: `${origin}/verify?code=${encodeURIComponent(flow.userCode)}`,
    expires_in_secs: flow.expiresInSecs,
    interval_secs: DEVICE_AUTH_POLL_INTERVAL_SECS,
  });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
