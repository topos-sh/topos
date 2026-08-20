import type { LoaderFunctionArgs } from "react-router";
import { bearerToken, uniformNotFound } from "@/lib/api/wire.server";
import {
  actorFromSession,
  type MemberActor,
  memberInScope,
  requireSessionActor,
  workspaceInScope,
} from "@/lib/auth/guards.server";
import { getAuth } from "@/lib/auth/server";
import {
  mcpRegistryLimit,
  workspaceRegistryServer,
  workspaceRegistryServers,
} from "@/lib/db/queries.mcp-catalog.server";
import {
  parseRegistryPath,
  registryEnvelope,
  registryList,
  registryProblem,
  selectVersion,
} from "@/lib/mcp/registry-api.server";

/**
 * THE WORKSPACE'S OWN MCP REGISTRY — the official read API (`v0.1`), served over the servers this
 * workspace runs, so an agent that already speaks to a registry can point at Topos and get the
 * team's servers in the shape it already parses.
 *
 *   GET …/registry/v0.1/servers                            the list (cursor-paged)
 *   GET …/registry/v0.1/servers/{name}/versions            what this workspace runs
 *   GET …/registry/v0.1/servers/{name}/versions/latest     that one, unwrapped
 *
 * `{name}` is the document's EMBEDDED registry name (`io.github.acme/weather`), whose slash is
 * percent-encoded into one path segment — so the name is parsed from the RAW pathname rather than
 * from a decoded route param, and both spellings resolve the same way.
 *
 * Two doors, one answer: a browser cookie session, or the session lane's bearer. Everything
 * else — no credential, a workspace this actor holds no seat in, an unknown workspace slug — is
 * the ONE uniform 404 the lane answers everywhere, so this route is no existence oracle either.
 * An unknown SERVER NAME inside a workspace the caller can read is a different fact, and gets the
 * registry's own problem shape.
 *
 * The versions list is deliberately ONE entry: what this lane answers is WHAT THE TEAM RUNS —
 * the revision the workspace's connection resolves to, pin included. The catalog's full history
 * is the catalog's own, not this lane's.
 */

interface RegistryScope {
  actor: MemberActor | Awaited<ReturnType<typeof requireSessionActor>>;
}

/**
 * Resolve the caller through whichever door they used, or null for the uniform 404. The bearer
 * arm runs the lane's own guard (which throws its uniform 404 response); the cookie arm runs
 * the ordinary membership resolution and folds every miss — signed out, unknown slug, no seat —
 * into the same null, because a machine lane must not bounce a browser redirect at an agent.
 */
async function registryScope(
  request: Request,
  params: { ws?: string },
): Promise<RegistryScope | null> {
  const workspace = await workspaceInScope(params).catch(() => null);
  if (workspace === null) {
    return null;
  }
  if (bearerToken(request) !== null) {
    return { actor: await requireSessionActor(request, workspace.id) };
  }
  const session = await getAuth().api.getSession({ headers: request.headers });
  const user = actorFromSession(session);
  if (user === null) {
    return null;
  }
  const scoped = await memberInScope(user, params).catch(() => null);
  return scoped === null ? null : { actor: scoped.actor };
}

/** The path prefix this lane owns — the tenancy-dependent part is found, not assumed. */
const MARKER = "/registry/v0.1/servers";

export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const url = new URL(request.url);
  const target = parseRegistryPath(url.pathname, MARKER);
  if (target.kind === "miss") {
    return uniformNotFound();
  }
  const scope = await registryScope(request, params);
  if (scope === null) {
    return uniformNotFound();
  }
  const headers = { "cache-control": "no-store" };
  if (target.kind === "list") {
    const page = await workspaceRegistryServers(scope.actor, {
      cursor: url.searchParams.get("cursor"),
      limit: mcpRegistryLimit(url.searchParams.get("limit")),
    });
    return Response.json(registryList(page.rows, page.nextCursor), { headers });
  }
  const row = await workspaceRegistryServer(scope.actor, target.name);
  if (row === undefined || row === null) {
    return registryProblem("this workspace does not run a server by that name");
  }
  if (target.kind === "versions") {
    return Response.json(registryList([row]), { headers });
  }
  const selected = selectVersion([row], target.version);
  if (selected === undefined) {
    return registryProblem("this workspace does not run that version of that server");
  }
  return Response.json(registryEnvelope(selected), { headers });
}

/** Any other method on a served registry path is the same uniform miss — never a 405 that
 * would confirm the path exists. */
export function action(): Response {
  return uniformNotFound();
}
