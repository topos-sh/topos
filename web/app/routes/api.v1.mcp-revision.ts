import type { LoaderFunctionArgs } from "react-router";
import { laneGate } from "@/lib/api/compat.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireReadActor } from "@/lib/auth/guards.server";
import { laneMcpRevision } from "@/lib/db/queries.lane.server";

/**
 * `GET /api/v1/workspaces/{ws}/mcp-servers/{bundle}/revisions/{revision}` — ONE STORED REVISION of
 * a server this workspace connects, in the same shape the catalog index (`…/skills`) serves the
 * connection's current one.
 *
 * It exists so a committed `topos.lock` means for an MCP entry exactly what it means for a skill:
 * a checkout installs the revision the lock NAMES, not whatever the catalog happens to serve
 * today. The answer is what delivery would have answered while that revision was current, so what
 * lands in an agent's config is the same document a teammate received then.
 *
 * Both doors of a read lane, exactly as the catalog index takes them: a person's session bearer
 * and a machine token, through the one branded read actor. Every miss — no credential, no seat, a
 * bundle this workspace does not connect, a revision of another server, one nobody was ever
 * delivered, a document today's gate refuses — is the ONE uniform 404, so the lane is no oracle
 * for the catalog at large.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  const actor = await requireReadActor(request, params.ws ?? "");
  const entry = await laneMcpRevision(actor, params.skill ?? "", params.revisionId ?? "");
  if (entry === null) {
    return uniformNotFound();
  }
  return Response.json(entry, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss. */
export function action(): Response {
  return uniformNotFound();
}
