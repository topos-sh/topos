import { randomBytes } from "node:crypto";
import { appendFileSync } from "node:fs";
import { eq, sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import { serverEnv } from "@/env.server";
import { type Db, getDb, isUniqueViolation } from "./index.server";
import { auditEvent, channel, cliSession, loginFlow, seat, workspace } from "./schema.app";

/**
 * The identity ceremonies' data layer: first-boot setup, the claim-code consume, the
 * gh-style LOGIN flow (approve + session mint), and the last-owner-fenced seat mutations.
 * These are the concurrency-critical writes of the identity model — each fence is ONE
 * transaction, FOR UPDATE-locked or single-statement-atomic, with its audit row emitted
 * inside the same transaction.
 *
 * A SESSION is user × workspace × installation: minted by `topos login <workspace-address>`
 * through the browser approval, carrying ONE workspace-scoped bearer credential. Sessions are
 * revocable from BOTH sides and DELETED, never tombstoned — history is the cause-tagged
 * audit trail.
 *
 * Secrets are HASH-STORED, and the hashing happens IN Postgres (the built-in SHA-256 over the
 * UTF-8 bytes) — this tier generates randomness but never computes a digest itself. A
 * presented code or credential is matched by `sha256(convert_to($x, 'UTF8'))`; the plaintext
 * never lands in a table, a log, or an error.
 */

// ── Id + code minting ────────────────────────────────────────────────────────────────────────

/** Opaque row ids keep their historical wire shapes (w_…, s_… are frozen wire facts). */
export function mintWorkspaceId(): string {
  return `w_${randomBytes(16).toString("hex")}`;
}
export function mintBundleId(): string {
  return `s_${randomBytes(16).toString("hex")}`;
}
export function mintChannelId(): string {
  return `c_${randomBytes(16).toString("hex")}`;
}
export function mintSessionId(): string {
  return `sn_${randomBytes(16).toString("hex")}`;
}
export function mintInvitationId(): string {
  return `inv_${randomBytes(16).toString("hex")}`;
}
export function mintProposalId(): string {
  return `p_${randomBytes(16).toString("hex")}`;
}

/** A high-entropy single-use secret (claim codes, login-flow codes): 32 random bytes, base64url. */
function mintSecret(): string {
  return randomBytes(32).toString("base64url");
}

/**
 * The short human code the login flow shows ("open /verify and enter AB29-CD34"): eight
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
  sessionId?: string;
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
    actorSessionId: args.actor.sessionId,
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

/**
 * The ceremony-lane read: the workspace row an OPAQUE ID names (null on a miss). For callers
 * whose authorization is the ceremony row itself — the granted login poll decorates from the
 * approval-persisted id — so no actor scope applies (contrast the DAL's actor-first
 * `workspaceById`).
 */
export async function workspaceRowById(id: string): Promise<typeof workspace.$inferSelect | null> {
  const rows = await getDb().select().from(workspace).where(eq(workspace.id, id)).limit(1);
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

// ── Sessions (user × workspace × installation) ──────────────────────────────────────────────

export type SessionStatus = "active" | "pending";

/**
 * THE born-status rule, written once: a session minted by an act of a seated member is born
 * 'active' when the person is an OWNER (the owner's act is its own approval, regardless of
 * the knob); otherwise the workspace's session-approval knob decides — 'off' → 'active',
 * 'on' → 'pending'. Invitation-woven logins get NO exception.
 */
export function sessionBornStatus(
  role: "owner" | "reviewer" | "member",
  knob: "off" | "on",
): SessionStatus {
  if (role === "owner") {
    return "active";
  }
  return knob === "on" ? "pending" : "active";
}

/** The workspace's session-approval knob, read inside the caller's transaction. */
async function sessionApprovalKnobTx(tx: Tx, workspaceId: string): Promise<"off" | "on"> {
  const rows = await tx.execute(
    sql`SELECT session_approval FROM ${workspace} WHERE id = ${workspaceId}`,
  );
  return (
    (rows.rows[0] as { session_approval: "off" | "on" } | undefined)?.session_approval ?? "off"
  );
}

/**
 * Delete a set of session rows inside the caller's transaction — the ONE ending helper every
 * revocation ceremony runs (self logout, account-page sign-out, owner remove/reject, seat
 * removal). One `session_ended` audit row per deleted session, cause-tagged; per-session
 * reported state dies by FK CASCADE; bytes already on the machine stay there.
 */
async function endSessionsTx(
  tx: Tx,
  args: {
    /** The session rows to end: every row this predicate matches. */
    where: ReturnType<typeof sql>;
    actor: AuditActor;
    cause: "self" | "owner_removed" | "owner_rejected" | "seat_removed";
  },
): Promise<{ sessionId: string; workspaceId: string }[]> {
  const deleted = await tx.execute(
    sql`DELETE FROM web.cli_session WHERE ${args.where}
        RETURNING id, workspace_id`,
  );
  const sessions = (deleted.rows as { id: string; workspace_id: string }[]).map((r) => ({
    sessionId: r.id,
    workspaceId: r.workspace_id,
  }));
  for (const s of sessions) {
    await auditInTx(tx, {
      workspaceId: s.workspaceId,
      actor: args.actor,
      kind: "session_ended",
      subject: s.sessionId,
      outcome: "ok",
      details: { cause: args.cause },
    });
  }
  return sessions;
}

/**
 * OWNER remove — a workspace owner ends any session in THEIR workspace (sessions page; the
 * route's owner guard is the gate). Kills exactly that workspace's access and nothing else
 * (the credential is workspace-scoped by construction). Bytes stay — the page copy says so.
 */
export async function ownerRemoveSession(
  actor: { userId: string; display: string },
  workspaceId: string,
  sessionId: string,
): Promise<"removed" | "unknown_session"> {
  return await getDb().transaction(async (tx) => {
    const ended = await endSessionsTx(tx, {
      where: sql`id = ${sessionId} AND workspace_id = ${workspaceId}`,
      actor: { userId: actor.userId, display: actor.display },
      cause: "owner_removed",
    });
    return ended.length > 0 ? "removed" : "unknown_session";
  });
}

/** APPROVE — an owner flips a PENDING session active (sessions page); `session_approved` audited. */
export async function approveSession(
  actor: { userId: string; display: string },
  workspaceId: string,
  sessionId: string,
): Promise<"approved" | "unknown_session"> {
  return await getDb().transaction(async (tx) => {
    const updated = await tx.execute(
      sql`UPDATE web.cli_session SET status = 'active'
          WHERE id = ${sessionId} AND workspace_id = ${workspaceId} AND status = 'pending'
          RETURNING id`,
    );
    if (updated.rows.length === 0) {
      return "unknown_session";
    }
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "session_approved",
      subject: sessionId,
      outcome: "ok",
    });
    return "approved";
  });
}

