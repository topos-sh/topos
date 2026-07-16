import type { ActionFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { badRequest, readCappedBody, uniformNotFound } from "@/lib/api/wire.server";
import { pollDeviceAuth, workspaceRowById } from "@/lib/db/identity.server";

/**
 * `POST /api/v1/device/token` — poll the device flow (`DeviceAuthPollRequest` →
 * `DeviceAuthPollResponse`). Terminal answers are delivered ONCE (the flow row dies as it is
 * reported); a `granted` poll echoes the PRESENTED device_code back as `credential` — the
 * approval promoted that same secret to the device's one bearer credential, which is how a
 * hash-only store still "delivers" it: the poller already holds it.
 *
 * The `workspace` decoration on a granted poll is THE FLOW'S workspace — the CLI records what
 * it enrolled into from this one field. It reads the workspace id the APPROVAL persisted
 * inside its fence, never a poll-time re-resolution of the mutable slug: a rename keeps the
 * decoration pointing at the approved workspace, and a delete+recreate of the slug can never
 * re-point a granted flow at a row the approval never covered. A workspace deleted inside the
 * TTL omits the field (the same arm a virgin install answers).
 */
const BODY_CAP = 8 * 1024;

export async function action({ request }: ActionFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  if (request.method !== "POST") {
    return uniformNotFound();
  }
  const raw = await readCappedBody(request, BODY_CAP, "device token body");
  if (raw instanceof Response) {
    return raw;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return badRequest("malformed JSON body");
  }
  const deviceCode = (parsed as { device_code?: unknown }).device_code;
  if (typeof parsed !== "object" || parsed === null || typeof deviceCode !== "string") {
    return badRequest("malformed device token body");
  }
  const result = await pollDeviceAuth(deviceCode);
  if (result.status !== "granted") {
    return Response.json({ status: result.status });
  }
  const ws =
    result.approvedWorkspaceId === null ? null : await workspaceRowById(result.approvedWorkspaceId);
  return Response.json({
    status: "granted",
    credential: deviceCode,
    device_id: result.deviceId,
    ...(ws === null
      ? {}
      : {
          workspace: {
            workspace_id: ws.id,
            name: ws.name,
            display_name: ws.displayName,
          },
        }),
  });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function loader(): Response {
  return uniformNotFound();
}
