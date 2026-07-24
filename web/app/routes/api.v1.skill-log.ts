import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { laneLogOf } from "@/lib/db/queries.lane.server";
import { custodyLog } from "@/lib/plane/reads.server";

/**
 * `GET /api/v1/workspaces/{ws}/skills/{skill}/log` — the bundle's history: the custody log
 * (versions, purge tombstones, the `current` mark) decorated with the catalog identity and
 * this app's proposal events. An ARCHIVED bundle stays addressable here. Member-scoped.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const decorated = await laneLogOf(actor, params.skill ?? "");
  if (decorated === null) {
    return uniformNotFound();
  }
  const { identity, proposals } = decorated;
  const log = await custodyLog(actor.workspaceId, identity.bundleId);
  const iso = (d: Date) => d.toISOString().replace(/\.\d{3}Z$/, "Z");
  return Response.json(
    {
      skill_id: identity.bundleId,
      name: identity.name,
      kind: identity.kind,
      status: identity.status,
      ...(identity.baseName === null ? {} : { base_name: identity.baseName }),
      // The custody log is the first-parent chain FROM current, newest first — the head entry
      // IS the current version. (The tombstone's purger is app-side audit now, not custody.)
      versions: log.ok
        ? log.data.versions.map((v, index) => ({
            version_id: v.version_id,
            author: v.author_display,
            message: v.message,
            current: index === 0,
            ...(v.purged_at_ms === undefined ? {} : { purged_at: v.purged_at_ms }),
          }))
        : [],
      proposals: proposals.map((p) => ({
        version_id: p.versionId,
        proposer: p.proposer,
        status: p.status,
        ...(p.resolvedBy === null ? {} : { resolved_by: p.resolvedBy }),
        ...(p.resolvedReason === null ? {} : { resolved_reason: p.resolvedReason }),
        ...(p.resolvedAt === null ? {} : { resolved_at: iso(p.resolvedAt) }),
        created_at: iso(p.createdAt),
      })),
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
