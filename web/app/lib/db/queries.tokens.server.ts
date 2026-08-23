import { eq, sql } from "drizzle-orm";
import { machineToken, serviceSession } from "@/lib/db/schema.app";
import {
  auditInTx,
  MACHINE_TOKEN_PREFIX,
  mintMachineTokenId,
  mintMachineTokenSecret,
  mintServiceSessionId,
} from "./identity.server";

export { MACHINE_TOKEN_PREFIX };

import { getDb } from "./index.server";

/**
 * Machine tokens — the workspace's headless READ credential (CI, VMs, sandboxes), and the
 * service sessions a token's runs appear as. A token is not a person: it resolves to a
 * TokenActor that only the read lanes accept, every write lane refuses it typed, and nothing
 * here touches `user` or `seat`. Custody mirrors session credentials — plaintext shown once
 * at mint, only the SHA-256 stored, the hash computed in Postgres.
 */

/** A service session idle past this window is deleted lazily on its token's next resolve. */
export const SERVICE_SESSION_IDLE_MS = 7 * 24 * 60 * 60 * 1000;

/** Same custody shape as session credentials: hash computed in Postgres, never in this tier. */
const sha256OfText = (text: string) => sql`sha256(convert_to(${text}, 'UTF8'))`;

export interface MachineTokenRow {
  tokenId: string;
  name: string;
  createdAt: Date;
  lastUsedAt: Date | null;
  serviceSessions: number;
}

/** The settings card's list — name-sorted, with each token's live service-session count. */
export async function machineTokensOf(workspaceId: string): Promise<MachineTokenRow[]> {
  const rows = await getDb().execute(sql`
    SELECT mt.id, mt.name, mt.created_at, mt.last_used_at,
      (SELECT COUNT(*) FROM web.service_session ss WHERE ss.token_id = mt.id) AS sessions
    FROM web.machine_token mt
    WHERE mt.workspace_id = ${workspaceId}
    ORDER BY mt.name, mt.id
  `);
  return (rows.rows as Record<string, unknown>[]).map((r) => ({
    tokenId: r.id as string,
    name: r.name as string,
    createdAt: new Date(r.created_at as string),
    lastUsedAt: r.last_used_at === null ? null : new Date(r.last_used_at as string),
    serviceSessions: Number(r.sessions),
  }));
}

/**
 * Mint one token (owner-gated at the route). Returns the plaintext EXACTLY ONCE — the row
 * stores its hash and the audit row its id, never the secret.
 */
export async function mintMachineToken(
  workspaceId: string,
  name: string,
  actor: { userId: string; display: string },
): Promise<{ tokenId: string; secret: string }> {
  const secret = mintMachineTokenSecret();
  const tokenId = mintMachineTokenId();
  await getDb().transaction(async (tx) => {
    await tx.insert(machineToken).values({
      id: tokenId,
      workspaceId,
      name,
      tokenSha256: sql`${sha256OfText(secret)}` as never,
      createdBy: actor.userId,
    });
    await auditInTx(tx, {
      workspaceId,
      actor,
      kind: "machine_token_minted",
      subject: tokenId,
      outcome: "ok",
      details: { name },
    });
  });
  return { tokenId, secret };
}

/** Revoke = DELETE (history = audit); the token's service sessions cascade away with it. */
export async function revokeMachineToken(
  workspaceId: string,
  tokenId: string,
  actor: { userId: string; display: string },
): Promise<"revoked" | "not_found"> {
  return await getDb().transaction(async (tx) => {
    const gone = await tx
      .delete(machineToken)
      .where(sql`${machineToken.id} = ${tokenId} AND ${machineToken.workspaceId} = ${workspaceId}`)
      .returning({ name: machineToken.name });
    if (gone.length === 0) {
      return "not_found";
    }
    await auditInTx(tx, {
      workspaceId,
      actor,
      kind: "machine_token_revoked",
      subject: tokenId,
      outcome: "ok",
      details: { name: gone[0]?.name },
    });
    return "revoked";
  });
}

