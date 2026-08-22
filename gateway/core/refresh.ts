/**
 * OAuth token refresh — the ONE place a token endpoint is spoken to at call time.
 *
 * Serialization: the store's refresh lock (a row lock in the container, trivially exclusive at
 * the edge) brackets the whole refresh, and the FIRST thing inside the lock is a re-read of the
 * credential — a concurrent caller that lost the race finds the already-rotated secret and never
 * hits the token endpoint. Refresh happens at most once per gateway request; on any failure the
 * caller passes the upstream 401 through untouched.
 *
 * Nothing here logs: a token endpoint exchange is all secrets.
 */

import type { Credential, GatewayContext } from "./ports";

export interface RefreshScope {
  workspaceId: string;
  serverId: string;
  userId: string;
}

/** True when the credential carries everything a refresh_token grant needs. */
export function isRefreshable(cred: Credential): boolean {
  return cred.kind === "oauth" && !!cred.refreshToken && !!cred.tokenEndpoint && !!cred.clientId;
}

/**
 * Rotate the credential (or adopt a concurrent rotation) and return the fresh secret; null when
 * the refresh cannot be completed — the caller then surfaces the upstream 401.
 */
export async function refreshCredential(
  ctx: GatewayContext,
  scope: RefreshScope,
  cred: Credential,
): Promise<string | null> {
  if (!isRefreshable(cred)) return null;
  try {
    return await ctx.store.withRefreshLock(cred.id, async () => {
      const fresh = await ctx.store.credentialFor(scope.workspaceId, scope.serverId, scope.userId);
      // A racing request already rotated under this lock's serialization: use its result.
      if (fresh && fresh.id === cred.id && fresh.secret !== cred.secret) return fresh.secret;
      if (!fresh || fresh.id !== cred.id) return null; // Revoked mid-flight — fail closed.

      const body = new URLSearchParams({
        grant_type: "refresh_token",
        refresh_token: cred.refreshToken as string,
        client_id: cred.clientId as string,
      });
      const resp = await ctx.env.fetch(cred.tokenEndpoint as string, {
        method: "POST",
        headers: { "content-type": "application/x-www-form-urlencoded", accept: "application/json" },
        body: body.toString(),
      });
      if (!resp.ok) return null;
      const parsed: unknown = await resp.json().catch(() => null);
      if (typeof parsed !== "object" || parsed === null) return null;
      const accessToken = (parsed as Record<string, unknown>)["access_token"];
      if (typeof accessToken !== "string" || accessToken.length === 0) return null;
      const rotatedRefresh = (parsed as Record<string, unknown>)["refresh_token"];
      await ctx.store.saveRotatedCredential(cred.id, {
        secret: accessToken,
        ...(typeof rotatedRefresh === "string" && rotatedRefresh.length > 0 ? { refreshToken: rotatedRefresh } : {}),
      });
      return accessToken;
    });
  } catch {
    return null;
  }
}
