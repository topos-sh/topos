import { randomBytes } from "node:crypto";
import { appendFileSync } from "node:fs";
import { eq, sql } from "drizzle-orm";
import { serverEnv } from "@/env.server";
import { type Db, getDb, isUniqueViolation } from "./index.server";
import {
  auditEvent,
  bundleDetachment,
  channel,
  device,
  deviceAuthSession,
  seat,
  workspace,
} from "./schema.app";

/**
 * The identity ceremonies' data layer: first-boot setup, the claim-code consume, the
 * gh-style device flow (approve + mint), and the last-owner-fenced seat mutations. These are
 * the concurrency-critical writes of the identity model — each fence is ONE transaction,
 * FOR UPDATE-locked or single-statement-atomic, with its audit row emitted inside the same
 * transaction.
 *
 * Secrets are HASH-STORED, and the hashing happens IN Postgres (the built-in SHA-256 over the
 * UTF-8 bytes) — this tier generates randomness but never computes a digest itself. A
 * presented code or credential is matched by `sha256(convert_to($x, 'UTF8'))`; the plaintext
 * never lands in a table, a log, or an error.
 */

// ── Id + code minting ────────────────────────────────────────────────────────────────────────

/** Opaque row ids keep their historical wire shapes (w_…, s_…, dk_… are frozen wire facts). */
export function mintWorkspaceId(): string {
  return `w_${randomBytes(16).toString("hex")}`;
}
export function mintBundleId(): string {
  return `s_${randomBytes(16).toString("hex")}`;
}
export function mintChannelId(): string {
  return `c_${randomBytes(16).toString("hex")}`;
}
export function mintDeviceId(): string {
  return `dk_${randomBytes(16).toString("hex")}`;
}
export function mintInvitationId(): string {
  return `inv_${randomBytes(16).toString("hex")}`;
}
export function mintProposalId(): string {
  return `p_${randomBytes(16).toString("hex")}`;
}

/** A high-entropy single-use secret (claim codes, device codes): 32 random bytes, base64url. */
function mintSecret(): string {
  return randomBytes(32).toString("base64url");
}

/**
 * The short human code the device flow shows ("open /verify and enter AB29-CD34"): eight
 * characters from an unambiguous alphabet (no I/O/0/1), grouped for reading aloud.
 */
function mintUserCode(): string {
  const alphabet = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
  const bytes = randomBytes(8);
  let code = "";
  for (let i = 0; i < 8; i++) {
    code += alphabet[(bytes[i] as number) % alphabet.length];
    if (i === 3) {
      code += "-";
    }
  }
  return code;
}

/** The one place a presented plaintext meets a stored hash — SHA-256 computed IN Postgres. */
const sha256OfText = (text: string) => sql`sha256(convert_to(${text}, 'UTF8'))`;

// ── Audit (same-transaction emission — the app-wide convention) ─────────────────────────────

type Tx = Parameters<Parameters<Db["transaction"]>[0]>[0];

export interface AuditActor {
  userId?: string;
  deviceId?: string;
  display: string;
}

/** Emit an audit row INSIDE the caller's transaction (append-only by code discipline). */
export async function auditInTx(
  tx: Tx,
  args: {
    workspaceId: string;
    actor: AuditActor;
    kind: string;
    subject?: string;
    outcome: string;
    details?: Record<string, unknown>;
  },
): Promise<void> {
  await tx.insert(auditEvent).values({
    workspaceId: args.workspaceId,
    actorUserId: args.actor.userId,
    actorDeviceId: args.actor.deviceId,
    actorDisplay: args.actor.display,
    kind: args.kind,
    subject: args.subject,
    outcome: args.outcome,
    details: args.details ?? {},
  });
}

// ── Setup (first boot): the boot-minted workspace + the printed claim link ──────────────────

let setupEnsuredThisBoot = false;

/**
 * The genesis ceremony, idempotent per process: create the workspace on a virgin database
 * (with its default channel — every workspace is born with one), and while it stays
 * unclaimed, (re)mint the claim code and print ONE line to the logs (+ an optional volume
 * file): the only tokened URL in the product, genesis-only, dead after one use.
 *
 * The code is regenerated on every boot while unclaimed (a stale printed link stops
 * working); `TOPOS_SETUP_CODE` presets it for CI/IaC and is then stable across boots. Only
 * the SHA-256 is stored. Runs under an advisory lock so parallel first requests race safely.
 */