/**
 * REJECT — an owner DELETES a pending session (sessions page); `session_rejected` audited.
 * Logging in again later is allowed (the row is gone, not tombstoned).
 */
export async function rejectSession(
  actor: { userId: string; display: string },
  workspaceId: string,
  sessionId: string,
): Promise<"rejected" | "unknown_session"> {
  return await getDb().transaction(async (tx) => {
    const deleted = await tx.execute(
      sql`DELETE FROM web.cli_session
          WHERE id = ${sessionId} AND workspace_id = ${workspaceId} AND status = 'pending'
          RETURNING id`,
    );
    if (deleted.rows.length === 0) {
      return "unknown_session";
    }
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "session_rejected",
      subject: sessionId,
      outcome: "ok",
    });
    return "rejected";
  });
}

/**
 * SELF revocation from the account page — a person ends ONE of their own sessions. Self-only
 * by the WHERE clause itself: a foreign session id matches nothing, the same answer an
 * unknown one gets. Bytes already on the machine stay there.
 */
export async function revokeOwnSession(
  actor: { userId: string; display: string },
  sessionId: string,
): Promise<"revoked" | "unknown_session"> {
  return await getDb().transaction(async (tx) => {
    const ended = await endSessionsTx(tx, {
      where: sql`id = ${sessionId} AND user_id = ${actor.userId}`,
      actor: { userId: actor.userId, display: actor.display },
      cause: "self",
    });
    return ended.length > 0 ? "revoked" : "unknown_session";
  });
}

/**
 * The CLI's `topos logout <workspace>`: end the session the PRESENTED CREDENTIAL names —
 * possession of the credential is the authorization (it is the session). A retry (or an
 * already-ended session) matches nothing and the route answers the uniform 404 — already
 * signed out.
 */
export async function revokeSessionByCredential(credential: string): Promise<boolean> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(
      sql`SELECT s.id, s.user_id, s.workspace_id,
                 COALESCE(NULLIF(btrim(u.name), ''), u.email) AS display
          FROM web.cli_session s
          JOIN web."user" u ON u.id = s.user_id
          WHERE s.credential_sha256 = ${sha256OfText(credential)}
          FOR UPDATE`,
    );
    const row = rows.rows[0] as
      | { id: string; user_id: string; workspace_id: string; display: string }
      | undefined;
    if (!row) {
      return false;
    }
    await endSessionsTx(tx, {
      where: sql`id = ${row.id}`,
      actor: { userId: row.user_id, display: row.display },
      cause: "self",
    });
    return true;
  });
}

// ── The gh-style login flow ──────────────────────────────────────────────────────────────────

const LOGIN_FLOW_TTL_MS = 15 * 60 * 1000;
export const LOGIN_FLOW_POLL_INTERVAL_SECS = 5;
export const LOGIN_FLOW_EXPIRES_IN_SECS = LOGIN_FLOW_TTL_MS / 1000;

/**
 * Start a login flow: mint the pair of codes and park the pending row. The flow_code is the
 * CLI's polling secret — and, on approval, it is PROMOTED to the session's one bearer
 * credential (same plaintext, same stored hash shape), which is what lets the hash-only store
 * still "deliver" the credential on the poll: the poller already holds it. The short
 * user_code is what a human types at /verify; the partial unique index keeps it unambiguous
 * among PENDING rows, so minting retries on that one conflict.
 *
 * `requestedWorkspace` is the workspace ADDRESS SLUG the login named — recorded, not
 * resolved: the flow's workspace is looked up (and the approver's seat in it required) at
 * approval time, inside the approve/deny fence. A login mints ONE workspace's session;
 * further workspaces are further logins.
 */
export async function startLoginFlow(
  requestedName: string,
  requestedWorkspace: string,
  /** The invite-link token a `topos login <invite-url>` carries — hashed and RECORDED, never
   * validated here (the unauthenticated start must not be a token oracle); the approval
   * resolves it under its own fence. */
  inviteToken?: string,
): Promise<{ flowCode: string; userCode: string; expiresInSecs: number }> {
  const db = getDb();
  // Opportunistic reap: every new login first clears expired ceremony rows (there is no
  // separate scheduler), which also frees any expired pending user_code for reuse. Only
  // past-TTL rows go, so a live grant awaiting its idempotent re-poll is never touched.
  await sweepExpiredLoginFlows();
  const flowCode = mintSecret();
  const expiresAt = new Date(Date.now() + LOGIN_FLOW_TTL_MS);
  for (let attempt = 0; attempt < 5; attempt++) {
    const userCode = mintUserCode();
    try {
      await db.insert(loginFlow).values({
        id: `lf_${randomBytes(16).toString("hex")}`,
        userCode,
        flowCodeSha256: sql`${sha256OfText(flowCode)}` as never,
        requestedName,
        requestedWorkspace,
        ...(inviteToken === undefined
          ? {}
          : { inviteTokenSha256: sql`${sha256OfText(inviteToken)}` as never }),
        expiresAt,
      });
      return { flowCode, userCode, expiresInSecs: LOGIN_FLOW_EXPIRES_IN_SECS };
    } catch (error) {
      if (isUniqueViolation(error) && attempt < 4) {
        continue; // a live pending row already shows this user_code — mint another
      }
      throw error;
    }
  }
  throw new Error("login flow start: user_code space exhausted");
}

/** The first-destination hint an accepted invitation carried, decorated onto a granted poll
 * (`kind` is the bundle catalog's own tag — 'skill' today — or the literal 'channel'). */
export interface LoginGrantHint {
  kind: string;
  name: string;
}

