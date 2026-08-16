import { Buffer } from "node:buffer";
import { randomBytes } from "node:crypto";
import { appendFileSync } from "node:fs";
import { eq, sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import { serverEnv } from "@/env.server";
import { type Db, getDb, isUniqueViolation } from "./index.server";
import { memberCapReachedInTx } from "./invite-caps.server";
import {
  assignment,
  auditEvent,
  channel,
  cliSession,
  loginFlow,
  seat,
  workspace,
} from "./schema.app";
// A deliberate module CYCLE with workspace-create.server.ts (it imports this module's audit +
// id mints): both sides bind at call time only, and the birth has exactly one spelling — the
// approve weave must run the identical transaction body /new runs, not a copy.
import { createWorkspaceTx } from "./workspace-create.server";

/**
 * The identity ceremonies' data layer: first-boot setup, the claim-code consume, the
 * gh-style LOGIN flow (browser approve, then the mint at the CLI's exchange), and the
 * last-owner-fenced seat mutations. These are the concurrency-critical writes of the identity
 * model — each fence is ONE transaction, FOR UPDATE-locked or single-statement-atomic, with
 * its audit row emitted inside the same transaction.
 *
 * A SESSION is user × workspace × installation: born of `topos login` — the browser approval
 * chooses (or creates) the workspace, the CLI's poll mints — carrying ONE workspace-scoped
 * bearer credential. Sessions are revocable from BOTH sides and DELETED, never tombstoned —
 * history is the cause-tagged audit trail.
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

/** Emit an audit row INSIDE the caller's transaction (append-only by code discipline).
 * `workspaceId` is null for the few SERVER-scoped events (a login deny lands before any
 * workspace is chosen); workspace-scoped readers query by equality, so a NULL row never
 * surfaces there. */
export async function auditInTx(
  tx: Tx,
  args: {
    workspaceId: string | null;
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
      const defaultChannelId = mintChannelId();
      await tx.insert(channel).values({
        id: defaultChannelId,
        workspaceId,
        name: "everyone",
        isDefault: true,
      });
      // The BASELINE, as a row: the default channel assigned to everyone. Nothing anywhere
      // treats the default channel as delivered by rule, so a workspace born without this row
      // would deliver nothing to anyone. No person has acted yet (the claim comes later), so
      // the attribution is the same 'system' the birth audit row carries.
      await tx.insert(assignment).values({
        workspaceId,
        userId: null,
        channelId: defaultChannelId,
        self: false,
        createdBy: "system",
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
 * End EVERY CLI session a person holds, in every workspace — the credential half of a password
 * change. A bearer is not reachable from the auth tier's own session store: it is a
 * `web.cli_session` row, workspace-scoped, with no default expiry, so a reset that ended only
 * browser cookies would leave the more durable credential alive on a machine the person may
 * have lost. Person-scoped by design: the act is "this account's password changed", so it is
 * not any one workspace's ceremony and carries no role gate.
 */
export async function endAllSessionsOfUser(
  userId: string,
  cause: "self",
): Promise<{ sessionId: string; workspaceId: string }[]> {
  return await getDb().transaction(async (tx) => {
    const who = await tx.execute(
      sql`SELECT COALESCE(NULLIF(btrim(u.name), ''), u.email) AS display
          FROM web."user" u WHERE u.id = ${userId}`,
    );
    const display = (who.rows[0] as { display: string } | undefined)?.display ?? userId;
    return await endSessionsTx(tx, {
      where: sql`user_id = ${userId}`,
      actor: { userId, display },
      cause,
    });
  });
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
    // An over-age pending row is refused like a vanished one: approval flips a status, never
    // re-mints the credential, so approving past the expiry would mint a session that the lane
    // guard already refuses.
    const updated = await tx.execute(
      sql`UPDATE web.cli_session cs SET status = 'active'
          FROM web.workspace w
          WHERE cs.id = ${sessionId} AND cs.workspace_id = ${workspaceId}
            AND cs.status = 'pending'
            AND w.id = cs.workspace_id AND ${sessionUnexpiredSql("cs", "w")}
          RETURNING cs.id`,
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
    // The expiry predicate applies here too: an over-age credential resolves to nothing, so
    // the route answers the same uniform 404 an unknown bearer gets (no liveness oracle). The
    // dead row itself stays for the owner's sessions page until removed there.
    const rows = await tx.execute(
      sql`SELECT s.id, s.user_id, s.workspace_id,
                 COALESCE(NULLIF(btrim(u.name), ''), u.email) AS display
          FROM web.cli_session s
          JOIN web."user" u ON u.id = s.user_id
          JOIN web.workspace w ON w.id = s.workspace_id
          WHERE s.credential_sha256 = ${sha256OfText(credential)}
            AND ${sessionUnexpiredSql("s", "w")}
          FOR UPDATE OF s`,
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

/** How a flow's credential may be collected — see `login_flow.binding`. */
export type LoginBinding = "device" | "loopback";

const LOGIN_FLOW_TTL_MS = 15 * 60 * 1000;
export const LOGIN_FLOW_POLL_INTERVAL_SECS = 5;
export const LOGIN_FLOW_EXPIRES_IN_SECS = LOGIN_FLOW_TTL_MS / 1000;

/**
 * Start a login flow: mint the pair of codes and park the pending row. The flow_code is the
 * CLI's polling secret — and, once the flow is approved, the EXCHANGE promotes it to the
 * session's one bearer credential (same plaintext, same stored hash shape), which is what lets
 * the hash-only store still "deliver" the credential on the poll: the poller already holds it.
 * The short user_code is what a human types at /verify; the partial unique index keeps it
 * unambiguous among PENDING rows, so minting retries on that one conflict.
 *
 * The flow starts WORKSPACE-LESS: the workspace is chosen (or created) at the browser
 * approval, where the approver's seats are known. `preselect` is the ADDRESS SLUG a
 * `login <workspace>` shortcut named — recorded shape-checked but UNRESOLVED, display-only
 * (it preselects the chooser's matching option and nothing more). A login mints ONE
 * workspace's session; further workspaces are further logins.
 */
export async function startLoginFlow(
  requestedName: string,
  preselect: string | null,
  /** The invite-link token a `topos login <invite-url>` carries — hashed and RECORDED, never
   * validated here (the unauthenticated start must not be a token oracle); the approval
   * resolves it under its own fence. */
  inviteToken?: string,
  /** How the approval outcome is ACCELERATED back. WRITE-ONCE: the CLI declares it at the
   * start, having just bound its own 127.0.0.1 listener, and nothing downstream may change it —
   * the binding is what gates the /verify card's URL pre-arm. The route resolves it on every
   * call; the `device` default is the typed-code flow, which is what a start that declares no
   * acceleration is asking for. */
  binding: LoginBinding = "device",
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
        preselectWorkspace: preselect,
        binding,
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
 * (`kind` is the bundle catalog's own kind tag, or the literal 'channel'). */
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
      /** The workspace id the APPROVAL chose (persisted inside its fence) — the token
       * route's `workspace` decoration reads this immutable id, so a slug rename or a
       * delete+recreate inside the TTL can never re-point a granted flow. */
      approvedWorkspaceId: string | null;
      /** The invitation hint, when the flow carried a token whose invitation names one. */
      hint: LoginGrantHint | null;
    };

/**
 * Mint an approved flow's session at its EXCHANGE — the first poll that finds the flow
 * approved, for BOTH bindings. The approval recorded consent + the chosen workspace and
 * nothing more; the credential comes into existence only when the machine that holds the flow
 * code collects it.
 *
 * ACCEPTED TRADE, documented where the mint happens: the retired auth-code exchange proved the
 * redeemer was the machine the approver's browser could reach, which made a phished approval
 * uncollectable. Approval-anywhere (the mailed magic link finishing a login from any browser,
 * any device) is worth more than that proof, so the flow code alone now redeems once a human
 * has approved — the industry-standard device-grant posture. The remaining mitigations are the
 * card naming the asking machine, the glance-check code read off the operator's own terminal,
 * and the per-user rate belt on the /verify lookup.
 *
 * The born status is computed HERE rather than at the approval: the approver's seat and the
 * workspace's session-approval knob are re-read inside this fence, so a demotion or a knob
 * flip between consent and collection lands on the side the rows say now. Fenced on the flow
 * row, and idempotent — a concurrent or repeated poll finds the session already there and
 * reads it back.
 */
async function mintSessionAtExchange(flowCode: string): Promise<LoginPollResult | null> {
  return await getDb().transaction(async (tx) => {
    // The flow code is re-verified HERE, inside the fence that does the writing — not merely by
    // the caller. A function that mints a credential has to be safe on its own terms, so that a
    // future second call site cannot reopen a hole silently. An approved flow past its TTL
    // matches nothing: consent that was never collected expires with the flow.
    const rows = await tx.execute(
      sql`SELECT id, requested_name, approved_by, approved_workspace_id, session_id,
                 flow_code_sha256, invite_token_sha256
          FROM ${loginFlow}
          WHERE flow_code_sha256 = ${sha256OfText(flowCode)} AND status = 'approved'
            AND expires_at > now()
          FOR UPDATE`,
    );
    const row = rows.rows[0] as
      | {
          id: string;
          requested_name: string;
          approved_by: string | null;
          approved_workspace_id: string | null;
          session_id: string | null;
          flow_code_sha256: Buffer;
          invite_token_sha256: Buffer | null;
        }
      | undefined;
    if (!row || row.approved_by === null || row.approved_workspace_id === null) {
      return null;
    }
    if (row.session_id !== null) {
      // A concurrent exchange won the race; read its outcome rather than minting a second.
      const live = await tx.execute(
        sql`SELECT status FROM web.cli_session WHERE id = ${row.session_id}`,
      );
      const status = (live.rows[0] as { status: SessionStatus } | undefined)?.status;
      return status === undefined
        ? null
        : {
            status: "granted" as const,
            sessionId: row.session_id,
            sessionStatus: status,
            approvedWorkspaceId: row.approved_workspace_id,
            hint:
              row.invite_token_sha256 === null
                ? null
                : await inviteHintByHash(row.invite_token_sha256, row.approved_workspace_id),
          };
    }
    // FOR UPDATE: the seat is the standing this mint rides — lock it so a concurrent seat
    // removal serializes with the exchange instead of racing it (an unlocked read past a
    // committing delete would carry into the insert below and fail its composite FK as a
    // 500 to the poller; the approve fence runs the same discipline).
    const seatRows = await tx.execute(
      sql`SELECT role FROM ${seat}
          WHERE workspace_id = ${row.approved_workspace_id} AND user_id = ${row.approved_by}
          FOR UPDATE`,
    );
    const role = (seatRows.rows[0] as { role: string } | undefined)?.role;
    if (role === undefined) {
      // The seat went away between consent and collection — revocation is a row delete and it
      // is effective immediately, so there is nothing to mint.
      return null;
    }
    const born = sessionBornStatus(
      role as Parameters<typeof sessionBornStatus>[0],
      await sessionApprovalKnobTx(tx, row.approved_workspace_id),
    );
    const sessionId = mintSessionId();
    await tx.insert(cliSession).values({
      id: sessionId,
      workspaceId: row.approved_workspace_id,
      userId: row.approved_by,
      displayName: row.requested_name,
      credentialSha256: row.flow_code_sha256 as never,
      status: born,
    });
    await tx.execute(sql`UPDATE ${loginFlow} SET session_id = ${sessionId} WHERE id = ${row.id}`);
    // The actor is the PERSON who approved, resolved by the one display rule — never the machine
    // name. This is the row an incident is reconstructed from; "MacBook Pro" is not an actor.
    const who = await tx.execute(
      sql`SELECT COALESCE(NULLIF(btrim(u.name), ''), u.email) AS display
          FROM web."user" u WHERE u.id = ${row.approved_by}`,
    );
    const approverDisplay =
      (who.rows[0] as { display: string } | undefined)?.display ?? row.approved_by;
    await auditInTx(tx, {
      workspaceId: row.approved_workspace_id,
      actor: { userId: row.approved_by, display: approverDisplay },
      kind: "session_created",
      subject: sessionId,
      outcome: "ok",
      details: { requestedName: row.requested_name, status: born },
    });
    return {
      status: "granted" as const,
      sessionId,
      sessionStatus: born,
      approvedWorkspaceId: row.approved_workspace_id,
      hint:
        row.invite_token_sha256 === null
          ? null
          : await inviteHintByHash(row.invite_token_sha256, row.approved_workspace_id),
    };
  });
}

/**
 * The CLI's poll, keyed by the flow_code hash — THE completion mechanism for both bindings
 * (a loopback flow's 127.0.0.1 redirect only wakes the waiting client; it decides nothing).
 * The first poll that finds the flow approved runs the mint-at-exchange fence; after that a
 * terminal answer (granted / denied) repeats on every poll until the row is swept, because the
 * client's crash-recovery is to re-poll — a CLI that received `granted` but crashed before
 * persisting its credential re-polls the same code and must get the same `granted` again (the
 * credential is the presented flow_code, echoed by the route, so re-delivery costs nothing).
 * Terminal rows are reaped by [`sweepExpiredLoginFlows`], not on read, so the grant survives
 * its whole TTL. A missing row (already swept, or never existed) reads as expired.
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
  if (row.status === "approved" && row.session_id !== null) {
    // Already exchanged. The grant stays granted while its SESSION lives; a session ended
    // between exchange and this poll (owner reject, revocation) reads as expired — the
    // credential is dead, so "start over" is the honest answer.
    if (row.session_status === null) {
      return { status: "expired" };
    }
    return {
      status: "granted",
      sessionId: row.session_id,
      sessionStatus: row.session_status,
      approvedWorkspaceId: row.approved_workspace_id,
      hint:
        row.invite_token_sha256 === null || row.approved_workspace_id === null
          ? null
          : await inviteHintByHash(row.invite_token_sha256, row.approved_workspace_id),
    };
  }
  if (row.status === "approved") {
    // Approved, never collected. Past the TTL the consent lapses with the flow — an approval
    // nobody polled mints nothing, ever (say `expired`, the honest terminal answer). Inside it,
    // THE EXCHANGE MINTS: the first poll to arrive here runs the fenced mint; a null answer
    // means the world moved between consent and collection (seat gone, flow lapsed mid-flight).
    if (row.expired) {
      return { status: "expired" };
    }
    const minted = await mintSessionAtExchange(flowCode);
    return minted ?? { status: "expired" };
  }
  // pending — expired pending is terminal (the human never approved in time).
  return row.expired ? { status: "expired" } : { status: "pending" };
}

/**
 * The first-destination hint of the invitation a token hash names — ANY status (a granted
 * flow's invitation was consumed by its own approval), the hinted thing resolved to its
 * display name, active bundles only, and only when the invitation belongs to the workspace
 * the approval actually chose (a flow whose token went unaccepted must not decorate a hint
 * into a workspace it never named). The token hash is retained on the row for exactly this
 * read.
 */
async function inviteHintByHash(
  tokenSha256: Buffer,
  approvedWorkspaceId: string,
): Promise<LoginGrantHint | null> {
  const rows = await getDb().execute(
    sql`SELECT b.kind AS bundle_kind, b.name AS bundle_name, c.name AS channel_name
        FROM web.invitation i
        LEFT JOIN web.bundle b ON b.id = i.hint_bundle_id AND b.status = 'active'
        LEFT JOIN web.channel c ON c.id = i.hint_channel_id
        WHERE i.token_sha256 = ${tokenSha256} AND i.workspace_id = ${approvedWorkspaceId}`,
  );
  const row = rows.rows[0] as
    | { bundle_kind: string | null; bundle_name: string | null; channel_name: string | null }
    | undefined;
  if (!row) {
    return null;
  }
  // Both bundle columns ride the SAME left join, so they stand or fall together; the catalog's
  // kind column is NOT NULL, which is why the hint reports it rather than assuming one.
  if (row.bundle_kind !== null && row.bundle_name !== null) {
    return { kind: row.bundle_kind, name: row.bundle_name };
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
 * Resolve a CHOSEN workspace slug AND the acting person's seat in it, inside the caller's
 * approve transaction. The tenancy grammar decides the lookup: single-tenant approvals
 * resolve to the install's one workspace whatever slug was posted; multi-tenant approvals
 * resolve the posted slug by name. A missing workspace or a seatless actor both resolve to
 * null, and the caller answers the same uniform refusal — a non-member learns nothing, not
 * even that the workspace exists.
 */
async function seatedFlowWorkspaceTx(
  tx: Tx,
  chosenSlug: string,
  actorUserId: string,
): Promise<{ workspaceId: string; role: "owner" | "reviewer" | "member" } | null> {
  const rows =
    composition.tenancy === "multi"
      ? await tx.execute(sql`SELECT id FROM ${workspace} WHERE name = ${chosenSlug} LIMIT 1`)
      : await tx.execute(sql`SELECT id FROM ${workspace} LIMIT 1`);
  const ws = rows.rows[0] as { id: string } | undefined;
  if (!ws) {
    return null;
  }
  // FOR UPDATE: the seat is the authorization — lock it so a concurrent seat removal
  // serializes with this ceremony instead of racing it (no approve commits on a seat whose
  // delete already committed).
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

/** The approver's workspace pick, posted by the /verify chooser. `null` when the page posted
 * no ordinary pick (the invite-token pre-bound arm) — valid only while the token binds. */
export type LoginApproveChoice =
  /** A workspace the approver already holds a seat in, by ADDRESS slug (single tenancy
   * ignores the slug — the install IS its one workspace). */
  | { kind: "seat"; workspace: string }
  /** A pending invitation of the approver's, by row id — accepted inside the approve fence. */
  | { kind: "invitation"; id: string }
  /** A brand-new workspace, born inside the approve fence; the approver becomes its owner. */
  | { kind: "create"; displayName: string; slug: string };

export type LoginApproveOutcome =
  /** Consent + the chosen workspace are recorded; the page copy names the join. NO session
   * exists yet — the CLI's next poll runs the exchange that mints it. */
  | {
      outcome: "approved";
      requestedName: string;
      workspaceName: string;
      workspaceDisplay: string;
      /** The flow's own challenge (hex of its code hash) when the flow is LOOPBACK-bound —
       * the page fires the listener's wake-up redirect only for EXACTLY this flow, never for
       * another card approved from the same loopback-armed URL. Non-secret (derivable by
       * whoever started the flow); null for a device flow, which has nothing to wake. */
      flowChallenge: string | null;
    }
  /** The create arm's typed refusal: the slug is reserved or taken (indistinguishable, like
   * /new); the whole transaction rolled back and the flow stays pending for a retry. */
  | { outcome: "taken" }
  /** The create arm's per-person floors (counted inside the birth fence, exactly as /new's) —
   * typed, honest, and the flow stays pending. */
  | { outcome: "rate-limited" }
  | { outcome: "owned-limit" };

/** The in-transaction abort sentinel: an approval that cannot complete must ROLL BACK any
 * invitation accept or workspace birth it already made (a bare `return null` from a Drizzle
 * transaction COMMITS — only a throw rolls back). Thrown inside the fence, caught at the
 * boundary → the uniform null. */
const APPROVE_ABORT = Symbol("login-approve-abort");
/** The create arm's typed rollback: same discipline, surfaced as `taken` instead of null. */
const APPROVE_TAKEN = Symbol("login-approve-taken");
/** The create arm's floor rollbacks — the same discipline, surfaced typed like `taken`. */
const APPROVE_RATE_LIMITED = Symbol("login-approve-rate-limited");
const APPROVE_OWNED_LIMIT = Symbol("login-approve-owned-limit");

/**
 * FENCE 2 — the login-flow approve, one FOR UPDATE transaction: lock the pending row by
 * user_code, re-check liveness under the lock, resolve THE CHOSEN WORKSPACE (a seat pick, an
 * invitation accept, or a workspace birth — validated inside this same fence), and record
 * consent: `status='approved'`, approved_by, approved_workspace_id. NO SESSION IS MINTED
 * HERE — for either binding. The credential comes into existence at the CLI's exchange (the
 * first poll that finds the flow approved), which re-reads seat + knob at collection time.
 *
 * A flow-carried invite token PRE-BINDS the workspace: when it still resolves to a live
 * invitation this approver's accept fences admit, the accept runs inside this fence and the
 * posted choice is IGNORED (a crafted flow cannot aim an invitation at A while connecting B).
 * A token that no longer binds — dead, or addressed to an account the fences refuse — wrote
 * nothing, and the posted choice decides like any ordinary flow (the page's chooser fell
 * through with the honest line).
 *
 * A refusal returns null — the same answer an expired code gets, so the ceremony is no
 * existence or membership oracle — EXCEPT the create arm's typed `taken`. The approver's
 * browser-session gate runs in the ROUTE before this is called — approval records consent
 * for a credential that acts as you, in this ONE workspace.
 */
export async function approveLoginFlow(
  userCode: string,
  approver: { userId: string; display: string },
  choice: LoginApproveChoice | null,
): Promise<LoginApproveOutcome | null> {
  try {
    return await getDb().transaction(async (tx) => {
      const rows = await tx.execute(
        sql`SELECT id, requested_name, invite_token_sha256, flow_code_sha256, binding
            FROM ${loginFlow}
            WHERE user_code = ${userCode} AND status = 'pending' AND expires_at > now()
            FOR UPDATE`,
      );
      const row = rows.rows[0] as
        | {
            id: string;
            requested_name: string;
            invite_token_sha256: Buffer | null;
            flow_code_sha256: Buffer;
            binding: LoginBinding;
          }
        | undefined;
      if (!row) {
        return null;
      }

      let chosenWorkspaceId: string | null = null;
      let via: "seat" | "invitation" | "create" | "invite-token" | null = null;

      // The invitation weave: a flow that carries an invite token accepts the invitation INSIDE
      // this same fence when the approver is its rightful addressee — sign-in → accept →
      // approve is one act even for a brand-new invitee, and the accept's seat is the standing
      // the exchange later re-reads. The fences answer wrong_account/unverified BEFORE any
      // write, so falling through to the posted choice commits nothing of the invitation.
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
            chosenWorkspaceId = outcome.workspaceId;
            via = "invite-token";
          }
        }
      }

      if (chosenWorkspaceId === null) {
        // No live token bound the workspace — the posted choice decides, each arm validated
        // under this same lock (standing is rows, read now, never page state).
        if (choice === null) {
          throw APPROVE_ABORT;
        }
        if (choice.kind === "seat") {
          const resolved = await seatedFlowWorkspaceTx(tx, choice.workspace, approver.userId);
          if (resolved === null) {
            throw APPROVE_ABORT;
          }
          chosenWorkspaceId = resolved.workspaceId;
          via = "seat";
        } else if (choice.kind === "invitation") {
          // The approver's OWN pending invitation, by id — the same accept fences as the token
          // weave (addressee + verified mailbox), so a guessed id buys nothing.
          const inv = await lockPendingInvitationTx(tx, sql`i.id = ${choice.id}`);
          if (inv === null) {
            throw APPROVE_ABORT;
          }
          const outcome = await acceptInvitationTx(tx, inv, await sessionAccountTx(tx, approver), {
            mailboxProven: false,
          });
          if (outcome.outcome !== "accepted") {
            throw APPROVE_ABORT;
          }
          chosenWorkspaceId = outcome.workspaceId;
          via = "invitation";
        } else {
          // The birth is a MULTI surface, refused HERE too, not only in the route's precheck:
          // the fence must be safe on its own terms, and a second workspace row on single
          // tenancy would break every LIMIT-1 resolution of the install's one workspace.
          if (composition.tenancy !== "multi") {
            throw APPROVE_ABORT;
          }
          // The workspace birth runs INSIDE this fence — the identical one-transaction birth
          // /new runs (the shared tx body, per-person advisory lock + counted floors
          // included). The route ran the surface pre-check (tenancy + the entitlement gate)
          // before calling; a reserved slug and the floors answer their typed rollbacks here,
          // and a create-race unique violation surfaces at the boundary below.
          const born = await createWorkspaceTx(
            tx,
            approver,
            { name: choice.slug, displayName: choice.displayName },
            { via: "login" },
          );
          if (born.outcome === "taken") {
            throw APPROVE_TAKEN;
          }
          if (born.outcome === "rate-limited") {
            throw APPROVE_RATE_LIMITED;
          }
          if (born.outcome === "owned-limit") {
            throw APPROVE_OWNED_LIMIT;
          }
          chosenWorkspaceId = born.workspaceId;
          via = "create";
        }
      }

      await tx.execute(
        sql`UPDATE ${loginFlow}
            SET status = 'approved', approved_by = ${approver.userId},
                approved_workspace_id = ${chosenWorkspaceId}
            WHERE id = ${row.id}`,
      );
      await auditInTx(tx, {
        workspaceId: chosenWorkspaceId,
        actor: { userId: approver.userId, display: approver.display },
        kind: "login_approved",
        subject: row.requested_name,
        outcome: "ok",
        details: { requestedName: row.requested_name, via },
      });
      const ws = await tx.execute(
        sql`SELECT name, display_name FROM ${workspace} WHERE id = ${chosenWorkspaceId}`,
      );
      const wsRow = ws.rows[0] as { name: string; display_name: string } | undefined;
      return {
        outcome: "approved" as const,
        requestedName: row.requested_name,
        workspaceName: wsRow?.name ?? "",
        workspaceDisplay: wsRow?.display_name ?? wsRow?.name ?? "",
        flowChallenge:
          row.binding === "loopback" ? Buffer.from(row.flow_code_sha256).toString("hex") : null,
      };
    });
  } catch (error) {
    // The clean-refusal rollbacks surface typed (the uniform null, or the create arm's taken —
    // a name race lands there too); any other error is a real fault and propagates.
    if (error === APPROVE_ABORT) {
      return null;
    }
    if (error === APPROVE_TAKEN || (choice?.kind === "create" && isUniqueViolation(error))) {
      return { outcome: "taken" };
    }
    if (error === APPROVE_RATE_LIMITED) {
      return { outcome: "rate-limited" };
    }
    if (error === APPROVE_OWNED_LIMIT) {
      return { outcome: "owned-limit" };
    }
    throw error;
  }
}

/**
 * The verify page's deny arm — the flow is workspace-less, so denying takes NO seat: any
 * signed-in holder of the code can kill a request to act as them (the same lock discipline,
 * terminal 'denied'). The audit row is SERVER-scoped (null workspace — no workspace was ever
 * chosen); workspace-scoped audit readers query by equality, so it surfaces nowhere but here.
 */
export async function denyLoginFlow(
  userCode: string,
  denier: { userId: string; display: string },
): Promise<{
  /** The denied flow's challenge when LOOPBACK-bound (see the approve outcome's twin) — the
   * page wakes the listener only for exactly the flow the CLI armed. */
  flowChallenge: string | null;
} | null> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(
      sql`SELECT id, requested_name, flow_code_sha256, binding FROM ${loginFlow}
          WHERE user_code = ${userCode} AND status = 'pending' AND expires_at > now()
          FOR UPDATE`,
    );
    const row = rows.rows[0] as
      | { id: string; requested_name: string; flow_code_sha256: Buffer; binding: LoginBinding }
      | undefined;
    if (!row) {
      return null;
    }
    await tx.execute(sql`UPDATE ${loginFlow} SET status = 'denied' WHERE id = ${row.id}`);
    await auditInTx(tx, {
      workspaceId: null,
      actor: { userId: denier.userId, display: denier.display },
      kind: "login_denied",
      subject: row.requested_name,
      outcome: "ok",
    });
    return {
      flowChallenge:
        row.binding === "loopback" ? Buffer.from(row.flow_code_sha256).toString("hex") : null,
    };
  });
}

/** The flow-carried invitation, resolved against THE VIEWER — decided in this module so the
 * addressing predicate (an email comparison, the invitation design's one sanctioned kind)
 * never leaves it. `live` pre-binds the card; every other state falls through to the
 * ordinary chooser with one honest line. Null when the flow carries no token. */
export type FlowInvite =
  | {
      state: "live";
      workspaceName: string;
      workspaceDisplay: string;
      role: string;
      /** The invitation's role against the workspace's knob — the card's static disclosure. */
      awaitsApproval: boolean;
    }
  /** Addressed to a DIFFERENT account — the emailed invitation link owns account switching. */
  | { state: "other"; workspaceName: string; workspaceDisplay: string }
  /** Addressed to the viewer's account, but the mailbox was never proven — the emailed link
   * (whose delivery IS the proof) is the way through. */
  | { state: "unverified"; workspaceName: string; workspaceDisplay: string }
  /** The token no longer resolves (expired, consumed, revoked). */
  | { state: "dead" }
  | null;

/** The verify page's resolved request: what is asking, the code for the glance-check, the
 * `login <workspace>` shortcut's preselect hint (display-only), and the flow-carried
 * invitation resolved against the viewing account. */
export interface PendingLoginFlowView {
  requestedName: string;
  userCode: string;
  /** Loopback flows may pre-arm the card from the URL challenge; device flows demand typing. */
  binding: LoginBinding;
  /** The shortcut's preselect slug — preselects a matching chooser option, nothing more. */
  preselect: string | null;
  invite: FlowInvite;
}

/** The verify page's lookup: the pending request a typed user_code names (display only),
 * with the flow-carried invitation resolved against the viewing account. */
export async function pendingLoginFlow(
  userCode: string,
  viewerId: string,
): Promise<PendingLoginFlowView | null> {
  return pendingLoginFlowWhere(sql`user_code = ${userCode}`, viewerId);
}

/**
 * The loopback auto-open's lookup: the pending request whose flow-code HASH the CLI put in
 * the URL it opened (hex of the same SHA-256 this store already keys the row by — the code
 * itself never enters a URL; a preimage is infeasible, so the challenge identifies without
 * revealing). A malformed challenge is simply a miss.
 */
export async function pendingLoginFlowByChallenge(
  challengeHex: string,
  viewerId: string,
): Promise<PendingLoginFlowView | null> {
  if (!/^[0-9a-f]{64}$/.test(challengeHex)) {
    return null;
  }
  // LOOPBACK FLOWS ONLY — the line that keeps a URL-resolved card honest. The challenge is the
  // device code's own hash, so whoever STARTED a flow can compute it, and starting one takes no
  // credential: pre-arming a card from it must stay a convenience for the machine's own
  // operator, not a mailed one-click approve for a stranger's flow. A device-bound flow
  // resolves only through the typed code — the typing is what binds approver to asker.
  return pendingLoginFlowWhere(
    sql`flow_code_sha256 = decode(${challengeHex}, 'hex') AND binding = 'loopback'`,
    viewerId,
  );
}

/**
 * The /login page's quiet hint probe: whether a LIVE pending LOOPBACK flow stands behind a
 * challenge — a bare EXISTENCE bit, deliberately nothing more (no name, no code, no view; the
 * page runs pre-auth, so this must open no new disclosure surface beyond what the challenge
 * holder — the flow's own starter — already knows). Loopback-fenced like the card pre-arm.
 */
export async function pendingLoopbackFlowCode(challengeHex: string): Promise<string | null> {
  if (!/^[0-9a-f]{64}$/.test(challengeHex)) {
    return null;
  }
  // The GLANCE CODE rides the answer, not just existence: the sign-in page is the FIRST screen
  // of the ceremony, and the terminal's waiting line points at "the same code" — so the code
  // must be visible from first paint, not only after sign-in. Disclosing it to the
  // challenge-holder adds no capability: this renders only under a challenge-bearing URL, and
  // possession of that URL already suffices to approve after sign-in (the card pre-arms from
  // the same challenge). Every fence stays: loopback-bound only, live-and-pending only,
  // constant null for anything else.
  const rows = await getDb().execute(
    sql`SELECT user_code FROM ${loginFlow}
        WHERE flow_code_sha256 = decode(${challengeHex}, 'hex') AND binding = 'loopback'
          AND status = 'pending' AND expires_at > now()`,
  );
  return (rows.rows[0] as { user_code: string } | undefined)?.user_code ?? null;
}

async function pendingLoginFlowWhere(
  cond: ReturnType<typeof sql>,
  viewerId: string,
): Promise<PendingLoginFlowView | null> {
  const rows = await getDb().execute(
    sql`SELECT f.requested_name, f.preselect_workspace, f.user_code, f.binding,
               f.invite_token_sha256 IS NOT NULL AS invite_carried,
               i.email AS invite_email, i.role AS invite_role,
               w.name AS invite_ws_name, w.display_name AS invite_ws_display,
               w.session_approval AS invite_ws_knob
        FROM ${loginFlow} f
        LEFT JOIN web.invitation i ON i.token_sha256 = f.invite_token_sha256
          AND i.status = 'pending' AND (i.expires_at IS NULL OR i.expires_at > now())
        LEFT JOIN ${workspace} w ON w.id = i.workspace_id
        WHERE ${cond} AND f.status = 'pending' AND f.expires_at > now()`,
  );
  const row = rows.rows[0] as
    | {
        requested_name: string;
        preselect_workspace: string | null;
        user_code: string;
        binding: LoginBinding;
        invite_carried: boolean;
        invite_email: string | null;
        invite_role: string | null;
        invite_ws_name: string | null;
        invite_ws_display: string | null;
        invite_ws_knob: "off" | "on" | null;
      }
    | undefined;
  if (!row) {
    return null;
  }
  let invite: FlowInvite = null;
  if (row.invite_carried) {
    if (row.invite_email === null || row.invite_ws_name === null) {
      invite = { state: "dead" };
    } else {
      const viewer = await getDb().execute(
        sql`SELECT email, email_verified FROM web."user" WHERE id = ${viewerId}`,
      );
      const account = viewer.rows[0] as { email: string; email_verified: boolean } | undefined;
      const workspaceName = row.invite_ws_name;
      const workspaceDisplay = row.invite_ws_display ?? row.invite_ws_name;
      // The SAME addressing predicate acceptInvitationTx fences on — display only here; the
      // approve fence re-decides under its own lock.
      if (account === undefined || account.email.trim().toLowerCase() !== row.invite_email) {
        invite = { state: "other", workspaceName, workspaceDisplay };
      } else if (!account.email_verified) {
        invite = { state: "unverified", workspaceName, workspaceDisplay };
      } else {
        const role = row.invite_role ?? "member";
        invite = {
          state: "live",
          workspaceName,
          workspaceDisplay,
          role,
          awaitsApproval:
            sessionBornStatus(
              role as Parameters<typeof sessionBornStatus>[0],
              row.invite_ws_knob ?? "off",
            ) === "pending",
        };
      }
    }
  }
  return {
    requestedName: row.requested_name,
    userCode: row.user_code,
    binding: row.binding,
    preselect: row.preselect_workspace,
    invite,
  };
}

// ── The /verify chooser's reads (display only — the approve fence re-validates) ─────────────

/** One workspace the viewer already holds a seat in — a chooser radio row. */
export interface SeatChoice {
  workspaceId: string;
  name: string;
  displayName: string;
  role: "owner" | "reviewer" | "member";
  /** The static per-option disclosure: a session this person mints there is born pending. */
  awaitsApproval: boolean;
}

/** The viewer's seats with each workspace's session-approval posture, oldest seat first. */
export async function seatChoicesFor(userId: string): Promise<SeatChoice[]> {
  const rows = await getDb().execute(
    sql`SELECT w.id, w.name, w.display_name, s.role, w.session_approval
        FROM ${seat} s JOIN ${workspace} w ON w.id = s.workspace_id
        WHERE s.user_id = ${userId}
        ORDER BY s.created_at, w.id`,
  );
  return (
    rows.rows as {
      id: string;
      name: string;
      display_name: string;
      role: "owner" | "reviewer" | "member";
      session_approval: "off" | "on" | null;
    }[]
  ).map((r) => ({
    workspaceId: r.id,
    name: r.name,
    displayName: r.display_name,
    role: r.role,
    awaitsApproval: sessionBornStatus(r.role, r.session_approval ?? "off") === "pending",
  }));
}

/** One pending invitation of the viewer's — a chooser row whose accept runs in the approve
 * fence. */
export interface PendingInvitationChoice {
  id: string;
  workspaceName: string;
  workspaceDisplay: string;
  role: string;
  /** The invitation's role against the workspace's knob — the same static disclosure. */
  awaitsApproval: boolean;
  hint: LoginGrantHint | null;
}

/**
 * The signed-in viewer's pending, unexpired invitations by their VERIFIED email — display
 * only; the accept act re-validates inside the approve fence. An unverified mailbox yields an
 * EMPTY list (possession of the account is not possession of the mailbox), with
 * `heldUnverified` saying honestly that invitations exist and the emailed link — whose
 * delivery proves the mailbox — is the way through.
 */
export async function pendingInvitationsFor(
  userId: string,
): Promise<{ invitations: PendingInvitationChoice[]; heldUnverified: boolean }> {
  const viewer = await getDb().execute(
    sql`SELECT email, email_verified FROM web."user" WHERE id = ${userId}`,
  );
  const account = viewer.rows[0] as { email: string; email_verified: boolean } | undefined;
  if (account === undefined) {
    return { invitations: [], heldUnverified: false };
  }
  const lowered = account.email.trim().toLowerCase();
  const rows = await getDb().execute(
    sql`SELECT i.id, i.role, w.name, w.display_name, w.session_approval,
               b.kind AS bundle_kind, b.name AS bundle_name, c.name AS channel_name
        FROM web.invitation i
        JOIN ${workspace} w ON w.id = i.workspace_id
        LEFT JOIN web.bundle b ON b.id = i.hint_bundle_id AND b.status = 'active'
        LEFT JOIN web.channel c ON c.id = i.hint_channel_id
        WHERE i.email = ${lowered} AND i.status = 'pending'
          AND (i.expires_at IS NULL OR i.expires_at > now())
        ORDER BY i.created_at, i.id`,
  );
  if (!account.email_verified) {
    return { invitations: [], heldUnverified: rows.rows.length > 0 };
  }
  return {
    heldUnverified: false,
    invitations: (
      rows.rows as {
        id: string;
        role: string;
        name: string;
        display_name: string;
        session_approval: "off" | "on" | null;
        bundle_kind: string | null;
        bundle_name: string | null;
        channel_name: string | null;
      }[]
    ).map((r) => ({
      id: r.id,
      workspaceName: r.name,
      workspaceDisplay: r.display_name,
      role: r.role,
      awaitsApproval:
        sessionBornStatus(
          r.role as Parameters<typeof sessionBornStatus>[0],
          r.session_approval ?? "off",
        ) === "pending",
      hint:
        r.bundle_kind !== null && r.bundle_name !== null
          ? { kind: r.bundle_kind, name: r.bundle_name }
          : r.channel_name !== null
            ? { kind: "channel", name: r.channel_name }
            : null,
    })),
  };
}

// ── The lane-side second connect (`POST /api/v1/login/connect`) ─────────────────────────────

/**
 * The browser-free second connect: an already-credentialed person's machine asks for a
 * FURTHER workspace's session. ONE transaction, authorization resolved INSIDE it, in the
 * SHARED LOCK ORDER (seat before session — `removeSeat` locks the target seat and then
 * deletes that person's session rows, so a connect that locked its acting session first
 * would deadlock a same-workspace offboarding):
 *
 *  1. a plain UNLOCKED read of the acting session by credential hash (active + unexpired) —
 *     identity only, nothing decided on it;
 *  2. resolve the target workspace under the tenancy grammar (multi: by slug; single: the
 *     install's one workspace — a slug naming anything else is the uniform miss) and lock
 *     the acting person's SEAT there FOR UPDATE (seat standing is the trust basis — this
 *     endpoint is never an existence oracle);
 *  3. re-select + lock the acting session FOR UPDATE and REVALIDATE active + unexpired —
 *     the round-1 security property lives HERE: a revocation between the plain read and
 *     this lock reads as gone, so no resolve-to-insert window reopens;
 *  4. mint a FRESH secret, insert the session born per the ONE rule, audit.
 *
 * Every miss is the same null. The plaintext credential returns exactly once over the
 * authenticated exchange, like the flow start returning its code.
 */
export async function connectSession(
  credential: string,
  workspaceSlug: string,
  requestedName: string,
): Promise<{
  credential: string;
  sessionId: string;
  sessionStatus: SessionStatus;
  workspace: { workspaceId: string; name: string; displayName: string };
} | null> {
  return await getDb().transaction(async (tx) => {
    // (1) Identity probe, deliberately lock-free — the same fail-closed predicate as the lane
    // guard (hash computed in Postgres); the authoritative re-check runs under lock at (3).
    const probe = await tx.execute(
      sql`SELECT s.id, s.user_id, COALESCE(NULLIF(btrim(u.name), ''), u.email) AS display
          FROM web.cli_session s
          JOIN web."user" u ON u.id = s.user_id
          JOIN ${workspace} w ON w.id = s.workspace_id
          WHERE s.credential_sha256 = ${sha256OfText(credential)}
            AND s.status = 'active'
            AND ${sessionUnexpiredSql("s", "w")}`,
    );
    const actor = probe.rows[0] as { id: string; user_id: string; display: string } | undefined;
    if (actor === undefined) {
      return null;
    }
    // (2) The target workspace + the SEAT lock — the shared order's first lock, matching
    // removeSeat; a concurrent removal serializes with this mint instead of racing it.
    const rows =
      composition.tenancy === "multi"
        ? await tx.execute(
            sql`SELECT id, name, display_name FROM ${workspace}
                WHERE name = ${workspaceSlug} LIMIT 1`,
          )
        : await tx.execute(sql`SELECT id, name, display_name FROM ${workspace} LIMIT 1`);
    const ws = rows.rows[0] as { id: string; name: string; display_name: string } | undefined;
    if (ws === undefined) {
      return null;
    }
    // Single tenancy: an empty slug addresses the origin's own workspace (the authorize
    // route's grammar); any OTHER name is the uniform miss.
    if (composition.tenancy !== "multi" && workspaceSlug !== "" && ws.name !== workspaceSlug) {
      return null;
    }
    const seats = await tx.execute(
      sql`SELECT role FROM ${seat} WHERE workspace_id = ${ws.id} AND user_id = ${actor.user_id}
          FOR UPDATE`,
    );
    const seatRow = seats.rows[0] as { role: "owner" | "reviewer" | "member" } | undefined;
    if (seatRow === undefined) {
      return null;
    }
    // (3) NOW the acting session, locked + revalidated — seat before session everywhere, and
    // a revocation that won the race reads as gone rather than being minted past.
    const held = await tx.execute(
      sql`SELECT 1 FROM web.cli_session s
          JOIN ${workspace} w ON w.id = s.workspace_id
          WHERE s.id = ${actor.id} AND s.status = 'active' AND ${sessionUnexpiredSql("s", "w")}
          FOR UPDATE OF s`,
    );
    if (held.rows.length === 0) {
      return null;
    }
    const born = sessionBornStatus(seatRow.role, await sessionApprovalKnobTx(tx, ws.id));
    const minted = mintSecret();
    const sessionId = mintSessionId();
    await tx.insert(cliSession).values({
      id: sessionId,
      workspaceId: ws.id,
      userId: actor.user_id,
      displayName: requestedName,
      credentialSha256: sql`${sha256OfText(minted)}` as never,
      status: born,
    });
    await auditInTx(tx, {
      workspaceId: ws.id,
      actor: { userId: actor.user_id, display: actor.display },
      kind: "session_created",
      subject: sessionId,
      outcome: "ok",
      details: { requestedName, status: born, via: "connect" },
    });
    return {
      credential: minted,
      sessionId,
      sessionStatus: born,
      workspace: { workspaceId: ws.id, name: ws.name, displayName: ws.display_name },
    };
  });
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
 * The ONE session-age predicate (the guard's rule) as a SQL fragment over the given
 * `cli_session` / `workspace` table aliases: TRUE while the session is within the workspace's
 * owner-set expiry (`session_max_age_ms`), or always when the policy is unset. Every surface
 * that treats a session as LIVE — the lane guard, the self-logout lookup, the owner approve,
 * the session counts — reuses THIS fragment, so no page or route can call a
 * session live after its credential stopped resolving. The aliases are compile-time constants
 * at every call site, never user input.
 */
export function sessionUnexpiredSql(cs: string, w: string) {
  return sql.raw(
    `(${w}.session_max_age_ms IS NULL OR ${cs}.created_at > now() - make_interval(secs => ${w}.session_max_age_ms / 1000.0))`,
  );
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
          AND ${sessionUnexpiredSql("cs", "w")}
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
 * The credential-FIRST resolve for routes whose workspace rides in the BODY (the publish
 * family): the same live-session → seat chain as `sessionActor`, keyed by the credential alone
 * — it is workspace-scoped and hash-unique, so at most one session answers, and the session
 * row itself names the one workspace it may act in. The caller authenticates BEFORE reading
 * the request body (so an unauthenticated caller never makes this tier buffer a large body),
 * then binds the parsed body's workspace against `workspaceId` — a mismatch is the same
 * uniform 404 the old workspace-keyed lookup answered.
 */
export async function sessionActorByCredential(
  credential: string,
): Promise<(SessionActorRow & { workspaceId: string }) | null> {
  const rows = await getDb().execute(
    sql`UPDATE web.cli_session cs SET last_seen_at = now()
        FROM ${seat} s, web."user" u, ${workspace} w
        WHERE cs.credential_sha256 = ${sha256OfText(credential)}
          AND w.id = cs.workspace_id
          AND ${sessionUnexpiredSql("cs", "w")}
          AND u.id = cs.user_id
          AND s.user_id = cs.user_id AND s.workspace_id = cs.workspace_id
        RETURNING cs.id AS session_id, cs.workspace_id, cs.user_id,
          -- The display rule (app/lib/person-display.ts): a blank name falls back to the email.
          COALESCE(NULLIF(btrim(u.name), ''), u.email) AS user_display, s.role,
          cs.status AS session_status`,
  );
  const row = rows.rows[0] as
    | {
        session_id: string;
        workspace_id: string;
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
    workspaceId: row.workspace_id,
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
      // The member cap's seat-mint backstop (a no-op without a `members` limit): a full
      // workspace is SKIPPED — its invitation stays pending, so a wider limit later binds it
      // on the next verification pass or through the accept ceremony.
      if (await memberCapReachedInTx(tx, inv.workspace_id, userId)) {
        continue;
      }
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
      row.bundle_kind !== null && row.bundle_name !== null
        ? { kind: row.bundle_kind, name: row.bundle_name }
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
  | { outcome: "unverified" }
  /** The workspace's member limit is reached — the invitation stays pending (a wider limit
   * lets the same link succeed later); nothing is consumed. */
  | { outcome: "workspace_full" };

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
  // The member cap's seat-mint backstop (a no-op without a `members` limit): a full workspace
  // refuses BEFORE anything is written — the invitation stays pending, and an accepter who
  // already holds a seat is never refused (the insert below no-ops for them anyway).
  if (await memberCapReachedInTx(tx, inv.workspaceId, account.userId)) {
    return { outcome: "workspace_full" };
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
  // Hint effects — AFTER the seat row, same transaction: the first destination becomes an
  // ASSIGNMENT aimed at the newcomer, attributed to the inviter (they chose it). Any decline
  // the person carried from an earlier stay is cleared, so an invitation that names a skill
  // really does deliver it. The hinted thing may have been deleted since the invite (the FK
  // cleared the column) or archived; then nothing lands.
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
        sql`INSERT INTO web.assignment (workspace_id, user_id, bundle_id, self, created_by)
            VALUES (${inv.workspaceId}, ${account.userId}, ${inv.hintBundleId},
                    ${inv.invitedBy === null}, ${inv.invitedBy ?? account.userId})
            ON CONFLICT DO NOTHING`,
      );
      await tx.execute(
        sql`DELETE FROM web.decline
            WHERE user_id = ${account.userId} AND bundle_id = ${inv.hintBundleId}`,
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
        sql`INSERT INTO web.assignment (workspace_id, user_id, channel_id, self, created_by)
            VALUES (${inv.workspaceId}, ${account.userId}, ${inv.hintChannelId},
                    ${inv.invitedBy === null}, ${inv.invitedBy ?? account.userId})
            ON CONFLICT DO NOTHING`,
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
  // Stored HASHED, never as the live token: this row is a two-minute sign-in capability for the
  // invited address, and a plaintext one would let anything that can read `web.verification` — a
  // backup, a replica, the cloud role's mirror of the web lane — sign in as its addressee.
  //
  // The digest is computed IN Postgres (this tier computes none) and must match the magic-link
  // plugin's stored form BYTE FOR BYTE, because the plugin is what looks it up on verify:
  // unpadded base64URL of the SHA-256. `encode(…, 'base64')` gives padded standard base64, so the
  // padding comes off and the two alphabet characters are translated.
  await getDb().execute(
    sql`INSERT INTO web.verification (id, identifier, value, expires_at)
        VALUES (${`iv_${randomBytes(16).toString("hex")}`},
                translate(rtrim(encode(sha256(convert_to(${token}, 'UTF8')), 'base64'), '='),
                          '+/', '-_'),
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
 * The canonical FEED fragment — what the server says this person should have: everything
 * assigned to them or to EVERYONE (bundles directly, and the members of assigned channels),
 * active bundles only, minus the bundles they have declined. The workspace baseline needs no
 * clause of its own: it is the default channel, assigned to everyone, like any other row.
 *
 * This is the demand HALF of demand ∩ entitlement — the seat itself is the entitlement
 * (whole-catalog), so delivery = this set, and callers add the has-current join as their
 * surface needs. Kept HERE so every demand-shaped read shares one predicate.
 * Params bind as values when strings, or inline as SQL fragments (a correlated column
 * reference, for set-level consumers).
 */
export const feedDemandSql = (
  userId: string | ReturnType<typeof sql>,
  workspaceId: string | ReturnType<typeof sql>,
) => sql`
  SELECT DISTINCT src.bundle_id
  FROM (
    SELECT a.bundle_id
    FROM web.assignment a
    WHERE a.workspace_id = ${workspaceId} AND a.bundle_id IS NOT NULL
      AND (a.user_id = ${userId} OR a.user_id IS NULL)
    UNION
    SELECT cb.bundle_id
    FROM web.assignment a
    JOIN web.channel_bundle cb ON cb.channel_id = a.channel_id
    WHERE a.workspace_id = ${workspaceId} AND a.channel_id IS NOT NULL
      AND (a.user_id = ${userId} OR a.user_id IS NULL)
  ) src
  JOIN web.bundle b ON b.id = src.bundle_id AND b.workspace_id = ${workspaceId}
  WHERE b.status = 'active'
    AND NOT EXISTS (
      SELECT 1 FROM web.decline d
      WHERE d.user_id = ${userId} AND d.bundle_id = src.bundle_id
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