export interface TokenActorRow {
  tokenId: string;
  tokenName: string;
  serviceSessionId: string;
  workspaceId: string;
}

/**
 * bearer → live token → service session, fail-closed: an unknown hash or a foreign workspace
 * resolves to nothing (the route answers the same uniform 404 an unknown session credential
 * gets — no token oracle). On a hit: the token's idle service sessions are swept, the
 * (token, reported name) session is upserted with last_seen = now, and last_used_at rides
 * along — one resolve, current state.
 */
export async function tokenActor(
  workspaceId: string,
  credential: string,
  reportedName: string | null,
): Promise<TokenActorRow | null> {
  return await getDb().transaction(async (tx) => {
    const hit = await tx.execute(sql`
      UPDATE web.machine_token mt SET last_used_at = now()
      WHERE mt.token_sha256 = ${sha256OfText(credential)} AND mt.workspace_id = ${workspaceId}
      RETURNING mt.id, mt.name
    `);
    const token = hit.rows[0] as { id: string; name: string } | undefined;
    if (token === undefined) {
      return null;
    }
    await tx.execute(sql`
      DELETE FROM web.service_session
      WHERE token_id = ${token.id}
        AND last_seen_at < now() - make_interval(secs => ${SERVICE_SESSION_IDLE_MS} / 1000.0)
    `);
    const name = reportedName === null || reportedName.trim() === "" ? token.name : reportedName;
    const upserted = await tx
      .insert(serviceSession)
      .values({
        id: mintServiceSessionId(),
        tokenId: token.id,
        workspaceId,
        displayName: name,
      })
      .onConflictDoUpdate({
        target: [serviceSession.tokenId, serviceSession.displayName],
        set: { lastSeenAt: sql`now()` },
      })
      .returning({ id: serviceSession.id });
    const sessionId = upserted[0]?.id;
    if (sessionId === undefined) {
      return null;
    }
    return {
      tokenId: token.id,
      tokenName: token.name,
      serviceSessionId: sessionId,
      workspaceId,
    };
  });
}

/** The machine's own applied-state summary, replaced wholesale — display only, per-person
 * report tables untouched. */
export async function serviceReportApplied(
  serviceSessionId: string,
  applied: { skillId: string; versionId: string }[],
): Promise<void> {
  await getDb()
    .update(serviceSession)
    .set({
      applied: applied.map((a) => ({ skill_id: a.skillId, version_id: a.versionId })),
      lastSeenAt: sql`now()`,
    })
    .where(eq(serviceSession.id, serviceSessionId));
}

export interface ServiceSessionRow {
  serviceSessionId: string;
  tokenName: string;
  displayName: string;
  createdAt: Date;
  lastSeenAt: Date;
  appliedCount: number | null;
}

/** The sessions page's service block — reviewer+ view, newest-seen first. */
export async function workspaceServiceSessions(workspaceId: string): Promise<ServiceSessionRow[]> {
  const rows = await getDb().execute(sql`
    SELECT ss.id, mt.name AS token_name, ss.display_name, ss.created_at, ss.last_seen_at,
      CASE WHEN ss.applied IS NULL THEN NULL ELSE jsonb_array_length(ss.applied) END AS applied
    FROM web.service_session ss
    JOIN web.machine_token mt ON mt.id = ss.token_id
    WHERE ss.workspace_id = ${workspaceId}
    ORDER BY ss.last_seen_at DESC, ss.id
  `);
  return (rows.rows as Record<string, unknown>[]).map((r) => ({
    serviceSessionId: r.id as string,
    tokenName: r.token_name as string,
    displayName: r.display_name as string,
    createdAt: new Date(r.created_at as string),
    lastSeenAt: new Date(r.last_seen_at as string),
    appliedCount: r.applied === null ? null : Number(r.applied),
  }));
}
