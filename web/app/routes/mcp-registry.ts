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
import { type McpCatalogEntry, scanMcpCatalog } from "@/lib/mcp/catalog.server";

/**
 * THE WORKSPACE'S OWN MCP REGISTRY — the official read API (`v0.1`), served over this
 * workspace's catalog so an agent that already speaks to a registry can point at Topos and get
 * the servers its team curates, in the shape it already parses.
 *
 *   GET …/registry/v0.1/servers                            the list
 *   GET …/registry/v0.1/servers/{name}/versions            this workspace's one version
 *   GET …/registry/v0.1/servers/{name}/versions/latest     that version, unwrapped
 *
 * `{name}` is the document's EMBEDDED registry name (`io.github.acme/weather`), whose slash is
 * percent-encoded into one path segment — so the name is parsed from the RAW pathname here
 * rather than from a decoded route param, and both spellings (`%2F` or a literal slash) resolve
 * the same way.
 *
 * Two doors, one answer: a browser cookie session, or the session lane's bearer. Everything
 * else — no credential, a workspace this actor holds no seat in, an unknown workspace slug —
 * is the ONE uniform 404 the lane answers everywhere, so this route is no existence oracle
 * either. An unknown SERVER NAME inside a workspace the caller can read is a different fact,
 * and gets the registry's own problem shape.
 *
 * The versions list is deliberately ONE entry: a workspace publishes a moving `current`, not a
 * version history an agent can pin to. What the team runs is what this serves.
 */

/** Our own `_meta` namespace inside the registry envelope — the publisher extension point. */
const META_KEY = "sh.topos/catalog";

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

/** The registry's problem shape for a name this workspace does not publish. */
function problem(detail: string): Response {
  return Response.json(
    { title: "Not Found", status: 404, detail },
    { status: 404, headers: { "cache-control": "no-store" } },
  );
}

/** One `{ server, _meta }` pair — the envelope both the list and the single read carry. */
function envelopeOf(entry: McpCatalogEntry): Record<string, unknown> {
  return {
    server: entry.server,
    _meta: {
      [META_KEY]: {
        bundleId: entry.row.bundleId,
        versionId: entry.row.versionId,
        updatedAt: new Date(entry.row.updatedAtMs).toISOString(),
      },
    },
  };
}

/**
 * Split the served path into the sub-resource this request names. The prefix is tenancy-
 * dependent (origin-rooted or under a `/:ws` slug), so it is found rather than assumed.
 */
type Target =
  | { kind: "list" }
  | { kind: "versions"; name: string }
  | { kind: "latest"; name: string }
  | { kind: "miss" };

export function parseRegistryPath(pathname: string): Target {
  const marker = "/registry/v0.1/servers";
  const at = pathname.indexOf(marker);
  if (at < 0) {
    return { kind: "miss" };
  }
  const rest = pathname.slice(at + marker.length).replace(/\/+$/, "");
  if (rest === "") {
    return { kind: "list" };
  }
  const segments = rest.split("/").filter((s) => s.length > 0);
  // The tail is `versions` or `versions/latest`; everything before it is the NAME, which may
  // arrive as one percent-encoded segment or as two literal ones.
  let kind: "versions" | "latest";
  let nameSegments: string[];
  if (segments.at(-1) === "versions") {
    kind = "versions";
    nameSegments = segments.slice(0, -1);
  } else if (segments.at(-1) === "latest" && segments.at(-2) === "versions") {
    kind = "latest";
    nameSegments = segments.slice(0, -2);
  } else {
    return { kind: "miss" };
  }
  if (nameSegments.length === 0) {
    return { kind: "miss" };
  }
  let name: string;
  try {
    name = nameSegments.map((s) => decodeURIComponent(s)).join("/");
  } catch {
    return { kind: "miss" };
  }
  return { kind, name };
}

export async function loader({ request, params }: LoaderFunctionArgs): Promise<Response> {
  const target = parseRegistryPath(new URL(request.url).pathname);
  if (target.kind === "miss") {
    return uniformNotFound();
  }
  const scope = await registryScope(request, params);
  if (scope === null) {
    return uniformNotFound();
  }
  // ONE pass over the catalog serves all three shapes: the documents are cached per immutable
  // version, so resolving a name costs the same as listing.
  const { entries } = await scanMcpCatalog(scope.actor);
  const headers = { "cache-control": "no-store" };
  if (target.kind === "list") {
    return Response.json(
      { servers: entries.map(envelopeOf), metadata: { count: entries.length } },
      { headers },
    );
  }
  const entry = entries.find((e) => e.serverName === target.name);
  if (entry === undefined) {
    return problem("this workspace does not publish a server by that name");
  }
  if (target.kind === "latest") {
    return Response.json(envelopeOf(entry), { headers });
  }
  return Response.json({ servers: [envelopeOf(entry)], metadata: { count: 1 } }, { headers });
}

/** Any other method on a served registry path is the same uniform miss — never a 405 that
 * would confirm the path exists. */
export function action(): Response {
  return uniformNotFound();
}
