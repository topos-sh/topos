import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { HEX_64 } from "@/lib/api/candidate.server";
import { badRequest, internalError, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { publishTargetOf } from "@/lib/db/queries.custody.server";
import { custodyObjectStream } from "@/lib/plane/reads.server";

/**
 * `GET /api/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}` — one content-addressed
 * object's raw bytes, STREAMED through from the vault (nothing buffers here — a large blob
 * crosses this tier chunk by chunk). Authorization is the device's membership, resolved HERE;
 * the vault confines the read to the bundle's own reachability (no object is served by bare
 * hash) and the client re-verifies every byte against the content id.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const objectId = params.objectId ?? "";
  if (!HEX_64.test(objectId)) {
    return badRequest("malformed object id");
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const target = await publishTargetOf(actor, params.skill ?? "");
  if (target === undefined || target.status === "deleted") {
    return uniformNotFound();
  }
  const upstream = await custodyObjectStream(actor.workspaceId, target.bundleId, objectId);
  if (upstream === null) {
    return internalError();
  }
  if (upstream.status === 404) {
    return uniformNotFound();
  }
  if (!upstream.ok) {
    return internalError();
  }
  const headers = new Headers({ "cache-control": "private, max-age=31536000, immutable" });
  const contentType = upstream.headers.get("content-type");
  if (contentType !== null) {
    headers.set("content-type", contentType);
  }
  const contentLength = upstream.headers.get("content-length");
  if (contentLength !== null) {
    headers.set("content-length", contentLength);
  }
  return new Response(upstream.body, { status: 200, headers });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
