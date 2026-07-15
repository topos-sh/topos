import { vaultFetch } from "./client.server";
import {
  failureFromResponse,
  type PlaneResult,
  tooLargeFailure,
  unreachableFailure,
} from "./errors";
import { versionCacheGet, versionCacheKey, versionCacheSet } from "./version-cache.server";
import type { CustodyCurrent, CustodyLog, CustodyVersionMeta } from "./wire";

/**
 * The custody READ surface. Every function returns a PlaneResult (never throws for
 * vault/network outcomes) and keys on the immutable bundle id — the catalog name was resolved
 * upstream in the app's OWN tables. Authorization already happened in the caller's guard
 * (session seat or device credential); the vault serves bytes to the internal lane alone and
 * asks no identity question.
 *
 * Reads are per-request FRESH — the ONLY cache in this tier is the content-addressed
 * version-metadata LRU (version-cache.server.ts), whose keys carry no credential.
 */

/** Parse a non-2xx response body best-effort (for the failure envelope's code/retryable). */
async function errorBody(res: Response): Promise<unknown> {
  try {
    return await res.json();
  } catch {
    return undefined;
  }
}

/** The movable pointer — always fresh. A bundle with no published version is `not_found`. */
export async function custodyCurrent(
  ws: string,
  bundleId: string,
): Promise<PlaneResult<CustodyCurrent>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/bundles/{bundle}/current",
      params: { ws, bundle: bundleId },
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const data = (await res.json()) as CustodyCurrent;
    return { ok: true, data, status: res.status };
  } catch {
    return unreachableFailure();
  }
}

async function fetchVersionMeta(
  ws: string,
  bundleId: string,
  versionId: string,
): Promise<PlaneResult<CustodyVersionMeta>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/bundles/{bundle}/versions/{version_id}",
      params: { ws, bundle: bundleId, version_id: versionId },
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const data = (await res.json()) as CustodyVersionMeta;
    versionCacheSet(versionCacheKey(ws, bundleId, versionId), data);
    return { ok: true, data, status: res.status };
  } catch {
    return unreachableFailure();
  }
}

/** Immutable version metadata, served from the LRU when warm (a hit can never be stale). */
export async function custodyVersionMeta(
  ws: string,
  bundleId: string,
  versionId: string,
): Promise<PlaneResult<CustodyVersionMeta>> {
  const hit = versionCacheGet(versionCacheKey(ws, bundleId, versionId));
  if (hit !== undefined) {
    return { ok: true, data: hit };
  }
  return fetchVersionMeta(ws, bundleId, versionId);
}

/**
 * The LRU-BYPASSING version-meta read, for candidates the vault may have RECLAIMED or purged.
 * Readability itself is the fact being asked, so a warm cache must not answer — the bytes are
 * immutable, but their retention moves. A success still warms the LRU.
 */
export async function custodyVersionMetaFresh(
  ws: string,
  bundleId: string,
  versionId: string,
): Promise<PlaneResult<CustodyVersionMeta>> {
  return fetchVersionMeta(ws, bundleId, versionId);
}

/** The bundle's version history (purge tombstones included), custody-side. */
export async function custodyLog(ws: string, bundleId: string): Promise<PlaneResult<CustodyLog>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/bundles/{bundle}/log",
      params: { ws, bundle: bundleId },
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const data = (await res.json()) as CustodyLog;
    return { ok: true, data, status: res.status };
  } catch {
    return unreachableFailure();
  }
}

/**
 * One content-addressed object's UPSTREAM response, streamed through untouched — the device
 * lane's bundle read (a large blob crosses this tier chunk by chunk; nothing buffers). The
 * caller has already authorized; a 404 stays a 404, any other non-2xx is `null` (the route's
 * store-fault answer).
 */
export async function custodyObjectStream(
  ws: string,
  bundleId: string,
  objectId: string,
): Promise<Response | null> {
  try {
    return await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/bundles/{bundle}/objects/{object_id}",
      params: { ws, bundle: bundleId, object_id: objectId },
    });
  } catch {
    return null;
  }
}

/**
 * One content-addressed object's raw bytes, streamed with a hard byte cap: the stream is
 * cancelled the moment it crosses `maxBytes` (`too_large`), so an oversized blob never buffers
 * in this tier.
 */
export async function custodyObjectCapped(
  ws: string,
  bundleId: string,
  objectId: string,
  maxBytes: number,
): Promise<PlaneResult<Uint8Array>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/bundles/{bundle}/objects/{object_id}",
      params: { ws, bundle: bundleId, object_id: objectId },
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const declared = res.headers.get("content-length");
    if (declared !== null && Number(declared) > maxBytes) {
      await res.body?.cancel();
      return tooLargeFailure();
    }
    const body = res.body;
    if (body === null) {
      return { ok: true, data: new Uint8Array(0), status: res.status };
    }
    const reader = body.getReader();
    const chunks: Uint8Array[] = [];
    let total = 0;
    for (;;) {
      const { done, value } = await reader.read();
      if (done) {
        break;
      }
      total += value.byteLength;
      if (total > maxBytes) {
        await reader.cancel();
        return tooLargeFailure();
      }
      chunks.push(value);
    }
    const bytes = new Uint8Array(total);
    let offset = 0;
    for (const chunk of chunks) {
      bytes.set(chunk, offset);
      offset += chunk.byteLength;
    }
    return { ok: true, data: bytes, status: res.status };
  } catch {
    return unreachableFailure();
  }
}
