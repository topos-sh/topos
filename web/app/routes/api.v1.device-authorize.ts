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
import { isWorkspaceNameShape } from "@/lib/workspace-name";

/**
 * `POST /api/v1/device/authorize` — begin the gh-style device flow toward a workspace named by
 * its address slug (`DeviceAuthStartRequest` → `DeviceAuthStartResponse`).
 *
 * SINGLE tenancy: an EMPTY `workspace` names "the workspace this origin itself addresses" (the
 * origin IS its one workspace); a non-empty name must equal this install's workspace, and any
 * other name answers the uniform 404 — the same body a wrong path gets. The flow row records
 * the install's workspace name as the slug it targets.
 *
 * MULTI tenancy: there is no origin-scoped default, so an empty name stays the uniform miss. A
 * non-empty name is validated for SHAPE ONLY (the workspace-name rule) — a shape-invalid name
 * answers the uniform 404 (such a name can never exist), and a shape-valid one MINTS the flow
 * with the slug recorded, WITHOUT any existence check. Deliberate and load-bearing: this start
 * is unauthenticated, so it must not be a workspace-existence oracle — and a CLI-first stranger
 * must be able to start an enrollment toward a workspace they will create mid-flow (the /verify
 * weave routes a seatless approver through workspace creation and back). Resolution and
 * authorization happen at APPROVAL, behind a session: the approve locks the flow, resolves the
 * recorded slug, and requires the approver's seat in the resolved workspace.
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

  let requestedWorkspace: string;
  if (composition.tenancy === "multi") {
    // Shape only — existence is deliberately NOT checked here (see the doc comment above).
    if (body.workspace.length === 0 || !isWorkspaceNameShape(body.workspace)) {
      return uniformNotFound();
    }
    requestedWorkspace = body.workspace;
  } else {
    const ws = await theWorkspace();
    if (ws === null) {
      return uniformNotFound();
    }
    // An empty workspace addresses "the origin's own workspace" — single-tenant only. A
    // non-empty name must equal this install's workspace.
    if (body.workspace !== "" && ws.name !== body.workspace) {
      return uniformNotFound();
    }
    requestedWorkspace = ws.name;
  }

  const flow = await startDeviceAuth(body.requested_name.trim(), requestedWorkspace);
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
