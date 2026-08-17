import type { LoaderFunctionArgs } from "react-router";
import { serverEnv } from "@/env.server";
import { uniformNotFound } from "@/lib/api/wire.server";
import {
  mcpRegistryLimit,
  publishedCatalogServers,
  publishedCatalogVersions,
} from "@/lib/db/queries.mcp-catalog.server";
import {
  parseRegistryPath,
  registryEnvelope,
  registryList,
  registryProblem,
  selectVersion,
} from "@/lib/mcp/registry-api.server";

/**
 * THE PUBLIC CATALOG FEED — this install's global MCP catalog, in the official read API's shape,
 * for anyone to read and for another install to sync from.
 *
 *   GET /mcp-catalog/v0.1/servers                          the list (cursor-paged)
 *   GET /mcp-catalog/v0.1/servers/{name}/versions          that server's published history
 *   GET /mcp-catalog/v0.1/servers/{name}/versions/latest   the one it currently offers
 *
 * PUBLISHED GLOBAL ROWS ONLY. A workspace's private server is its own and is never exported; a
 * candidate is a row somebody pulled in to look at, not a recommendation; a revoked revision was
 * pulled back on purpose. What this serves is exactly what this install stands behind — which is
 * the only thing another install should be syncing.
 *
 * NO CREDENTIAL, and no workspace: this describes the DEPLOYMENT's catalog, so it is origin-rooted
 * in both tenancy grammars and answers the same bytes to everyone. That is also why it may be
 * cached briefly — a catalog changes when staff publish something, not per reader.
 *
 * OFF UNLESS A DEPLOYMENT SAYS OTHERWISE (`TOPOS_MCP_CATALOG_FEED`). Being a feed is a decision:
 * an install whose catalog is its own team's business should not become a source other installs
 * follow because it happened to run this code. Off, the paths are the house's uniform miss.
 */

/** The path prefix this lane owns. */
const MARKER = "/mcp-catalog/v0.1/servers";

/** How long a shared cache may hold an answer. Short: a publish should show up the same minute. */
const CACHE_CONTROL = "public, max-age=60";

export async function loader({ request }: LoaderFunctionArgs): Promise<Response> {
  if (serverEnv().TOPOS_MCP_CATALOG_FEED !== "on") {
    return uniformNotFound();
  }
  const url = new URL(request.url);
  const target = parseRegistryPath(url.pathname, MARKER);
  if (target.kind === "miss") {
    return uniformNotFound();
  }
  const headers = { "cache-control": CACHE_CONTROL };
  if (target.kind === "list") {
    const page = await publishedCatalogServers({
      cursor: url.searchParams.get("cursor"),
      limit: mcpRegistryLimit(url.searchParams.get("limit")),
    });
    return Response.json(registryList(page.rows, page.nextCursor), { headers });
  }
  const versions = await publishedCatalogVersions(target.name);
  if (versions.length === 0) {
    return registryProblem("this catalog does not publish a server by that name");
  }
  if (target.kind === "versions") {
    return Response.json(registryList(versions), { headers });
  }
  const selected = selectVersion(versions, target.version);
  if (selected === undefined) {
    return registryProblem("this catalog does not publish that version of that server");
  }
  return Response.json(registryEnvelope(selected), { headers });
}

/** Any other method on a served path is the same uniform miss — never a 405 that would confirm
 * the path exists. */
export function action(): Response {
  return uniformNotFound();
}
