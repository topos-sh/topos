import type { LoaderFunctionArgs } from "react-router";
import { checkBelt } from "@/lib/api/belt.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { openProposalsIndex } from "@/lib/db/queries.lane.server";
import { custodyCurrent, custodyVersionMeta } from "@/lib/plane/reads.server";

/**
 * `GET /api/v1/workspaces/{ws}/proposals` — the review inbox: every OPEN proposal in the
 * workspace, author-message first (the message reads from custody — the candidate's commit
 * frame). The caller splits inbox (others') from outbox (own) by the server-computed `yours`
 * (user-id equality — never the display string, which email-vs-name skew would mislabel);
 * `stale` derives from custody (the candidate's first parent no longer `current`).
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const belted = checkBelt(request);
  if (belted !== null) {
    return belted;
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const open = await openProposalsIndex(actor);
  const currents = new Map<string, string | null>();
  const proposals: Record<string, unknown>[] = [];
  for (const row of open) {
    let currentVersion = currents.get(row.bundleId);
    if (currentVersion === undefined) {
      const current = await custodyCurrent(actor.workspaceId, row.bundleId);
      currentVersion = current.ok ? current.data.version_id : null;
      currents.set(row.bundleId, currentVersion);
    }
    const meta = await custodyVersionMeta(actor.workspaceId, row.bundleId, row.versionId);
    const base = meta.ok ? (meta.data.parents[0] ?? null) : null;
    proposals.push({
      skill_id: row.bundleId,
      skill_name: row.bundleName,
      version_id: row.versionId,
      // A genesis-based candidate records no parent; the zero id is the honest "no base".
      base_version_id: base ?? "0".repeat(64),
      proposer: row.proposerEmail ?? row.proposerDisplay,
      // The author-owned split is a user-id comparison — the one identity — never the display
      // string (email-vs-name skew would mislabel the author's own proposal).
      yours: row.proposedBy === actor.userId,
      message: meta.ok ? meta.data.message : "",
      created_at: row.createdAt.toISOString().replace(/\.\d{3}Z$/, "Z"),
      stale: base !== currentVersion,
    });
  }
  return Response.json({ proposals }, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
