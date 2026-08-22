import { serverEnv } from "@/env.server";
import { DELIVERED_AUTH_KEY } from "@/lib/db/queries.mcp-catalog.server";

/**
 * THE DELIVERY FLIP. Where a gateway is deployed, a machine no longer dials the upstream MCP
 * server: it dials this deployment's gateway, at an address that names the CALLING session and the
 * server it wants, and the gateway attaches the workspace's sign-in on the far side.
 *
 * So delivery rewrites the document it hands over — the same document, with its remotes replaced by
 * the one gateway address and a `_meta` flag saying so. The flag is what a renderer branches on: it
 * attaches the machine's OWN standing credential to that address (the server never delivers a
 * secret), and stops teaching a per-machine sign-in that no longer happens.
 *
 * Three things this deliberately does NOT do:
 *  - it does not touch a PACKAGE-ONLY document. A server a machine installs and runs over its own
 *    pipes has no address to redirect, and putting one there would be inventing a capability.
 *  - it does not rewrite when `GATEWAY_PUBLIC_URL` is unset. That is the whole rollback: clear the
 *    variable and the next delivery carries the direct shape again.
 *  - it does not decide policy or sign-in state. Those are read at CALL time by the gateway, from
 *    rows; a delivered document that says "go through the gateway" is not a claim that a sign-in
 *    exists.
 */

/** The `_meta` key a renderer reads to know an entry is proxied. */
export const GATEWAY_META_KEY = "sh.topos/gateway";

/** The one `sh.topos/*` key a document AUTHOR may state; every other is delivery-control. */
const AUTHOR_META_KEY = "sh.topos/auth";

/**
 * Strip every reserved `sh.topos/*` control key except the author's `sh.topos/auth` from a
 * document's `_meta`. The document gate already refuses these at entry, but delivery is the last
 * chokepoint EVERY delivered document passes — including rows written before the gate learned to
 * refuse them, or by any future path — so a stored `sh.topos/gateway` can never reach a machine
 * and be read as "attach this workspace's credential to this URL". The trusted rewrite below adds
 * its own flag AFTER this runs, on documents this has already cleaned.
 */
export function sanitizeReservedMeta(document: Record<string, unknown>): Record<string, unknown> {
  const meta = document._meta;
  if (typeof meta !== "object" || meta === null) {
    return document;
  }
  const source = meta as Record<string, unknown>;
  const offending = Object.keys(source).filter(
    (key) => key.startsWith("sh.topos/") && key !== AUTHOR_META_KEY,
  );
  if (offending.length === 0) {
    return document;
  }
  const cleaned: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(source)) {
    if (!key.startsWith("sh.topos/") || key === AUTHOR_META_KEY) {
      cleaned[key] = value;
    }
  }
  return { ...document, _meta: cleaned };
}

/** The one transport a gateway remote speaks. */
const STREAMABLE_HTTP = "streamable-http";

/**
 * The gateway's public base, trailing slash trimmed, or null where this deployment runs none.
 * Exported so callers can skip the whole rewrite path (and its per-row work) in one check.
 */
export function gatewayPublicBase(): string | null {
  const configured = serverEnv().GATEWAY_PUBLIC_URL;
  return configured === undefined ? null : configured.replace(/\/+$/, "");
}

/** Whether a resolved document offers a `streamable-http` remote — the one shape the gateway can
 *  stand in front of. Mirrors the renderer's own pick: the first remote of that type with a url. */
export function hasStreamableRemote(document: Record<string, unknown>): boolean {
  const remotes = document.remotes;
  if (!Array.isArray(remotes)) {
    return false;
  }
  return remotes.some((remote) => {
    if (typeof remote !== "object" || remote === null) {
      return false;
    }
    const row = remote as Record<string, unknown>;
    return row.type === STREAMABLE_HTTP && typeof row.url === "string" && row.url.length > 0;
  });
}

/**
 * The rewritten document, or the document unchanged.
 *
 * Unchanged means the SAME OBJECT — delivery serializes what it is handed, and returning a copy of
 * an untouched document would only make a diff harder to trust. A rewrite is a shallow copy with
 * two keys replaced, so the resolved revision's own row is never mutated.
 */
export function gatewayDeliveryDocument(
  document: Record<string, unknown>,
  args: { base: string; sessionId: string; serverId: string },
): Record<string, unknown> {
  if (!hasStreamableRemote(document)) {
    return document;
  }
  const meta = (
    typeof document._meta === "object" && document._meta !== null ? document._meta : {}
  ) as Record<string, unknown>;
  return {
    ...document,
    remotes: [
      {
        type: STREAMABLE_HTTP,
        url: `${args.base}/${encodeURIComponent(args.sessionId)}/${encodeURIComponent(args.serverId)}`,
      },
    ],
    // The tier flips to `none` WITH the address: the machine-side question the key answers —
    // "what must THIS machine do before tools work" — has a different answer through the gateway
    // (nothing; the sign-in lives at the workspace), and a receipt echoing the upstream's own
    // tier would send an agent chasing a sign-in that no longer happens here.
    _meta: { ...meta, [GATEWAY_META_KEY]: true, [DELIVERED_AUTH_KEY]: "none" },
  };
}
