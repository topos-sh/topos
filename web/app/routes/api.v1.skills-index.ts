import type { LoaderFunctionArgs } from "react-router";
import { laneGate } from "@/lib/api/compat.server";
import { NO_STORE, uniformNotFound } from "@/lib/api/wire.server";
import { requireSessionActor } from "@/lib/auth/guards.server";
import { laneMcpServersIndex, laneSkillsIndex } from "@/lib/db/queries.lane.server";

/**
 * `GET /api/v1/workspaces/{ws}/skills` — the workspace catalog, authorized by workspace
 * membership. Ordered by id, and split the way the feed is: `skills` are the file bundles
 * (metadata only, no bytes — the byte lane serves those), `mcp_servers` are the connected
 * servers with the document each one resolves to, because there is no byte lane behind them.
 * A bundle is named here by its CATALOG name, whatever its kind: an MCP server's embedded
 * registry name is the registry lane's business (`…/registry/v0.1/servers`), which serves it in
 * the shape a registry client expects.
 */
export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const gated = laneGate(request);
  if (gated !== null) {
    return gated;
  }
  const actor = await requireSessionActor(request, params.ws ?? "");
  const [skills, mcpServers] = await Promise.all([
    laneSkillsIndex(actor),
    laneMcpServersIndex(actor),
  ]);
  return Response.json({ skills, mcp_servers: mcpServers }, { headers: NO_STORE });
}

/** Any other HTTP method on this served path is the uniform 404 — the door owns it, so a
 * wrong-method probe answers the same envelope as a miss, never react-router's 400/405 (which
 * would leak the route's existence and, in dev, a stack). */
export function action(): Response {
  return uniformNotFound();
}
