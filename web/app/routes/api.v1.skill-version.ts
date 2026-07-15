import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { HEX_64 } from "@/lib/api/candidate.server";
import { badRequest, NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { publishTargetOf } from "@/lib/db/queries.custody.server";
import { custodyVersionMeta } from "@/lib/plane/reads.server";

/**
 * `GET /api/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}` — a version's
 * authenticated metadata (`WireVersionMeta`): parents, author, message, the consent
 * `bundle_digest`, and the per-file `(path, mode, object_id)` leaves. NO blob bytes — the
 * client fetches each by `object_id` and re-hashes against the digest pin.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const versionId = params.versionId ?? "";
  if (!HEX_64.test(versionId)) {
    return badRequest("malformed version id");
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const target = await publishTargetOf(actor, params.skill ?? "");
  if (target === undefined || target.status === "deleted") {
    return uniformNotFound();
  }
  const meta = await custodyVersionMeta(actor.workspaceId, target.bundleId, versionId);
  if (!meta.ok) {
    return uniformNotFound();
  }
  return Response.json(
    {
      version_id: meta.data.version_id,
      parents: meta.data.parents,
      author: meta.data.author,
      message: meta.data.message,
      bundle_digest: meta.data.bundle_digest,
      files: meta.data.files.map((f) => ({ path: f.path, mode: f.mode, object_id: f.object_id })),
    },
    { headers: NO_STORE },
  );
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
