import type { McpRegistryRow } from "@/lib/db/queries.mcp-catalog.server";

/**
 * THE REGISTRY SHAPE — catalog rows told in the official read API's own words, written once for
 * every lane that speaks it.
 *
 * An agent that already talks to `registry.modelcontextprotocol.io` should be able to point at
 * this install and keep its parser: a list is `{ servers: [ { server, _meta } ], metadata }`, a
 * server's history is the same envelope repeated, and `latest` names whichever revision is on
 * offer. Two lanes serve it — the public catalog feed and a workspace's own registry — and they
 * differ ONLY in which rows they may see. That is why the shaping lives here rather than in
 * either route: two spellings of one envelope is how a client ends up parsing two registries.
 *
 * WHAT WE ADD, AND WHERE WE PUT IT. The document is served VERBATIM, whatever `_meta` it carries
 * of its own. Everything this catalog knows on top of it — the sign-in tier somebody actually
 * verified, the scope options, whether this revision is the current one — rides under OUR key.
 * The official registry's own namespace is never written here: those fields are upstream's
 * statements about a server's life in ITS registry, and a subregistry that filled them in would
 * be forging the one thing a reader uses to tell the two apart.
 */

/** Our own namespace inside the envelope's `_meta` — the publisher extension point. */
export const REGISTRY_META_KEY = "sh.topos/catalog";

/**
 * The official registry's reserved namespace. Named here so the rule is a constant a test can
 * hold this module to, never a convention somebody has to remember.
 */
export const OFFICIAL_META_KEY = "io.modelcontextprotocol.registry/official";

/** One `{ server, _meta }` pair — the envelope both the list and a single read carry. */
export function registryEnvelope(row: McpRegistryRow): Record<string, unknown> {
  const meta: Record<string, unknown> = {
    isLatest: row.isLatest,
  };
  // A sign-in tier is stated only where somebody established one. NULL is not `none`: "nobody
  // checked" and "this server asks for nothing" are different claims, and only one was made.
  if (row.authMode !== null) {
    meta.auth = row.authMode;
  }
  if (row.authNote !== null) {
    meta.authNote = row.authNote;
  }
  if (row.scopeMenu !== null && row.scopeMenu !== undefined) {
    meta.scopes = row.scopeMenu;
  }
  return { server: row.document, _meta: { [REGISTRY_META_KEY]: meta } };
}

/**
 * A LIST answer. `metadata.nextCursor` is present only when there IS a next page — the official
 * shape's own signal that a client is done, rather than a null it has to interpret.
 */
export function registryList(
  rows: McpRegistryRow[],
  nextCursor: string | null = null,
): Record<string, unknown> {
  return {
    servers: rows.map(registryEnvelope),
    metadata: {
      count: rows.length,
      ...(nextCursor === null ? {} : { nextCursor }),
    },
  };
}

/** The registry's problem shape for a name (or a version) this lane does not serve. */
export function registryProblem(detail: string): Response {
  return Response.json(
    { title: "Not Found", status: 404, detail },
    { status: 404, headers: { "cache-control": "no-store" } },
  );
}

/**
 * What one request names. The server NAME carries a slash (`io.github.acme/weather`), which may
 * arrive percent-encoded as one segment or as two literal ones — so the path is split around the
 * `versions` marker rather than read out of a decoded route param.
 *
 * `version` is the official read API's version selector, where `latest` is the reserved word for
 * whichever revision is currently on offer.
 */
export type RegistryTarget =
  | { kind: "list" }
  | { kind: "versions"; name: string }
  | { kind: "version"; name: string; version: string }
  | { kind: "miss" };

export function parseRegistryPath(pathname: string, marker: string): RegistryTarget {
  const at = pathname.indexOf(marker);
  if (at < 0) {
    return { kind: "miss" };
  }
  const rest = pathname.slice(at + marker.length).replace(/\/+$/, "");
  if (rest === "") {
    return { kind: "list" };
  }
  const segments = rest.split("/").filter((s) => s.length > 0);
  // The LAST `versions` is the marker: a server name may legitimately contain the word, and the
  // tail is at most one segment, so the rightmost occurrence is the only reading that works for
  // both `…/io.github.acme/versions/versions` and `…/acme/versions`.
  const pivot = segments.lastIndexOf("versions");
  if (pivot < 1 || pivot < segments.length - 2) {
    return { kind: "miss" };
  }
  let name: string;
  try {
    name = segments
      .slice(0, pivot)
      .map((s) => decodeURIComponent(s))
      .join("/");
  } catch {
    return { kind: "miss" };
  }
  if (name.length === 0) {
    return { kind: "miss" };
  }
  const tail = segments[pivot + 1];
  if (tail === undefined) {
    return { kind: "versions", name };
  }
  let version: string;
  try {
    version = decodeURIComponent(tail);
  } catch {
    return { kind: "miss" };
  }
  return { kind: "version", name, version };
}

/**
 * Which of a server's revisions a version selector names. `latest` is the one on offer — which
 * is what `isLatest` marks — and anything else is matched against the document's own version
 * string, since that is the number a client read off the document it holds.
 */
export function selectVersion(rows: McpRegistryRow[], version: string): McpRegistryRow | undefined {
  if (version === "latest") {
    return rows.find((row) => row.isLatest) ?? rows[0];
  }
  return rows.find((row) => documentVersion(row) === version);
}

/** The `version` inside a document, or null for one that names none. */
export function documentVersion(row: McpRegistryRow): string | null {
  const value = row.document.version;
  return typeof value === "string" ? value : null;
}