export type LoginPollResult =
  | { status: "pending" }
  | { status: "denied" }
  | { status: "expired" }
  | {
      status: "granted";
      sessionId: string;
      /** The session's born status — 'pending' delivers nothing until an owner approves. */
      sessionStatus: SessionStatus;
      /** The workspace id the APPROVAL resolved (persisted inside its fence) — the token
       * route's `workspace` decoration reads this immutable id, so a slug rename or a
       * delete+recreate inside the TTL can never re-point a granted flow. */
      approvedWorkspaceId: string | null;
      /** The invitation hint, when the flow carried a token whose invitation names one. */
      hint: LoginGrantHint | null;
    };

/**
 * The CLI's poll, keyed by the flow_code hash. IDEMPOTENT by design: a terminal answer
 * (granted / denied) repeats on every poll until the row is swept, because the client's
 * crash-recovery is to re-poll — a CLI that received `granted` but crashed before persisting
 * its credential re-polls the same code and must get the same `granted` again (the credential
 * is the presented flow_code, echoed by the route, so re-delivery costs nothing). Terminal
 * rows are reaped by [`sweepExpiredLoginFlows`], not on read, so the grant survives its whole
 * TTL. A missing row (already swept, or never existed) reads as expired.
 */
export async function pollLoginFlow(flowCode: string): Promise<LoginPollResult> {
  const rows = await getDb().execute(
    sql`SELECT f.status, f.session_id, f.approved_workspace_id, f.invite_token_sha256,
               f.expires_at < now() AS expired, s.status AS session_status
        FROM ${loginFlow} f
        LEFT JOIN web.cli_session s ON s.id = f.session_id
        WHERE f.flow_code_sha256 = ${sha256OfText(flowCode)}`,
  );
  const row = rows.rows[0] as
    | {
        status: string;
        session_id: string | null;
        approved_workspace_id: string | null;
        invite_token_sha256: Buffer | null;
        expired: boolean;
        session_status: SessionStatus | null;
      }
    | undefined;
  if (!row) {
    return { status: "expired" };
  }
  if (row.status === "denied") {
    return { status: "denied" };
  }
  if (row.status === "approved") {
    // A granted flow stays granted while its SESSION lives (the approve minted it). A session
    // ended between approval and this poll (owner reject, revocation) reads as expired — the
    // credential is dead, so "start over" is the honest answer.
    if (row.session_id === null || row.session_status === null) {
      return { status: "expired" };
    }
    return {
      status: "granted",
      sessionId: row.session_id,
      sessionStatus: row.session_status,
      approvedWorkspaceId: row.approved_workspace_id,
      hint:
        row.invite_token_sha256 === null ? null : await inviteHintByHash(row.invite_token_sha256),
    };
  }
  // pending — expired pending is terminal (the human never approved in time).
  return row.expired ? { status: "expired" } : { status: "pending" };
}

/**
 * The first-destination hint of the invitation a token hash names — ANY status (a granted
 * flow's invitation was consumed by its own approval), the hinted thing resolved to its
 * display name, active bundles only. The token hash is retained on the row for exactly this
 * read.
 */
async function inviteHintByHash(tokenSha256: Buffer): Promise<LoginGrantHint | null> {
  const rows = await getDb().execute(
    sql`SELECT b.kind AS bundle_kind, b.name AS bundle_name, c.name AS channel_name
        FROM web.invitation i
        LEFT JOIN web.bundle b ON b.id = i.hint_bundle_id AND b.status = 'active'
        LEFT JOIN web.channel c ON c.id = i.hint_channel_id
        WHERE i.token_sha256 = ${tokenSha256}`,
  );
  const row = rows.rows[0] as
    | { bundle_kind: string | null; bundle_name: string | null; channel_name: string | null }
    | undefined;
  if (!row) {
    return null;
  }
  if (row.bundle_name !== null) {
    return { kind: row.bundle_kind ?? "skill", name: row.bundle_name };
  }
  if (row.channel_name !== null) {
    return { kind: "channel", name: row.channel_name };
  }
  return null;
}

/**
 * Reap login-flow ceremony rows past their TTL — a periodic sweep (the app's maintenance
 * loop), NOT a read-time delete, so an idempotent re-poll of a fresh grant always finds it.
 * A grant the client already consumed is harmless to keep until expiry (the credential is
 * live regardless); this only bounds the table.
 */
export async function sweepExpiredLoginFlows(): Promise<number> {
  const result = await getDb().execute(sql`DELETE FROM ${loginFlow} WHERE expires_at < now()`);
  return result.rowCount ?? 0;
}

/**
 * Resolve a locked flow's workspace AND the acting person's seat in it, inside the caller's
 * approve/deny transaction. The tenancy grammar decides the lookup: single-tenant flows
 * resolve to the install's one workspace whatever slug they recorded; multi-tenant flows
 * resolve the recorded slug by name — which may have been created AFTER the flow started
 * (a CLI-first person creates the workspace mid-flow and returns to approve). A missing
 * workspace or a seatless actor both resolve to null, and the caller answers the same
 * uniform refusal — a non-member learns nothing, not even that the workspace exists.
 */
async function seatedFlowWorkspaceTx(
  tx: Tx,
  requestedWorkspace: string,
  actorUserId: string,
): Promise<{ workspaceId: string; role: "owner" | "reviewer" | "member" } | null> {
  const rows =
    composition.tenancy === "multi"
      ? await tx.execute(
          sql`SELECT id FROM ${workspace} WHERE name = ${requestedWorkspace} LIMIT 1`,
        )
      : await tx.execute(sql`SELECT id FROM ${workspace} LIMIT 1`);
  const ws = rows.rows[0] as { id: string } | undefined;
  if (!ws) {
    return null;
  }
  // FOR UPDATE: the seat is the authorization — lock it so a concurrent seat removal
  // serializes with this ceremony instead of racing it (no approve/deny commits on a seat
  // whose delete already committed).
  const seats = await tx.execute(
    sql`SELECT role FROM ${seat} WHERE workspace_id = ${ws.id} AND user_id = ${actorUserId}
        FOR UPDATE`,
  );
  const seatRow = seats.rows[0] as { role: "owner" | "reviewer" | "member" } | undefined;
  if (seatRow === undefined) {
    return null;
  }
  return { workspaceId: ws.id, role: seatRow.role };
}

