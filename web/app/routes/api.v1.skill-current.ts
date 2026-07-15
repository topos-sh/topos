import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireDeviceActor } from "@/lib/auth/guards.server";
import { publishTargetOf } from "@/lib/db/queries.custody.server";
import { custodyCurrent } from "@/lib/plane/reads.server";

/**
 * `GET /api/v1/workspaces/{ws}/skills/{skill}/current` — the unsigned `WireCurrentRecord`
 * (`{schema_version, scope, record: {version_id, generation}}`), ETag = the generation for the
 * conditional-GET/304 the client's currency check rides. A bundle with no published version —
 * or none at all — is the uniform 404.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireDeviceActor(request, params.ws ?? "");
  const skillId = params.skill ?? "";
  const target = await publishTargetOf(actor, skillId);
  if (target === undefined || target.status === "deleted") {
    return uniformNotFound();
  }
  const current = await custodyCurrent(actor.workspaceId, target.bundleId);
  if (!current.ok) {
    return uniformNotFound();
  }
  const etag = `"${current.data.generation}"`;
  if (request.headers.get("if-none-match") === etag) {
    return new Response(null, { status: 304, headers: { etag, ...NO_STORE } });
  }
  return Response.json(
    {
      schema_version: 1,
      scope: { workspace_id: actor.workspaceId, skill_id: target.bundleId },
      record: { version_id: current.data.version_id, generation: current.data.generation },
    },
    { headers: { etag, ...NO_STORE } },
  );
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
