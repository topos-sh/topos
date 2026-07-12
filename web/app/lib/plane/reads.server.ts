import { vaultFetch } from "./client.server";
import {
  failureFromResponse,
  type PlaneResult,
  tooLargeFailure,
  unreachableFailure,
} from "./errors";
import { versionCacheGet, versionCacheKey, versionCacheSet } from "./version-cache.server";
import type {
  VerificationContext,
  WireCurrentRecord,
  WireProposalDetail,
  WireProposalList,
  WireVersionMeta,
} from "./wire";

/**
 * The vault read surface. Every function returns a PlaneResult (never throws for vault/network
 * outcomes). Reads are per-request FRESH — the ONLY cache in this tier is the content-addressed
 * version-metadata LRU (version-cache.server.ts), whose keys carry no credential.
 *
 * ── The member-session read lane ─────────────────────────────────────────────────────────────
 * The content reads, authorized by WORKSPACE MEMBERSHIP: every call rides the vault's internal
 * lane, keyed on the immutable `skillId` (the catalog name is resolved to it upstream by the
 * DAL), and the acting identity is the session-verified email — threaded EXPLICITLY by the
 * caller (a guarded loader passes `actor.email`, never a wire body field). The vault re-verifies
 * the acting principal's confirmed seat in-transaction on every call, so this tier's assertion is
 * evidence, never authority.
 */

/** Parse a non-2xx response body best-effort (for the failure envelope's code/retryable). */
async function errorBody(res: Response): Promise<unknown> {
  try {
    return await res.json();
  } catch {
    return undefined;
  }
}

/** The unsigned `current` pointer, member-session lane. Always fresh — the one movable value. */
export async function sessionCurrent(
  actingEmail: string,
  ws: string,
  skillId: string,
): Promise<PlaneResult<WireCurrentRecord>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/skills/{skill}/current",
      params: { ws, skill: skillId },
      actingEmail,
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const data = (await res.json()) as WireCurrentRecord;
    return { ok: true, data, status: res.status };
  } catch {
    return unreachableFailure();
  }
}

async function fetchVersionMeta(
  actingEmail: string,
  ws: string,
  skillId: string,
  versionId: string,
): Promise<PlaneResult<WireVersionMeta>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}",
      params: { ws, skill: skillId, version_id: versionId },
      actingEmail,
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const data = (await res.json()) as WireVersionMeta;
    versionCacheSet(versionCacheKey(ws, skillId, versionId), data);
    return { ok: true, data, status: res.status };
  } catch {
    return unreachableFailure();
  }
}

/** Immutable version metadata, member-session lane. Served from the LRU when warm (a version is
 *  content-addressed, so a hit can never be stale). */
export async function sessionVersionMeta(
  actingEmail: string,
  ws: string,
  skillId: string,
  versionId: string,
): Promise<PlaneResult<WireVersionMeta>> {
  const hit = versionCacheGet(versionCacheKey(ws, skillId, versionId));
  if (hit !== undefined) {
    return { ok: true, data: hit };
  }
  return fetchVersionMeta(actingEmail, ws, skillId, versionId);
}

/**
 * The LRU-BYPASSING version-meta read, for candidates the vault may have RECLAIMED (a stale or
 * rejected proposal's bytes stay readable only while trunk-reachable or an open proposal on the
 * live base). Readability itself is the fact being asked, so a warm cache must not answer — the
 * bytes are immutable, but their retention moves. A success still warms the LRU.
 */
export async function sessionVersionMetaFresh(
  actingEmail: string,
  ws: string,
  skillId: string,
  versionId: string,
): Promise<PlaneResult<WireVersionMeta>> {
  return fetchVersionMeta(actingEmail, ws, skillId, versionId);
}

/**
 * One content-addressed object's raw bytes over the member-session lane, streamed with a hard
 * byte cap: the stream is cancelled the moment it crosses `maxBytes` (`too_large`), so an
 * oversized blob never buffers in this tier.
 */
export async function sessionBundleCapped(
  actingEmail: string,
  ws: string,
  skillId: string,
  objectId: string,
  maxBytes: number,
): Promise<PlaneResult<Uint8Array>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}",
      params: { ws, skill: skillId, object_id: objectId },
      actingEmail,
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

/** The skill's open, non-stale proposals over the member-session lane. */
export async function sessionProposals(
  actingEmail: string,
  ws: string,
  skillId: string,
): Promise<PlaneResult<WireProposalList>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/skills/{skill}/proposals",
      params: { ws, skill: skillId },
      actingEmail,
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const data = (await res.json()) as WireProposalList;
    return { ok: true, data, status: res.status };
  } catch {
    return unreachableFailure();
  }
}

/**
 * One proposal's detail over the member-session lane: the STORED status (staleness stays a
 * derived view), the base generation, the proposer (the four-eyes display surface), the live
 * review-required policy, and the resolution facts. Always fresh — the status is the page's one
 * movable review value. A never-proposed candidate is the uniform `not_found`.
 */
export async function sessionProposalDetail(
  actingEmail: string,
  ws: string,
  skillId: string,
  versionId: string,
): Promise<PlaneResult<WireProposalDetail>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}",
      params: { ws, skill: skillId, version_id: versionId },
      actingEmail,
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const data = (await res.json()) as WireProposalDetail;
    return { ok: true, data, status: res.status };
  } catch {
    return unreachableFailure();
  }
}

/**
 * The public verification-page disclosure for a device user code. No credential at all: the
 * route is public by design (the confused-deputy guard a human reviews before confirming), so it
 * rides bare — no internal-lane headers.
 */
export async function getVerificationContext(
  userCode: string,
): Promise<PlaneResult<VerificationContext>> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/v1/enroll/verify/{user_code}",
      params: { user_code: userCode },
    });
    if (!res.ok) {
      return failureFromResponse(res, await errorBody(res));
    }
    const data = (await res.json()) as VerificationContext;
    return { ok: true, data, status: res.status };
  } catch {
    return unreachableFailure();
  }
}

/** What the raw claim pass-through hands the resource route: the vault's answer, verbatim. */
export interface ClaimPassthrough {
  status: number;
  contentType: string;
  body: string;
  /** The vault's Retry-After, when it rate-limited (the frozen 429 shape carries it). */
  retryAfter?: string;
}

/**
 * The verbatim `/i/<token>` pass-through for EVERY fetch of a one-time ADMIN CLAIM link (there is
 * no HTML preview page): the incoming `Accept` rides through, and the vault's own content
 * negotiation answers — JSON for the topos client, the plain-text agent-instruction document for
 * browsers, curl, and agent web-fetches. The body is returned as-is (status + content-type
 * included), so the web tier adds no interpretation of its own. `undefined` ⇒ the vault was
 * unreachable.
 */
export async function fetchClaimPassthrough(
  token: string,
  accept: string,
): Promise<ClaimPassthrough | undefined> {
  try {
    const res = await vaultFetch({
      method: "GET",
      template: "/i/{token}",
      params: { token },
      headers: { accept: accept || "*/*" },
    });
    const body = await res.text();
    return {
      status: res.status,
      contentType: res.headers.get("content-type") ?? "application/octet-stream",
      body,
      retryAfter: res.headers.get("retry-after") ?? undefined,
    };
  } catch {
    return undefined;
  }
}