/**
 * FENCE 2 — the login-flow approve + session mint, one FOR UPDATE transaction: lock the
 * pending row by user_code, re-check liveness under the lock, resolve the flow's workspace
 * (by the recorded slug under the tenancy grammar) and require the approver's SEAT in it,
 * mint the SESSION row (user × workspace × installation; credential hash = the flow_code
 * hash; born per the ONE rule), and flip the row to approved. An unresolvable workspace or a
 * seatless approver returns null — the same answer an expired code gets, so the ceremony is
 * no existence or membership oracle. The approver's browser-session gate runs in the ROUTE
 * before this is called — approval mints a credential that acts as you, in this ONE
 * workspace.
 */
/** The in-transaction abort sentinel: an approval that cannot complete must ROLL BACK any
 * invitation accept it already made (a bare `return null` from a Drizzle transaction COMMITS —
 * only a throw rolls back). Thrown inside the fence, caught at the boundary → the uniform null.
 */
const APPROVE_ABORT = Symbol("login-approve-abort");

export async function approveLoginFlow(
  userCode: string,
  approver: { userId: string; display: string },
): Promise<{ sessionId: string; requestedName: string; sessionStatus: SessionStatus } | null> {
  try {
    return await getDb().transaction(async (tx) => {
      const rows = await tx.execute(
        sql`SELECT id, requested_name, requested_workspace, flow_code_sha256,
                   invite_token_sha256
            FROM ${loginFlow}
            WHERE user_code = ${userCode} AND status = 'pending' AND expires_at > now()
            FOR UPDATE`,
      );
      const row = rows.rows[0] as
        | {
            id: string;
            requested_name: string;
            requested_workspace: string;
            flow_code_sha256: Buffer;
            invite_token_sha256: Buffer | null;
          }
        | undefined;
      if (!row) {
        return null;
      }
      // The invitation weave: a flow that carries an invite token accepts the invitation INSIDE
      // this same fence when the approver is its rightful addressee — so a typed-code approval
      // (never having visited the invitation page) still lands sign-in → accept → approve as one
      // act, and the seat requirement below then finds the seat the accept just wrote. A token
      // that resolves to nothing, or an approver the accept fences refuse (wrong account,
      // unverified mailbox), seats nothing.
      let acceptedWorkspaceId: string | null = null;
      if (row.invite_token_sha256 !== null) {
        const inv = await lockPendingInvitationTx(
          tx,
          sql`i.token_sha256 = ${row.invite_token_sha256}`,
        );
        if (inv !== null) {
          const outcome = await acceptInvitationTx(tx, inv, await sessionAccountTx(tx, approver), {
            mailboxProven: false,
          });
          if (outcome.outcome === "accepted") {
            acceptedWorkspaceId = outcome.workspaceId;
          }
        }
      }
      const resolved = await seatedFlowWorkspaceTx(tx, row.requested_workspace, approver.userId);
      // The seat is the sole authority — a seatless approver's approval cannot complete. But an
      // invite-weave accept may have already seated + consumed inside this tx, so a bare return
      // would COMMIT that while the poll reports refused: throw to roll it back instead.
      if (resolved === null) {
        throw APPROVE_ABORT;
      }
      // Consistency: an accepted invitation must be for the SAME workspace this approval
      // resolves to — otherwise the accept seated + consumed in a workspace the session is not
      // being minted toward (a crafted flow: invite for A, requested_workspace naming B).
      // Roll the whole thing back rather than commit a split-brain login.
      if (acceptedWorkspaceId !== null && acceptedWorkspaceId !== resolved.workspaceId) {
        throw APPROVE_ABORT;
      }
      const sessionId = mintSessionId();
      const born = sessionBornStatus(
        resolved.role,
        await sessionApprovalKnobTx(tx, resolved.workspaceId),
      );
      await tx.insert(cliSession).values({
        id: sessionId,
        workspaceId: resolved.workspaceId,
        userId: approver.userId,
        displayName: row.requested_name,
        credentialSha256: row.flow_code_sha256,
        status: born,
      });
      await tx.execute(
        sql`UPDATE ${loginFlow}
            SET status = 'approved', approved_by = ${approver.userId}, session_id = ${sessionId},
                approved_workspace_id = ${resolved.workspaceId}
            WHERE id = ${row.id}`,
      );
      await auditInTx(tx, {
        workspaceId: resolved.workspaceId,
        actor: { userId: approver.userId, display: approver.display },
        kind: "session_created",
        subject: sessionId,
        outcome: "ok",
        details: { requestedName: row.requested_name, status: born },
      });
      return { sessionId, requestedName: row.requested_name, sessionStatus: born };
    });
  } catch (error) {
    // The clean-refusal rollback surfaces as the uniform null (the same answer an expired code
    // gets); any other error is a real fault and propagates.
    if (error === APPROVE_ABORT) {
      return null;
    }
    throw error;
  }
}

/**
 * The verify page's deny arm — same lock discipline AND the same workspace + seat
 * requirement as the approve (a person who cannot approve a flow cannot destroy it either;
 * an unresolvable flow dies by its TTL), terminal 'denied'.
 */
export async function denyLoginFlow(
  userCode: string,
  denier: { userId: string; display: string },
): Promise<boolean> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(
      sql`SELECT id, requested_name, requested_workspace FROM ${loginFlow}
          WHERE user_code = ${userCode} AND status = 'pending' AND expires_at > now()
          FOR UPDATE`,
    );
    const row = rows.rows[0] as
      | { id: string; requested_name: string; requested_workspace: string }
      | undefined;
    if (!row) {
      return false;
    }
    const resolved = await seatedFlowWorkspaceTx(tx, row.requested_workspace, denier.userId);
    if (resolved === null) {
      return false;
    }
    await tx.execute(sql`UPDATE ${loginFlow} SET status = 'denied' WHERE id = ${row.id}`);
    await auditInTx(tx, {
      workspaceId: resolved.workspaceId,
      actor: { userId: denier.userId, display: denier.display },
      kind: "login_denied",
      subject: row.requested_name,
      outcome: "ok",
    });
    return true;
  });
}