export async function ensureSetup(
  requestOrigin: string,
  tenancy: "single" | "multi" = "single",
): Promise<void> {
  // MULTI tenancy mints no boot workspace and no claim code — workspaces are born through the
  // superset's own creation surface, not the single-tenant genesis ceremony.
  if (tenancy === "multi") {
    return;
  }
  if (setupEnsuredThisBoot) {
    return;
  }
  const env = serverEnv();
  const db = getDb();
  const code = env.TOPOS_SETUP_CODE ?? mintSecret();
  let printLink = false;
  await db.transaction(async (tx) => {
    await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext('topos_setup'))`);
    const existing = await tx.execute(
      sql`SELECT id, claimed_at IS NOT NULL AS claimed FROM ${workspace} LIMIT 1`,
    );
    if (existing.rows.length === 0) {
      const workspaceId = mintWorkspaceId();
      const name = env.TOPOS_WORKSPACE_NAME;
      await tx.insert(workspace).values({
        id: workspaceId,
        name,
        displayName: name,
        claimCodeSha256: sql`${sha256OfText(code)}` as never,
      });
      await tx.insert(channel).values({
        id: mintChannelId(),
        workspaceId,
        name: "everyone",
        isDefault: true,
      });
      await auditInTx(tx, {
        workspaceId,
        actor: { display: "system" },
        kind: "workspace_created",
        subject: name,
        outcome: "ok",
      });
      printLink = true;
    } else if (!(existing.rows[0] as { claimed: boolean }).claimed) {
      await tx.execute(
        sql`UPDATE ${workspace} SET claim_code_sha256 = ${sha256OfText(code)} WHERE claimed_at IS NULL`,
      );
      printLink = true;
    }
  });
  setupEnsuredThisBoot = true;
  if (printLink) {
    const origin = serverEnv().TOPOS_PUBLIC_URL ?? requestOrigin;
    const line = `→ Finish setup: ${origin}/claim?code=${code}`;
    // biome-ignore lint/suspicious/noConsole: the printed setup line IS the product surface.
    console.log(line);
    if (env.TOPOS_SETUP_LINK_FILE) {
      try {
        appendFileSync(env.TOPOS_SETUP_LINK_FILE, `${line}\n`);
      } catch {
        // The file is a convenience mirror of the log line; failing to write it never blocks boot.
      }
    }
  }
}

/** The single-tenant read: the one workspace this install serves (null on a virgin DB). */
export async function theWorkspace(): Promise<typeof workspace.$inferSelect | null> {
  const rows = await getDb().select().from(workspace).limit(1);
  return rows[0] ?? null;
}

/**
 * The multi-tenant read: the workspace a NAME slug names (null on a miss). The name is the unique
 * address slug — the multi-tenant browser URL key. A miss resolves the same uniform 404 as a
 * non-member, so this discloses no more than the member gate already does.
 */
export async function workspaceByName(name: string): Promise<typeof workspace.$inferSelect | null> {
  const rows = await getDb().select().from(workspace).where(eq(workspace.name, name)).limit(1);
  return rows[0] ?? null;
}

/** The claim page's GET probe: the workspace IF the presented code is live. Uniform miss otherwise. */
export async function claimableWorkspace(
  code: string,
): Promise<{ id: string; name: string; displayName: string } | null> {
  const rows = await getDb().execute(
    sql`SELECT id, name, display_name FROM ${workspace}
        WHERE claim_code_sha256 = ${sha256OfText(code)} AND claimed_at IS NULL`,
  );
  const row = rows.rows[0] as { id: string; name: string; display_name: string } | undefined;
  return row ? { id: row.id, name: row.name, displayName: row.display_name } : null;
}

/**
 * FENCE 1 — the claim-code consume: one atomic UPDATE is the race arbiter (two concurrent
 * claims: exactly one row returns; the loser gets the uniform miss). Consuming sets
 * claimed_at and clears the hash in the same statement (the workspace CHECK ties the two),
 * then seats the claimant as the first owner. Single-use by construction.
 */
export async function consumeClaim(
  code: string,
  userId: string,
  userDisplay: string,
): Promise<{ workspaceId: string } | null> {
  return await getDb().transaction(async (tx) => {
    const consumed = await tx.execute(
      sql`UPDATE ${workspace} SET claimed_at = now(), claim_code_sha256 = NULL
          WHERE claim_code_sha256 = ${sha256OfText(code)} AND claimed_at IS NULL
          RETURNING id`,
    );
    const row = consumed.rows[0] as { id: string } | undefined;
    if (!row) {
      return null;
    }
    await tx.insert(seat).values({ workspaceId: row.id, userId, role: "owner" });
    await auditInTx(tx, {
      workspaceId: row.id,
      actor: { userId, display: userDisplay },
      kind: "workspace_claimed",
      outcome: "ok",
    });
    return { workspaceId: row.id };
  });
}

// ── The gh-style device flow ─────────────────────────────────────────────────────────────────

const DEVICE_AUTH_TTL_MS = 15 * 60 * 1000;
export const DEVICE_AUTH_POLL_INTERVAL_SECS = 5;
export const DEVICE_AUTH_EXPIRES_IN_SECS = DEVICE_AUTH_TTL_MS / 1000;

/**
 * Start a device authorization: mint the pair of codes and park the pending row. The
 * device_code is the CLI's polling secret — and, on approval, it is PROMOTED to the device's
 * one bearer credential (same plaintext, same stored hash shape), which is what lets the
 * hash-only store still "deliver" the credential on the poll: the poller already holds it.
 * The short user_code is what a human types at /verify; the partial unique index keeps it
 * unambiguous among PENDING rows, so minting retries on that one conflict.
 */
export async function startDeviceAuth(
  requestedName: string,
): Promise<{ deviceCode: string; userCode: string; expiresInSecs: number }> {
  const db = getDb();
  // Opportunistic reap: every new enrollment first clears expired ceremony rows (there is no
  // separate scheduler), which also frees any expired pending user_code for reuse. Only
  // past-TTL rows go, so a live grant awaiting its idempotent re-poll is never touched.
  await sweepExpiredDeviceAuth();
  const deviceCode = mintSecret();
  const expiresAt = new Date(Date.now() + DEVICE_AUTH_TTL_MS);
  for (let attempt = 0; attempt < 5; attempt++) {
    const userCode = mintUserCode();
    try {
      await db.insert(deviceAuthSession).values({
        id: `da_${randomBytes(16).toString("hex")}`,
        userCode,
        deviceCodeSha256: sql`${sha256OfText(deviceCode)}` as never,
        requestedName,
        expiresAt,
      });
      return { deviceCode, userCode, expiresInSecs: DEVICE_AUTH_EXPIRES_IN_SECS };
    } catch (error) {
      if (isUniqueViolation(error) && attempt < 4) {
        continue; // a live pending row already shows this user_code — mint another
      }
      throw error;
    }
  }
  throw new Error("device auth start: user_code space exhausted");
}

export type DevicePollResult =
  | { status: "pending" }
  | { status: "denied" }
  | { status: "expired" }
  | { status: "granted"; deviceId: string };

/**
 * The CLI's poll, keyed by the device_code hash. IDEMPOTENT by design: a terminal answer
 * (granted / denied) repeats on every poll until the row is swept, because the client's
 * crash-recovery is to re-poll — a device that received `granted` but crashed before
 * persisting its credential re-polls the same code and must get the same `granted` again
 * (the credential is the presented device_code, echoed by the route, so re-delivery costs
 * nothing). Terminal rows are reaped by [`sweepExpiredDeviceAuth`], not on read, so the grant
 * survives its whole TTL. A missing row (already swept, or never existed) reads as expired.
 */
export async function pollDeviceAuth(deviceCode: string): Promise<DevicePollResult> {
  const rows = await getDb().execute(
    sql`SELECT status, device_id, expires_at < now() AS expired
        FROM ${deviceAuthSession}
        WHERE device_code_sha256 = ${sha256OfText(deviceCode)}`,
  );
  const row = rows.rows[0] as
    | { status: string; device_id: string | null; expired: boolean }
    | undefined;
  if (!row) {
    return { status: "expired" };
  }
  if (row.status === "denied") {
    return { status: "denied" };
  }
  if (row.status === "approved" && row.device_id !== null) {
    // A granted flow stays granted for its whole TTL — the approve already minted the device,
    // so this is a permanent fact until the sweep reaps the ceremony row.
    return { status: "granted", deviceId: row.device_id };
  }
  // pending — expired pending is terminal (the human never approved in time).
  return row.expired ? { status: "expired" } : { status: "pending" };
}

/**
 * Reap device-auth ceremony rows past their TTL — a periodic sweep (the app's maintenance
 * loop), NOT a read-time delete, so an idempotent re-poll of a fresh grant always finds it.
 * A grant the client already consumed is harmless to keep until expiry (the credential is
 * live regardless); this only bounds the table.
 */
export async function sweepExpiredDeviceAuth(): Promise<number> {
  const result = await getDb().execute(
    sql`DELETE FROM ${deviceAuthSession} WHERE expires_at < now()`,
  );
  return result.rowCount ?? 0;
}

/**
 * FENCE 2 — the device-flow approve + mint, one FOR UPDATE transaction: lock the pending row
 * by user_code, re-check liveness under the lock, mint the device row (owned by the
 * approver, credential hash = the device_code hash), and flip the row to approved. The
 * step-up gate runs in the ROUTE before this is called — approval mints a credential that
 * acts as you.
 */
export async function approveDeviceAuth(
  userCode: string,
  approver: { userId: string; display: string },
  workspaceId: string,
): Promise<{ deviceId: string; requestedName: string } | null> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(
      sql`SELECT id, requested_name, device_code_sha256 FROM ${deviceAuthSession}
          WHERE user_code = ${userCode} AND status = 'pending' AND expires_at > now()
          FOR UPDATE`,
    );
    const row = rows.rows[0] as
      | { id: string; requested_name: string; device_code_sha256: Buffer }
      | undefined;
    if (!row) {
      return null;
    }
    const deviceId = mintDeviceId();
    await tx.insert(device).values({
      id: deviceId,
      userId: approver.userId,
      displayName: row.requested_name,
      credentialSha256: row.device_code_sha256,
    });
    await tx.execute(
      sql`UPDATE ${deviceAuthSession}
          SET status = 'approved', approved_by = ${approver.userId}, device_id = ${deviceId}
          WHERE id = ${row.id}`,
    );
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: approver.userId, display: approver.display },
      kind: "device_approved",
      subject: deviceId,
      outcome: "ok",
      details: { requestedName: row.requested_name },
    });
    return { deviceId, requestedName: row.requested_name };
  });
}

/** The verify page's deny arm — same lock discipline, terminal 'denied'. */
export async function denyDeviceAuth(
  userCode: string,
  denier: { userId: string; display: string },
  workspaceId: string,
): Promise<boolean> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(
      sql`UPDATE ${deviceAuthSession} SET status = 'denied'
          WHERE user_code = ${userCode} AND status = 'pending' AND expires_at > now()
          RETURNING requested_name`,
    );
    const row = rows.rows[0] as { requested_name: string } | undefined;
    if (!row) {
      return false;
    }
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: denier.userId, display: denier.display },
      kind: "device_denied",
      subject: row.requested_name,
      outcome: "ok",
    });
    return true;
  });
}

/** The verify page's lookup: the pending request a typed user_code names (display only). */
export async function pendingDeviceAuth(
  userCode: string,
): Promise<{ requestedName: string } | null> {
  const rows = await getDb().execute(
    sql`SELECT requested_name FROM ${deviceAuthSession}
        WHERE user_code = ${userCode} AND status = 'pending' AND expires_at > now()`,
  );
  const row = rows.rows[0] as { requested_name: string } | undefined;
  return row ? { requestedName: row.requested_name } : null;
}

// ── Step-up email confirmation (the password-less admin-ceremony rung) ──────────────────────

const STEP_UP_TTL_MS = 10 * 60 * 1000;

/** Namespaced per user AND per ceremony page, so a step-up token never collides with Better
 * Auth's own verification rows (email verification, magic links, reset), a consume only ever
 * touches this person's, and a token mailed for ONE ceremony page cannot be spent on another
 * (the reach of an act and the grade of its ceremony stay matched — a link requested on the
 * members page proves nothing toward a purge). `scope` is the ceremony page's pathname. */
const stepUpIdentifier = (userId: string, scope: string) => `step-up:${userId}:${scope}`;

/**
 * Mint a single-use step-up confirmation token for a password-less account and store ONLY its
 * hash (SHA-256 computed IN Postgres, hex-encoded into the text `verification.value`) under a
 * per-user, per-ceremony-page identifier with a short TTL. The prior token for this user+page is
 * dropped first, so at most one confirmation link is ever live per ceremony. Returns the
 * plaintext — which only ever leaves as the mailed link. Randomness is this tier's; the digest
 * is the database's.
 */
export async function mintStepUpConfirmation(userId: string, scope: string): Promise<string> {
  const token = mintSecret();
  const identifier = stepUpIdentifier(userId, scope);
  const expiresAt = new Date(Date.now() + STEP_UP_TTL_MS);
  await getDb().transaction(async (tx) => {
    await tx.execute(sql`DELETE FROM web.verification WHERE identifier = ${identifier}`);
    await tx.execute(
      sql`INSERT INTO web.verification (id, identifier, value, expires_at)
          VALUES (${`su_${randomBytes(16).toString("hex")}`}, ${identifier},
                  encode(${sha256OfText(token)}, 'hex'), ${expiresAt})`,
    );
  });
  return token;
}

/**
 * Consume a presented step-up token: ONE atomic DELETE … RETURNING, so a token is usable at most
 * once and only before expiry, and only under its own user's identifier for the SAME ceremony
 * page it was minted on (a foreign token, or one minted for a different ceremony, misses).
 * Single-use by construction — the row is gone the instant it matches.
 */
export async function consumeStepUpConfirmation(
  userId: string,
  scope: string,
  token: string,
): Promise<boolean> {
  if (token.length === 0) {
    return false;
  }
  const rows = await getDb().execute(
    sql`DELETE FROM web.verification
        WHERE identifier = ${stepUpIdentifier(userId, scope)}
          AND value = encode(${sha256OfText(token)}, 'hex')
          AND expires_at > now()
        RETURNING id`,
  );
  return rows.rows.length > 0;
}

// ── The device lane's actor resolve ─────────────────────────────────────────────────────────

export interface DeviceActorRow {
  deviceId: string;
  userId: string;
  userDisplay: string;
  role: "owner" | "reviewer" | "member";
}

/**
 * credential-hash → device → user → seat, one query, fail-closed: a revoked device or a
 * seatless owner resolves to nothing (the route answers the uniform wire 404). The hash is
 * computed in Postgres; last_seen_at rides along.
 */
export async function deviceActor(
  workspaceId: string,
  credential: string,
): Promise<DeviceActorRow | null> {
  const rows = await getDb().execute(
    sql`UPDATE ${device} d SET last_seen_at = now()
        FROM ${seat} s, web."user" u
        WHERE d.credential_sha256 = ${sha256OfText(credential)}
          AND d.revoked_at IS NULL
          AND u.id = d.user_id
          AND s.user_id = d.user_id AND s.workspace_id = ${workspaceId}
        RETURNING d.id AS device_id, d.user_id, u.name AS user_display, s.role`,
  );
  const row = rows.rows[0] as
    | { device_id: string; user_id: string; user_display: string; role: DeviceActorRow["role"] }
    | undefined;
  if (!row) {
    return null;
  }
  return {
    deviceId: row.device_id,
    userId: row.user_id,
    userDisplay: row.user_display,
    role: row.role,
  };
}

/**
 * Self-service revocation — SELF-ONLY by design (a device is a possession; no owner arm
 * reaches into someone else's pocket), effective immediately and FINAL (the trigger refuses
 * any un-revoke).
 */
export async function revokeOwnDevice(
  actor: { userId: string; display: string },
  deviceId: string,
  workspaceId: string,
): Promise<boolean> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(
      sql`UPDATE ${device} SET revoked_at = now()
          WHERE id = ${deviceId} AND user_id = ${actor.userId} AND revoked_at IS NULL
          RETURNING id`,
    );
    if (rows.rows.length === 0) {
      return false;
    }
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "device_revoked",
      subject: deviceId,
      outcome: "ok",
    });
    return true;
  });
}

/**
 * The invited sign-up's binding leg: convert every pending, unexpired invitation for this
 * (verified) address into a seat, atomically per run. Called from the auth layer's
 * after-verification hook — the mailbox round-trip IS the identity rung, so this is the one
 * place an invitation becomes admission. Locked so a concurrent verification and a
 * revocation serialize.
 */
export async function bindInvitedSeats(
  userId: string,
  email: string,
  display: string,
): Promise<number> {
  const lowered = email.trim().toLowerCase();
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(
      sql`SELECT id, workspace_id, role, invited_by FROM web.invitation
          WHERE email = ${lowered} AND status = 'pending'
            AND (expires_at IS NULL OR expires_at > now())
          FOR UPDATE`,
    );
    let bound = 0;
    for (const raw of rows.rows) {
      const inv = raw as {
        id: string;
        workspace_id: string;
        role: string;
        invited_by: string | null;
      };
      await tx.execute(
        sql`UPDATE web.invitation
            SET status = 'accepted', accepted_by = ${userId}, accepted_at = now()
            WHERE id = ${inv.id}`,
      );
      await tx.execute(
        sql`INSERT INTO ${seat} (workspace_id, user_id, role, invited_by)
            VALUES (${inv.workspace_id}, ${userId}, ${inv.role}, ${inv.invited_by})
            ON CONFLICT (workspace_id, user_id) DO NOTHING`,
      );
      await auditInTx(tx, {
        workspaceId: inv.workspace_id,
        actor: { userId, display },
        kind: "invitation_accepted",
        subject: lowered,
        outcome: "ok",
        details: { role: inv.role },
      });
      bound++;
    }
    return bound;
  });
}

/** The admission read: the seat a user holds in a workspace (undefined = no admission). */
export async function seatOf(
  userId: string,
  workspaceId: string,
): Promise<{ role: "owner" | "reviewer" | "member" } | undefined> {
  const rows = await getDb()
    .select({ role: seat.role })
    .from(seat)
    .where(sql`${seat.userId} = ${userId} AND ${seat.workspaceId} = ${workspaceId}`)
    .limit(1);
  const role = rows[0]?.role;
  return role ? { role: role as "owner" | "reviewer" | "member" } : undefined;
}

// ── FENCE 3 — the last-owner lockout (role change · leave · seat removal) ───────────────────

/**
 * The canonical entitlement fragment: what delivery derives per person —
 * ((default channels − self opt-outs) ∪ member channels ∪ direct follows) − unfollows —
 * active bundles only. Callers add per-device exclusions and the has-current join as their
 * surface needs. Kept HERE so the seat-removal detach and the delivery query share one
 * predicate. Params bind as values when strings, or inline as SQL fragments (a correlated
 * column reference, for set-level consumers like the reach counts).
 */
export const entitledBundlesSql = (
  userId: string | ReturnType<typeof sql>,
  workspaceId: string | ReturnType<typeof sql>,
) => sql`
  SELECT DISTINCT src.bundle_id
  FROM (
    SELECT cb.bundle_id
    FROM web.channel c
    JOIN web.channel_bundle cb ON cb.channel_id = c.id
    WHERE c.workspace_id = ${workspaceId} AND c.is_default
      AND NOT EXISTS (
        SELECT 1 FROM web.channel_optout o
        WHERE o.channel_id = c.id AND o.user_id = ${userId}
      )
    UNION
    SELECT cb.bundle_id
    FROM web.channel_member cm
    JOIN web.channel_bundle cb ON cb.channel_id = cm.channel_id
    WHERE cm.workspace_id = ${workspaceId} AND cm.user_id = ${userId}
    UNION
    SELECT bs.bundle_id
    FROM web.bundle_subscription bs
    WHERE bs.workspace_id = ${workspaceId} AND bs.user_id = ${userId}
      AND bs.state = 'following'
  ) src
  JOIN web.bundle b ON b.id = src.bundle_id AND b.workspace_id = ${workspaceId}
  WHERE b.status = 'active'
    AND NOT EXISTS (
      SELECT 1 FROM web.bundle_subscription un
      WHERE un.user_id = ${userId} AND un.bundle_id = src.bundle_id
        AND un.state = 'unfollowed'
    )`;

export type SeatMutationRefusal = "last_owner" | "missing";

/** Lock every owner seat of the workspace — the serialization point of all three ceremonies. */
async function lockOwnerSeats(tx: Tx, workspaceId: string): Promise<string[]> {
  const rows = await tx.execute(
    sql`SELECT user_id FROM ${seat}
        WHERE workspace_id = ${workspaceId} AND role = 'owner'
        FOR UPDATE`,
  );
  return (rows.rows as { user_id: string }[]).map((r) => r.user_id);
}

/**
 * Role change, last-owner-fenced: demoting the only owner is refused under the same lock a
 * concurrent demotion would need, so two owners demoting each other serialize and one is
 * refused.
 */
export async function setSeatRole(
  actor: { userId: string; display: string },
  workspaceId: string,
  targetUserId: string,
  newRole: "owner" | "reviewer" | "member",
): Promise<SeatMutationRefusal | "ok"> {
  return await getDb().transaction(async (tx) => {
    const owners = await lockOwnerSeats(tx, workspaceId);
    if (owners.includes(targetUserId) && newRole !== "owner" && owners.length === 1) {
      await auditInTx(tx, {
        workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "role_change",
        subject: targetUserId,
        outcome: "denied",
        details: { reason: "last_owner", newRole },
      });
      return "last_owner";
    }
    const updated = await tx.execute(
      sql`UPDATE ${seat} SET role = ${newRole}
          WHERE workspace_id = ${workspaceId} AND user_id = ${targetUserId}
          RETURNING user_id`,
    );
    if (updated.rows.length === 0) {
      return "missing";
    }
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "role_change",
      subject: targetUserId,
      outcome: "ok",
      details: { newRole },
    });
    return "ok";
  });
}

/**
 * Seat removal (an owner removing a member, or self-service leave): last-owner-fenced, and
 * delivery ends IN THIS REQUEST — the seat delete cascades memberships, subscriptions, and
 * opt-outs, and the detach RECORDS (cause per the ceremony) are written first, for exactly
 * the bundles the person was being delivered (entitled ∧ current-holding), so the fleet page
 * can chase the copies their devices keep. Re-invite starts clean by construction.
 */
export async function removeSeat(
  actor: { userId: string; display: string },
  workspaceId: string,
  targetUserId: string,
  cause: "membership_removed" | "channel_leave",
): Promise<SeatMutationRefusal | "ok"> {
  return await getDb().transaction(async (tx) => {
    const owners = await lockOwnerSeats(tx, workspaceId);
    if (owners.includes(targetUserId) && owners.length === 1) {
      await auditInTx(tx, {
        workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: targetUserId === actor.userId ? "leave" : "member_removed",
        subject: targetUserId,
        outcome: "denied",
        details: { reason: "last_owner" },
      });
      return "last_owner";
    }
    await tx.execute(
      sql`INSERT INTO ${bundleDetachment} (user_id, workspace_id, bundle_id, cause)
          SELECT ${targetUserId}, ${workspaceId}, e.bundle_id, ${cause}
          FROM (${entitledBundlesSql(targetUserId, workspaceId)}) e
          WHERE EXISTS (
            SELECT 1 FROM plane.current_pointer cp
            WHERE cp.workspace_id = ${workspaceId} AND cp.bundle_id = e.bundle_id
          )
          ON CONFLICT (user_id, bundle_id) DO NOTHING`,
    );
    const deleted = await tx.execute(
      sql`DELETE FROM ${seat}
          WHERE workspace_id = ${workspaceId} AND user_id = ${targetUserId}
          RETURNING user_id`,
    );
    if (deleted.rows.length === 0) {
      return "missing";
    }
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: targetUserId === actor.userId ? "leave" : "member_removed",
      subject: targetUserId,
      outcome: "ok",
    });
    return "ok";
  });
}
