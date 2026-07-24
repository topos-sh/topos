import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { publishTargetOf } from "@/lib/db/queries.custody.server";
import { openProposalsOf } from "@/lib/db/queries.lane.server";
import { custodyCurrent, custodyVersionMeta } from "@/lib/plane/reads.server";

/**
 * `GET /api/v1/workspaces/{ws}/skills/{skill}/proposals` — the OPEN, NON-STALE proposals on a
 * bundle: `{version_id, base_generation, created_at}` handles only (no bytes, no proposer).
 * Staleness derives from custody: a proposal's base IS its candidate's first parent, so a
 * candidate whose parent is no longer `current` has staled and drops out of this list —
 * `base_generation` on a listed (non-stale) proposal therefore equals the live generation.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const target = await publishTargetOf(actor, params.skill ?? "");
  if (target === undefined || target.status === "deleted") {
    return uniformNotFound();
  }
  const open = await openProposalsOf(actor, target.bundleId);
  if (open.length === 0) {
    return Response.json({ proposals: [] }, { headers: NO_STORE });
  }
  const current = await custodyCurrent(actor.workspaceId, target.bundleId);
  const currentVersion = current.ok ? current.data.version_id : null;
  const currentGeneration = current.ok ? current.data.generation : 0;
  const proposals: { version_id: string; base_generation: number; created_at: string }[] = [];
  for (const row of open) {
    const meta = await custodyVersionMeta(actor.workspaceId, target.bundleId, row.versionId);
    const base = meta.ok ? (meta.data.parents[0] ?? null) : null;
    if (base !== null && base === currentVersion) {
      proposals.push({
        version_id: row.versionId,
        base_generation: currentGeneration,
        created_at: row.createdAt.toISOString().replace(/\.\d{3}Z$/, "Z"),
      });
    }
  }
  return Response.json({ proposals }, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