/** The verify page's resolved request: what is asking, the code for the glance-check, and —
 * when the flow carries an invite token that still resolves — the workspace the invitation
 * would join (disclosed to the code-holder, who is the token-holder's own terminal). The
 * invitation's role rides along so the approval copy can say honestly whether the session
 * will await an owner. */
export interface PendingLoginFlowView {
  requestedName: string;
  requestedWorkspace: string;
  userCode: string;
  inviteWorkspace: { name: string; displayName: string; role: string } | null;
}

/** The verify page's lookup: the pending request a typed user_code names (display only). */
export async function pendingLoginFlow(userCode: string): Promise<PendingLoginFlowView | null> {
  return pendingLoginFlowWhere(sql`user_code = ${userCode}`);
}

/**
 * The loopback auto-open's lookup: the pending request whose flow-code HASH the CLI put in
 * the URL it opened (hex of the same SHA-256 this store already keys the row by — the code
 * itself never enters a URL; a preimage is infeasible, so the challenge identifies without
 * revealing). A malformed challenge is simply a miss.
 */
export async function pendingLoginFlowByChallenge(
  challengeHex: string,
): Promise<PendingLoginFlowView | null> {
  if (!/^[0-9a-f]{64}$/.test(challengeHex)) {
    return null;
  }
  return pendingLoginFlowWhere(sql`flow_code_sha256 = decode(${challengeHex}, 'hex')`);
}

async function pendingLoginFlowWhere(
  cond: ReturnType<typeof sql>,
): Promise<PendingLoginFlowView | null> {
  const rows = await getDb().execute(
    sql`SELECT f.requested_name, f.requested_workspace, f.user_code,
               w.name AS invite_ws_name, w.display_name AS invite_ws_display,
               i.role AS invite_role
        FROM ${loginFlow} f
        LEFT JOIN web.invitation i ON i.token_sha256 = f.invite_token_sha256
          AND i.status = 'pending' AND (i.expires_at IS NULL OR i.expires_at > now())
        LEFT JOIN ${workspace} w ON w.id = i.workspace_id
        WHERE ${cond} AND f.status = 'pending' AND f.expires_at > now()`,
  );
  const row = rows.rows[0] as
    | {
        requested_name: string;
        requested_workspace: string;
        user_code: string;
        invite_ws_name: string | null;
        invite_ws_display: string | null;
        invite_role: string | null;
      }
    | undefined;
  if (!row) {
    return null;
  }
  return {
    requestedName: row.requested_name,
    requestedWorkspace: row.requested_workspace,
    userCode: row.user_code,
    inviteWorkspace:
      row.invite_ws_name === null
        ? null
        : {
            name: row.invite_ws_name,
            displayName: row.invite_ws_display ?? row.invite_ws_name,
            role: row.invite_role ?? "member",
          },
  };
}

// ── The session lane's actor resolve ────────────────────────────────────────────────────────

export interface SessionActorRow {
  sessionId: string;
  userId: string;
  userDisplay: string;
  role: "owner" | "reviewer" | "member";
  /** The session's status — a LIVE row is standing; 'active' is authorization. */
  sessionStatus: SessionStatus;
}

/**
 * credential-hash → live session → seat, one query, fail-closed: an ended session, a
 * mismatched workspace (the credential is WORKSPACE-SCOPED — presenting it against another
 * workspace's path is a miss), a seatless user, or an expired session (the workspace's
 * session_max_age_ms policy, checked at guard time) all resolve to nothing (the route answers
 * the uniform wire 404 — NO row is byte-indistinguishable from a workspace that never
 * existed). A PENDING session resolves WITH its status: exactly two routes answer typed for
 * it (the guard folds everything else to the 404). The hash is computed in Postgres;
 * last_seen_at rides along.
 */
