import { gatewayFetch } from "./client.server";

/**
 * The three sign-in wrappers over the gateway's internal lane — the typed door every route uses,
 * so no page composes a request of its own.
 *
 * The division of labour is the same one the vault lane keeps: THIS TIER decides who may act (the
 * guards + seat rows above these calls), the gateway does the part that needs the secret — walking
 * the upstream's discovery chain, registering a client, holding ciphertext. Nothing here ever
 * learns a token, and the one body that carries a secret (`storeManualCredential`) is neither
 * logged nor echoed: its answer is an id.
 *
 * Every wrapper folds status into ONE typed outcome; an unreachable gateway is `fault`, never an
 * exception into a ceremony.
 */

/** Why an authorize walk could not start — the gateway's own vocabulary, rendered as copy above. */
export type AuthorizeRefusal = "MANUAL_ONLY" | "NO_AUTH_NEEDED" | "DISCOVERY_FAILED";

export type AuthorizeOutcome =
  | { kind: "authorize"; authorizeUrl: string }
  | { kind: "refused"; code: AuthorizeRefusal }
  | { kind: "fault" };

const REFUSALS: readonly AuthorizeRefusal[] = ["MANUAL_ONLY", "NO_AUTH_NEEDED", "DISCOVERY_FAILED"];

/**
 * Begin the authorize walk for ONE person (or, with `userId: null`, for the workspace service
 * account — the caller has already run the owner gate). `returnTo` is where the gateway sends the
 * browser once the callback has stored the credential.
 */
export async function beginAuthorize(args: {
  workspaceId: string;
  serverId: string;
  userId: string | null;
  returnTo: string;
}): Promise<AuthorizeOutcome> {
  let res: Response;
  try {
    res = await gatewayFetch({
      method: "POST",
      template: "/internal/v1/authorize/begin",
      body: args,
    });
  } catch {
    return { kind: "fault" };
  }
  if (res.ok) {
    const body = (await res.json().catch(() => null)) as { authorizeUrl?: unknown } | null;
    return typeof body?.authorizeUrl === "string"
      ? { kind: "authorize", authorizeUrl: body.authorizeUrl }
      : { kind: "fault" };
  }
  if (res.status === 409) {
    const body = (await res.json().catch(() => null)) as { code?: unknown } | null;
    const code = REFUSALS.find((known) => known === body?.code);
    return code === undefined ? { kind: "fault" } : { kind: "refused", code };
  }
  return { kind: "fault" };
}

export type StoreManualOutcome =
  | { kind: "stored"; credentialId: string }
  | { kind: "rejected" }
  | { kind: "fault" };

/**
 * Store a pasted secret. The secret goes over the internal network to the gateway and is held
 * nowhere in this tier — not in a variable that outlives the call, not in an audit detail, not in
 * the reply.
 */
export async function storeManualCredential(args: {
  workspaceId: string;
  serverId: string;
  userId: string | null;
  secret: string;
  createdByDisplay: string;
}): Promise<StoreManualOutcome> {
  let res: Response;
  try {
    res = await gatewayFetch({
      method: "POST",
      template: "/internal/v1/credentials/manual",
      body: args,
    });
  } catch {
    return { kind: "fault" };
  }
  if (res.ok) {
    const body = (await res.json().catch(() => null)) as { credentialId?: unknown } | null;
    return typeof body?.credentialId === "string"
      ? { kind: "stored", credentialId: body.credentialId }
      : { kind: "fault" };
  }
  return res.status === 400 ? { kind: "rejected" } : { kind: "fault" };
}

export type DeleteCredentialOutcome = { kind: "deleted" } | { kind: "fault" };

/**
 * Revoke one sign-in by id. A credential this tier can no longer see is already gone, so a 404 is
 * the same answer as a 204 — revoking twice must not read as a fault.
 */
export async function deleteCredential(credentialId: string): Promise<DeleteCredentialOutcome> {
  let res: Response;
  try {
    res = await gatewayFetch({
      method: "DELETE",
      template: "/internal/v1/credentials/{credentialId}",
      params: { credentialId },
    });
  } catch {
    return { kind: "fault" };
  }
  return res.ok || res.status === 404 ? { kind: "deleted" } : { kind: "fault" };
}