export async function sessionActor(
  workspaceId: string,
  credential: string,
): Promise<SessionActorRow | null> {
  const rows = await getDb().execute(
    sql`UPDATE web.cli_session cs SET last_seen_at = now()
        FROM ${seat} s, web."user" u, ${workspace} w
        WHERE cs.credential_sha256 = ${sha256OfText(credential)}
          AND cs.workspace_id = ${workspaceId}
          AND w.id = cs.workspace_id
          AND (w.session_max_age_ms IS NULL
               OR cs.created_at > now() - make_interval(secs => w.session_max_age_ms / 1000.0))
          AND u.id = cs.user_id
          AND s.user_id = cs.user_id AND s.workspace_id = cs.workspace_id
        RETURNING cs.id AS session_id, cs.user_id,
          -- The display rule (app/lib/person-display.ts): a blank name falls back to the email.
          COALESCE(NULLIF(btrim(u.name), ''), u.email) AS user_display, s.role,
          cs.status AS session_status`,
  );
  const row = rows.rows[0] as
    | {
        session_id: string;
        user_id: string;
        user_display: string;
        role: SessionActorRow["role"];
        session_status: SessionStatus;
      }
    | undefined;
  if (!row) {
    return null;
  }
  return {
    sessionId: row.session_id,
    userId: row.user_id,
    userDisplay: row.user_display,
    role: row.role,
    sessionStatus: row.session_status,
  };
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

// ── The tokened invitation ceremonies (view · accept · decline) ─────────────────────────────
//
// The invite LINK is worth one invitation, never an account or a credential: viewing never
// consumes (GET-safe for scanners), and the accept binds to the INVITED EMAIL's account — the
// one sanctioned email comparison beside bindInvitedSeats above. Only the token's SHA-256 is
// stored (the claim-code pattern); the plaintext travels in the invitation mail alone.

/** Mint the single-use invite-link token (32 random bytes, base64url — URL-path-safe). The
 * caller stores only its hash; the plaintext goes into the mailed link. */
export function mintInviteToken(): string {
  return mintSecret();
}

/**
 * Supersede a DECLINED invitation record for an address being re-invited, inside the
 * inviter's transaction — the audit trail keeps the permanent record; the members page stays
 * clean. Email-keyed BY DESIGN (the same key the pending upsert conflicts on), which is why
 * this row op lives here in the sanctioned module and not in the DAL.
 */
export async function supersedeDeclinedInvitationTx(
  tx: Tx,
  workspaceId: string,
  email: string,
): Promise<void> {
  await tx.execute(
    sql`DELETE FROM web.invitation
        WHERE workspace_id = ${workspaceId} AND email = ${email} AND status = 'declined'`,
  );
}

/** What the invitation page shows BEFORE accept: who invited, where to, the role, and what it
 * delivers. Resolved only for a live (pending, unexpired) token — every other state is the one
 * constant page, so nothing here leaks on a miss. */
export interface InvitationView {
  workspaceId: string;
  workspaceName: string;
  workspaceDisplayName: string;
  /** The invited address (shown to the token-holder — the mailbox the link was sent to). */
  email: string;
  role: string;
  inviterDisplay: string | null;
  /** The first-destination hint (`kind` = the bundle catalog's tag, or 'channel'). */
  hint: LoginGrantHint | null;
  /** Active bundles the default channel delivers to every member — the pre-accept summary. */
  deliveredCount: number;
  /** The default channels delivering them (usually just 'everyone'). */
  viaChannels: string[];
}

/** The invitation a live token names, for the pre-accept summary. Null = the constant page. */
export async function invitationByToken(token: string): Promise<InvitationView | null> {
  const rows = await getDb().execute(
    sql`SELECT i.workspace_id, i.email, i.role, w.name, w.display_name,
               COALESCE(NULLIF(btrim(u.name), ''), u.email) AS inviter_display,
               b.kind AS bundle_kind, b.name AS bundle_name, c.name AS channel_name
        FROM web.invitation i
        JOIN ${workspace} w ON w.id = i.workspace_id
        LEFT JOIN web."user" u ON u.id = i.invited_by
        LEFT JOIN web.bundle b ON b.id = i.hint_bundle_id AND b.status = 'active'
        LEFT JOIN web.channel c ON c.id = i.hint_channel_id
        WHERE i.token_sha256 = ${sha256OfText(token)} AND i.status = 'pending'
          AND (i.expires_at IS NULL OR i.expires_at > now())`,
  );
  const row = rows.rows[0] as
    | {
        workspace_id: string;
        email: string;
        role: string;
        name: string;
        display_name: string;
        inviter_display: string | null;
        bundle_kind: string | null;
        bundle_name: string | null;
        channel_name: string | null;
      }
    | undefined;
  if (!row) {
    return null;
  }
  const delivered = await getDb().execute(
    sql`SELECT c.name, count(DISTINCT cb.bundle_id)::int AS bundles
        FROM web.channel c
        JOIN web.channel_bundle cb ON cb.channel_id = c.id
        JOIN web.bundle b ON b.id = cb.bundle_id AND b.status = 'active'
        WHERE c.workspace_id = ${row.workspace_id} AND c.is_default
        GROUP BY c.name`,
  );
  const via = delivered.rows as { name: string; bundles: number }[];
  return {
    workspaceId: row.workspace_id,
    workspaceName: row.name,
    workspaceDisplayName: row.display_name,
    email: row.email,
    role: row.role,
    inviterDisplay: row.inviter_display,
    hint:
      row.bundle_name !== null
        ? { kind: row.bundle_kind ?? "skill", name: row.bundle_name }
        : row.channel_name !== null
          ? { kind: "channel", name: row.channel_name }
          : null,
    deliveredCount: via.reduce((n, r) => n + r.bundles, 0),
    viaChannels: via.map((r) => r.name),
  };
}

/**
 * Which arm the invitation page shows a visitor — decided HERE so the email-binding predicate
 * never leaves this module (the route renders branches, it compares nothing):
 *  - `anon_new` — no session, no account under the invited address: the account-minting accept;
 *  - `anon_existing` — no session, the address has an account: sign in first, then return;
 *  - `match` — signed in AS the invited address, mailbox proven: the one-click accept;
 *  - `match_unverified` — signed in as the invited address but the mailbox was never proven:
 *     one verification round-trip first (the true owner passes; a squatter cannot);
 *  - `other` — signed in as a DIFFERENT account: the switch page (never accepts as current);
 *  - `member` — signed in as the invited address AND already seated: redirect into the
 *     workspace (the loader's redirect; nothing consumed on a GET).
 */
export type InvitationPageBranch =
  | "anon_new"
  | "anon_existing"
  | "match"
  | "match_unverified"
  | "other"
  | "member";

/** The invitation page's whole server-side read: the view + the visitor's branch. */
export async function invitationPageView(
  token: string,
  sessionUserId: string | null,
): Promise<{ view: InvitationView; branch: InvitationPageBranch } | null> {
  const view = await invitationByToken(token);
  if (view === null) {
    return null;
  }
  if (sessionUserId === null) {
    const rows = await getDb().execute(
      sql`SELECT 1 FROM web."user" WHERE lower(email) = ${view.email} LIMIT 1`,
    );
    return { view, branch: rows.rows.length > 0 ? "anon_existing" : "anon_new" };
  }
  const rows = await getDb().execute(
    sql`SELECT email, email_verified FROM web."user" WHERE id = ${sessionUserId}`,
  );
  const row = rows.rows[0] as { email: string; email_verified: boolean } | undefined;
  if (!row || row.email.trim().toLowerCase() !== view.email) {
    return { view, branch: "other" };
  }
  const seated = await seatOf(sessionUserId, view.workspaceId);
  if (seated !== undefined) {
    return { view, branch: "member" };
  }
  return { view, branch: row.email_verified ? "match" : "match_unverified" };
}

/** The session account an accept fences against — email + its verification state alongside the
 * branded actor facts. Resolved server-side from the user id, never from a form. */
export interface SessionAccount {
  userId: string;
  display: string;
  email: string;
  emailVerified: boolean;
}

/** Read the acting account's email facts inside the caller's transaction (a deleted user reads
 * as an empty account no invitation can match — fail-closed). */
async function sessionAccountTx(
  tx: Tx,
  actor: { userId: string; display: string },
): Promise<SessionAccount> {
  const rows = await tx.execute(
    sql`SELECT email, email_verified FROM web."user" WHERE id = ${actor.userId}`,
  );
  const row = rows.rows[0] as { email: string; email_verified: boolean } | undefined;
  return {
    userId: actor.userId,
    display: actor.display,
    email: row?.email ?? "",
    emailVerified: row?.email_verified ?? false,
  };
}

/** The row an accept/decline fence locks. */
interface LockedInvitation {
  id: string;
  workspaceId: string;
  email: string;
  role: string;
  invitedBy: string | null;
  hintBundleId: string | null;
  hintChannelId: string | null;
}

/** Lock ONE live (pending, unexpired) invitation row by an arbitrary predicate — the shared
 * FOR-UPDATE fence of accept, decline, and the login-approval weave. */
async function lockPendingInvitationTx(
  tx: Tx,
  cond: ReturnType<typeof sql>,
): Promise<LockedInvitation | null> {
  const rows = await tx.execute(
    sql`SELECT i.id, i.workspace_id, i.email, i.role, i.invited_by,
               i.hint_bundle_id, i.hint_channel_id
        FROM web.invitation i
        WHERE ${cond} AND i.status = 'pending'
          AND (i.expires_at IS NULL OR i.expires_at > now())
        FOR UPDATE`,
  );
  const row = rows.rows[0] as
    | {
        id: string;
        workspace_id: string;
        email: string;
        role: string;
        invited_by: string | null;
        hint_bundle_id: string | null;
        hint_channel_id: string | null;
      }
    | undefined;
  if (!row) {
    return null;
  }
  return {
    id: row.id,
    workspaceId: row.workspace_id,
    email: row.email,
    role: row.role,
    invitedBy: row.invited_by,
    hintBundleId: row.hint_bundle_id,
    hintChannelId: row.hint_channel_id,
  };
}

export type InviteAcceptOutcome =
  /** Consumed + seated (or already seated) — the landing facts ride along. */
  | {
      outcome: "accepted";
      workspaceId: string;
      workspaceName: string;
      workspaceDisplayName: string;
      hint: LoginGrantHint | null;
      alreadyMember: boolean;
    }
  /** No live invitation under this token — the one constant page. */
  | { outcome: "gone" }
  /** The session account is not the invited address — the switch page; never accepts. */
  | { outcome: "wrong_account" }
  /** The invited address's account never proved its mailbox — one round-trip first (the true
   * owner passes; a squatter cannot). */
  | { outcome: "unverified" };

/**
 * FENCE — the invitation accept, ONE transaction beside bindInvitedSeats: the email-binding
 * predicate, the unverified-squat fence, consume the row, write the seat, apply the hint
 * effects AFTER the seat (the seat-anchoring invariant: a profile line without the workspace
 * stays unrepresentable), audit — all under the caller's FOR-UPDATE lock on the invitation
 * row, so two racing accepts serialize and exactly one consumes.
 *
 * The HINT PREFILLS the newcomer's profile — an include row for the hinted bundle or channel.
 * Nothing lands on any machine from a web accept: bytes flow only when a session's reconcile
 * next runs.
 *
 * `mailboxProven` marks the account-minting path, where possession of the mailed token IS the
 * mailbox proof: the fence is satisfied and the account's email_verified flips true here.
 */
async function acceptInvitationTx(
  tx: Tx,
  inv: LockedInvitation,
  account: SessionAccount,
  opts: { mailboxProven: boolean },
): Promise<InviteAcceptOutcome> {
  if (account.email.trim().toLowerCase() !== inv.email) {
    return { outcome: "wrong_account" };
  }
  if (!account.emailVerified && !opts.mailboxProven) {
    return { outcome: "unverified" };
  }
  if (opts.mailboxProven && !account.emailVerified) {
    await tx.execute(sql`UPDATE web."user" SET email_verified = true WHERE id = ${account.userId}`);
  }
  await tx.execute(
    sql`UPDATE web.invitation
        SET status = 'accepted', accepted_by = ${account.userId}, accepted_at = now()
        WHERE id = ${inv.id}`,
  );
  const seated = await tx.execute(
    sql`INSERT INTO ${seat} (workspace_id, user_id, role, invited_by)
        VALUES (${inv.workspaceId}, ${account.userId}, ${inv.role}, ${inv.invitedBy})
        ON CONFLICT (workspace_id, user_id) DO NOTHING
        RETURNING user_id`,
  );
  const alreadyMember = seated.rows.length === 0;
  // Hint effects — AFTER the seat row, same transaction: PREFILL the newcomer's profile. The
  // hinted thing may have been deleted since the invite (the FK cleared the column) or
  // archived; then nothing lands.
  let hint: LoginGrantHint | null = null;
  if (inv.hintBundleId !== null) {
    const named = await tx.execute(
      sql`SELECT kind, name FROM web.bundle
          WHERE id = ${inv.hintBundleId} AND workspace_id = ${inv.workspaceId}
            AND status = 'active'`,
    );
    const row = named.rows[0] as { kind: string; name: string } | undefined;
    if (row) {
      await tx.execute(
        sql`INSERT INTO web.profile_entry (workspace_id, user_id, mode, bundle_id)
            VALUES (${inv.workspaceId}, ${account.userId}, 'include', ${inv.hintBundleId})
            ON CONFLICT (user_id, bundle_id) WHERE bundle_id is not null
            DO UPDATE SET mode = 'include', updated_at = now()`,
      );
      hint = { kind: row.kind, name: row.name };
    }
  } else if (inv.hintChannelId !== null) {
    const named = await tx.execute(
      sql`SELECT name FROM web.channel
          WHERE id = ${inv.hintChannelId} AND workspace_id = ${inv.workspaceId}`,
    );
    const row = named.rows[0] as { name: string } | undefined;
    if (row) {
      await tx.execute(
        sql`INSERT INTO web.profile_entry (workspace_id, user_id, mode, channel_id)
            VALUES (${inv.workspaceId}, ${account.userId}, 'include', ${inv.hintChannelId})
            ON CONFLICT (user_id, channel_id) WHERE channel_id is not null
            DO UPDATE SET mode = 'include', updated_at = now()`,
      );
      hint = { kind: "channel", name: row.name };
    }
  }
  await auditInTx(tx, {
    workspaceId: inv.workspaceId,
    actor: { userId: account.userId, display: account.display },
    kind: "invitation_accepted",
    subject: inv.email,
    outcome: "ok",
    details: { role: inv.role, ...(hint === null ? {} : { hint }) },
  });
  const ws = await tx.execute(
    sql`SELECT name, display_name FROM ${workspace} WHERE id = ${inv.workspaceId}`,
  );
  const wsRow = ws.rows[0] as { name: string; display_name: string } | undefined;
  return {
    outcome: "accepted",
    workspaceId: inv.workspaceId,
    workspaceName: wsRow?.name ?? "",
    workspaceDisplayName: wsRow?.display_name ?? wsRow?.name ?? "",
    hint,
    alreadyMember,
  };
}

/** The invitation page's accept: lock the live row by token, run the fence. */
export async function acceptInvitationByToken(
  token: string,
  actor: { userId: string; display: string },
  opts: { mailboxProven: boolean },
): Promise<InviteAcceptOutcome> {
  return await getDb().transaction(async (tx) => {
    const inv = await lockPendingInvitationTx(tx, sql`i.token_sha256 = ${sha256OfText(token)}`);
    if (inv === null) {
      return { outcome: "gone" };
    }
    return acceptInvitationTx(tx, inv, await sessionAccountTx(tx, actor), opts);
  });
}

/**
 * Decline — recorded (the inviter sees it; re-inviting mints a fresh row), and deliberately
 * SESSION-LESS: possession of the mailed token is the same proof the account mint accepts, and
 * demanding an account to say "no thanks" would be hostile. Uniform miss otherwise.
 */
export async function declineInvitationByToken(token: string): Promise<"declined" | "gone"> {
  return await getDb().transaction(async (tx) => {
    const inv = await lockPendingInvitationTx(tx, sql`i.token_sha256 = ${sha256OfText(token)}`);
    if (inv === null) {
      return "gone";
    }
    await tx.execute(sql`UPDATE web.invitation SET status = 'declined' WHERE id = ${inv.id}`);
    await auditInTx(tx, {
      workspaceId: inv.workspaceId,
      actor: { display: inv.email },
      kind: "invitation_declined",
      subject: inv.email,
      outcome: "ok",
    });
    return "declined";
  });
}

/**
 * The passwordless account mint's bridge: park a single-use sign-in token in Better Auth's own
 * verification store, shaped exactly as the magic-link plugin's verify endpoint consumes it —
 * so the invitation page can mint the invited email's account + session THROUGH Better Auth's
 * own door (hooks included) without sending any mail: the invite token's delivery to that
 * mailbox already IS the proof. Short TTL; consumed atomically by the verify call.
 */
export async function mintInvitationSignIn(email: string): Promise<string> {
  const token = mintSecret();
  const expiresAt = new Date(Date.now() + 2 * 60 * 1000);
  await getDb().execute(
    sql`INSERT INTO web.verification (id, identifier, value, expires_at)
        VALUES (${`iv_${randomBytes(16).toString("hex")}`}, ${token},
                ${JSON.stringify({ email })}, ${expiresAt})`,
  );
  return token;
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
 * The canonical PERSON-SIDE DEMAND fragment: what the person's profile requests —
 * ((the default channel, unless the profile excludes it) ∪ included channels ∪ included
 * bundles) − excluded bundles — active bundles only. This is the demand HALF of
 * demand ∩ entitlement: the seat itself is the entitlement (whole-catalog), so delivery =
 * this set, and callers add the has-current join as their surface needs. Kept HERE so the
 * delivery query and every reach count share one predicate. Params bind as values when
 * strings, or inline as SQL fragments (a correlated column reference, for set-level
 * consumers like the reach counts).
 */
export const profileDemandSql = (
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
        SELECT 1 FROM web.profile_entry px
        WHERE px.channel_id = c.id AND px.user_id = ${userId} AND px.mode = 'exclude'
      )
    UNION
    SELECT cb.bundle_id
    FROM web.profile_entry pe
    JOIN web.channel_bundle cb ON cb.channel_id = pe.channel_id
    WHERE pe.workspace_id = ${workspaceId} AND pe.user_id = ${userId}
      AND pe.mode = 'include' AND pe.channel_id IS NOT NULL
    UNION
    SELECT pe.bundle_id
    FROM web.profile_entry pe
    WHERE pe.workspace_id = ${workspaceId} AND pe.user_id = ${userId}
      AND pe.mode = 'include' AND pe.bundle_id IS NOT NULL
  ) src
  JOIN web.bundle b ON b.id = src.bundle_id AND b.workspace_id = ${workspaceId}
  WHERE b.status = 'active'
    AND NOT EXISTS (
      SELECT 1 FROM web.profile_entry ex
      WHERE ex.user_id = ${userId} AND ex.bundle_id = src.bundle_id
        AND ex.mode = 'exclude'
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
 * delivery ends IN THIS REQUEST — the target's sessions in this workspace are ended
 * EXPLICITLY (audited, cause-tagged) before the seat delete, and the seat delete cascades
 * the person's profile away. Re-invite starts clean by construction; bytes already on their
 * machines stay there (severed machines simply stop receiving).
 */
export async function removeSeat(
  actor: { userId: string; display: string },
  workspaceId: string,
  targetUserId: string,
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
    // THE SERIALIZATION POINT with the login ceremonies: lock the TARGET's seat row BEFORE
    // ending sessions. `approveLoginFlow` locks this same row (FOR UPDATE) before minting its
    // session, so a concurrent approval either committed first — its session is visible to the
    // ending below (each statement reads a fresh snapshot) — or blocks here and finds no seat
    // after this commit (refused). The explicit session ending must PRECEDE the seat delete
    // only for the audit rows; the FK cascade would delete them silently otherwise.
    const targetSeat = await tx.execute(
      sql`SELECT 1 FROM ${seat}
          WHERE workspace_id = ${workspaceId} AND user_id = ${targetUserId}
          FOR UPDATE`,
    );
    if (targetSeat.rows.length === 0) {
      return "missing";
    }
    await endSessionsTx(tx, {
      where: sql`workspace_id = ${workspaceId} AND user_id = ${targetUserId}`,
      actor: { userId: actor.userId, display: actor.display },
      cause: "seat_removed",
    });
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
