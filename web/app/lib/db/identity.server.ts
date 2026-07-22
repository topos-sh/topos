import { randomBytes } from "node:crypto";
import { appendFileSync } from "node:fs";
import { eq, sql } from "drizzle-orm";
import { composition } from "@/composition.server";
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
export function mintDeviceLinkId(): string {
  return `dl_${randomBytes(16).toString("hex")}`;
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

/**
 * The ceremony-lane read: the workspace row an OPAQUE ID names (null on a miss). For callers
 * whose authorization is the ceremony row itself — the granted device poll decorates from the
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

// ── Device links (device ↔ workspace — the per-workspace half of enrollment) ────────────────

export type DeviceLinkStatus = "active" | "pending";

/**
 * THE born-status rule, written once: a link created by an act of a seated member is born
 * 'active' when the actor is an OWNER (the actor is the approval, regardless of the knob);
 * otherwise the workspace's device-approval knob decides — 'off' → 'active', 'on' → 'pending'.
 * Invitation-woven links get NO exception. Applies identically at /verify approval, at
 * invitation accept, and at the link lane op.
 */
export function linkBornStatus(
  role: "owner" | "reviewer" | "member",
  knob: "off" | "on",
): DeviceLinkStatus {
  if (role === "owner") {
    return "active";
  }
  return knob === "on" ? "pending" : "active";
}

/** The workspace's device-approval knob, read inside the caller's transaction. */
async function deviceApprovalKnobTx(tx: Tx, workspaceId: string): Promise<"off" | "on"> {
  const rows = await tx.execute(
    sql`SELECT device_approval FROM ${workspace} WHERE id = ${workspaceId}`,
  );
  return (rows.rows[0] as { device_approval: "off" | "on" } | undefined)?.device_approval ?? "off";
}

/**
 * Create ONE device↔workspace link inside the caller's transaction, idempotently: an existing
 * row (whatever its status) is left untouched and its CURRENT status returned — no duplicate,
 * no error. A created link lands its `device_linked` audit row in the same transaction.
 *
 * The DEVICE ROW IS LOCKED FIRST and a revoked (or vanished) device refuses — the person
 * guard runs outside the transaction, so without this check a link apply racing the global
 * revoke (which flips `revoked_at` under the same row lock, THEN severs) could insert after
 * the sever and leave a link attached to a dead device. The lock serializes the two: the
 * apply either commits first (its link is then severed by the revoke's fresh-snapshot
 * DELETE) or blocks on the device row and sees `revoked_at` set. The device-mint fence
 * (approval) inserts the device in this same transaction, so its lock is trivially clean.
 */
async function createDeviceLinkTx(
  tx: Tx,
  args: {
    deviceId: string;
    workspaceId: string;
    born: DeviceLinkStatus;
    actor: AuditActor;
  },
): Promise<{ status: DeviceLinkStatus; created: boolean } | "device_revoked"> {
  const deviceRows = await tx.execute(
    sql`SELECT revoked_at FROM web.device WHERE id = ${args.deviceId} FOR UPDATE`,
  );
  const deviceRow = deviceRows.rows[0] as { revoked_at: string | null } | undefined;
  if (deviceRow === undefined || deviceRow.revoked_at !== null) {
    return "device_revoked";
  }
  const inserted = await tx.execute(
    sql`INSERT INTO web.device_link (id, device_id, workspace_id, status)
        VALUES (${mintDeviceLinkId()}, ${args.deviceId}, ${args.workspaceId}, ${args.born})
        ON CONFLICT (device_id, workspace_id) DO NOTHING
        RETURNING status`,
  );
  if (inserted.rows.length > 0) {
    await auditInTx(tx, {
      workspaceId: args.workspaceId,
      actor: args.actor,
      kind: "device_linked",
      subject: args.deviceId,
      outcome: "ok",
      details: { status: args.born },
    });
    return { status: args.born, created: true };
  }
  const existing = await tx.execute(
    sql`SELECT status FROM web.device_link
        WHERE device_id = ${args.deviceId} AND workspace_id = ${args.workspaceId}`,
  );
  const status =
    (existing.rows[0] as { status: DeviceLinkStatus } | undefined)?.status ?? args.born;
  return { status, created: false };
}

/**
 * Delete a set of link rows + the linked devices' per-workspace reported per-skill state,
 * inside the caller's transaction — the ONE severing helper every unlink ceremony runs
 * (self unlink, owner remove, seat removal, device revocation). One `device_unlinked` audit
 * row per deleted link, cause-tagged; bytes already on the machine stay there. Reported state
 * dies with the link so a relinked device re-reports fresh.
 */
async function severDeviceLinksTx(
  tx: Tx,
  args: {
    /** The link rows to sever: every (device × workspace) pair this predicate matches. */
    where: ReturnType<typeof sql>;
    actor: AuditActor;
    cause: "self" | "owner_removed" | "seat_removed" | "device_revoked";
  },
): Promise<{ deviceId: string; workspaceId: string }[]> {
  const deleted = await tx.execute(
    sql`DELETE FROM web.device_link WHERE ${args.where}
        RETURNING device_id, workspace_id`,
  );
  const links = (deleted.rows as { device_id: string; workspace_id: string }[]).map((r) => ({
    deviceId: r.device_id,
    workspaceId: r.workspace_id,
  }));
  for (const link of links) {
    await tx.execute(
      sql`DELETE FROM web.device_bundle_state st
          USING web.bundle b
          WHERE st.device_id = ${link.deviceId} AND b.id = st.bundle_id
            AND b.workspace_id = ${link.workspaceId}`,
    );
    await auditInTx(tx, {
      workspaceId: link.workspaceId,
      actor: args.actor,
      kind: "device_unlinked",
      subject: link.deviceId,
      outcome: "ok",
      details: { cause: args.cause },
    });
  }
  return links;
}

/**
 * OWNER remove — a workspace owner severs any link in THEIR workspace (fleet page; the route's
 * owner guard is the gate): the link row + that device's reported state there, `device_unlinked`
 * (cause: removed by owner). Bytes already on the machine stay there — the page copy says so.
 */
export async function ownerRemoveDeviceLink(
  actor: { userId: string; display: string },
  workspaceId: string,
  deviceId: string,
): Promise<"removed" | "unknown_link"> {
  return await getDb().transaction(async (tx) => {
    const severed = await severDeviceLinksTx(tx, {
      where: sql`device_id = ${deviceId} AND workspace_id = ${workspaceId}`,
      actor: { userId: actor.userId, display: actor.display },
      cause: "owner_removed",
    });
    return severed.length > 0 ? "removed" : "unknown_link";
  });
}

/** APPROVE — an owner flips a PENDING link active (fleet page); `link_approved` audited. */
export async function approveDeviceLink(
  actor: { userId: string; display: string },
  workspaceId: string,
  deviceId: string,
): Promise<"approved" | "unknown_link"> {
  return await getDb().transaction(async (tx) => {
    const updated = await tx.execute(
      sql`UPDATE web.device_link SET status = 'active'
          WHERE device_id = ${deviceId} AND workspace_id = ${workspaceId}
            AND status = 'pending'
          RETURNING id`,
    );
    if (updated.rows.length === 0) {
      return "unknown_link";
    }
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "link_approved",
      subject: deviceId,
      outcome: "ok",
    });
    return "approved";
  });
}

/**
 * REJECT — an owner DELETES a pending link (fleet page); `link_rejected` audited. Relinking
 * later is allowed (the row is gone, not tombstoned).
 */
export async function rejectDeviceLink(
  actor: { userId: string; display: string },
  workspaceId: string,
  deviceId: string,
): Promise<"rejected" | "unknown_link"> {
  return await getDb().transaction(async (tx) => {
    const deleted = await tx.execute(
      sql`DELETE FROM web.device_link
          WHERE device_id = ${deviceId} AND workspace_id = ${workspaceId}
            AND status = 'pending'
          RETURNING id`,
    );
    if (deleted.rows.length === 0) {
      return "unknown_link";
    }
    await auditInTx(tx, {
      workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "link_rejected",
      subject: deviceId,
      outcome: "ok",
    });
    return "rejected";
  });
}

/** The link a (device, workspace) pair holds right now — the granted poll's decoration read. */
export async function deviceLinkStatus(
  deviceId: string,
  workspaceId: string,
): Promise<DeviceLinkStatus | null> {
  const rows = await getDb().execute(
    sql`SELECT status FROM web.device_link
        WHERE device_id = ${deviceId} AND workspace_id = ${workspaceId}`,
  );
  return (rows.rows[0] as { status: DeviceLinkStatus } | undefined)?.status ?? null;
}

/**
 * The link lane ops' workspace resolution — one grammar for describe AND apply: a non-empty
 * `workspace` is looked up by NAME in BOTH tenancies; the empty string is the single-tenant
 * origin-addressed form (the install's one workspace) and a refusal in multi (there is no
 * "the" workspace to name). A miss resolves null; the caller folds it into the SAME refusal a
 * seatless member gets — no existence oracle.
 */
async function resolveLinkWorkspace(
  workspaceName: string,
): Promise<typeof workspace.$inferSelect | null> {
  if (workspaceName.length === 0) {
    return composition.tenancy === "multi" ? null : await theWorkspace();
  }
  return await workspaceByName(workspaceName);
}

export type DeviceLinkOp =
  | {
      outcome: "ok";
      workspaceId: string;
      name: string;
      displayName: string;
      role: "owner" | "reviewer" | "member";
      /** The link this device holds NOW ('none' on the describe when no row exists). */
      linkStatus: DeviceLinkStatus | "none";
      /** What a link created now would be born as — the describe's forward look. */
      born: DeviceLinkStatus;
    }
  /** Seatless caller OR unknown workspace name — byte-identical, no existence oracle. */
  | { outcome: "not_a_member" }
  /** The presented device was revoked between the guard and the transaction — the route folds
   * this into the uniform 404 (the same answer the dead credential gets everywhere else). */
  | { outcome: "device_revoked" };

/** The link DESCRIBE (`GET /v1/device/link`): the caller's standing + what apply would do.
 * Nothing mutates. */
export async function describeDeviceLink(
  person: DevicePersonRow,
  workspaceName: string,
): Promise<DeviceLinkOp> {
  const ws = await resolveLinkWorkspace(workspaceName);
  if (ws === null) {
    return { outcome: "not_a_member" };
  }
  const seated = await seatOf(person.userId, ws.id);
  if (seated === undefined) {
    return { outcome: "not_a_member" };
  }
  const status = await deviceLinkStatus(person.deviceId, ws.id);
  return {
    outcome: "ok",
    workspaceId: ws.id,
    name: ws.name,
    displayName: ws.displayName,
    role: seated.role,
    linkStatus: status ?? "none",
    born: linkBornStatus(seated.role, ws.deviceApproval as "off" | "on"),
  };
}

/**
 * The link APPLY (`POST /v1/device/link`): create THIS device's link to the named workspace —
 * born per the ONE rule, `device_linked` audited in the same transaction, IDEMPOTENT (an
 * existing row answers ok with its current status). The seat is locked FOR UPDATE so a
 * concurrent seat removal serializes with the link instead of racing it.
 */
export async function applyDeviceLink(
  person: DevicePersonRow,
  workspaceName: string,
): Promise<DeviceLinkOp> {
  const ws = await resolveLinkWorkspace(workspaceName);
  if (ws === null) {
    return { outcome: "not_a_member" };
  }
  return await getDb().transaction(async (tx) => {
    const seats = await tx.execute(
      sql`SELECT role FROM ${seat}
          WHERE workspace_id = ${ws.id} AND user_id = ${person.userId}
          FOR UPDATE`,
    );
    const seatRow = seats.rows[0] as { role: "owner" | "reviewer" | "member" } | undefined;
    if (seatRow === undefined) {
      return { outcome: "not_a_member" as const };
    }
    const born = linkBornStatus(seatRow.role, await deviceApprovalKnobTx(tx, ws.id));
    const link = await createDeviceLinkTx(tx, {
      deviceId: person.deviceId,
      workspaceId: ws.id,
      born,
      actor: { userId: person.userId, display: person.display },
    });
    if (link === "device_revoked") {
      return { outcome: "device_revoked" as const };
    }
    return {
      outcome: "ok" as const,
      workspaceId: ws.id,
      name: ws.name,
      displayName: ws.displayName,
      role: seatRow.role,
      linkStatus: link.status,
      born,
    };
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
 *
 * `requestedWorkspace` is the workspace ADDRESS SLUG the authorize call named — recorded, not
 * resolved: the flow's workspace is looked up (and the approver's seat in it required) at
 * approval time, inside the approve/deny fence.
 */
export async function startDeviceAuth(
  requestedName: string,
  requestedWorkspace: string,
  /** The invite-link token a `follow <invite-url>` enrollment carries — hashed and RECORDED,
   * never validated here (the unauthenticated start must not be a token oracle); the approval
   * resolves it under its own fence. */
  inviteToken?: string,
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
        requestedWorkspace,
        ...(inviteToken === undefined
          ? {}
          : { inviteTokenSha256: sql`${sha256OfText(inviteToken)}` as never }),
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

/** The first-destination hint an accepted invitation carried, decorated onto a granted poll
 * (`kind` is the bundle catalog's own tag — 'skill' today — or the literal 'channel'). */
export interface DeviceGrantHint {
  kind: string;
  name: string;
}

export type DevicePollResult =
  | { status: "pending" }
  | { status: "denied" }
  | { status: "expired" }
  | {
      status: "granted";
      deviceId: string;
      /** The workspace id the APPROVAL resolved (persisted inside its fence) — the token
       * route's `workspace` decoration reads this immutable id, so a slug rename or a
       * delete+recreate inside the TTL can never re-point a granted flow. */
      approvedWorkspaceId: string | null;
      /** The invitation hint, when the flow carried a token whose invitation names one. */
      hint: DeviceGrantHint | null;
    };

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
    sql`SELECT status, device_id, approved_workspace_id, invite_token_sha256,
               expires_at < now() AS expired
        FROM ${deviceAuthSession}
        WHERE device_code_sha256 = ${sha256OfText(deviceCode)}`,
  );
  const row = rows.rows[0] as
    | {
        status: string;
        device_id: string | null;
        approved_workspace_id: string | null;
        invite_token_sha256: Buffer | null;
        expired: boolean;
      }
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
    return {
      status: "granted",
      deviceId: row.device_id,
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
async function inviteHintByHash(tokenSha256: Buffer): Promise<DeviceGrantHint | null> {
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
 * FENCE 2 — the device-flow approve + mint, one FOR UPDATE transaction: lock the pending row
 * by user_code, re-check liveness under the lock, resolve the flow's workspace (by the
 * recorded slug under the tenancy grammar) and require the approver's SEAT in it, mint the
 * device row (owned by the approver, credential hash = the device_code hash), and flip the
 * row to approved. An unresolvable workspace or a seatless approver returns null — the same
 * answer an expired code gets, so the ceremony is no existence or membership oracle. The
 * approver's session gate runs in the ROUTE before this is called — approval mints a credential
 * that acts as you.
 */
/** The in-transaction abort sentinel: an approval that cannot complete must ROLL BACK any
 * invitation accept it already made (a bare `return null` from a Drizzle transaction COMMITS —
 * only a throw rolls back). Thrown inside the fence, caught at the boundary → the uniform null.
 */
const APPROVE_ABORT = Symbol("device-approve-abort");

export async function approveDeviceAuth(
  userCode: string,
  approver: { userId: string; display: string },
): Promise<{ deviceId: string; requestedName: string } | null> {
  try {
    return await getDb().transaction(async (tx) => {
      const rows = await tx.execute(
        sql`SELECT id, requested_name, requested_workspace, device_code_sha256,
                   invite_token_sha256
            FROM ${deviceAuthSession}
            WHERE user_code = ${userCode} AND status = 'pending' AND expires_at > now()
            FOR UPDATE`,
      );
      const row = rows.rows[0] as
        | {
            id: string;
            requested_name: string;
            requested_workspace: string;
            device_code_sha256: Buffer;
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
      // resolves to — otherwise the accept seated + consumed in a workspace the device is not
      // being approved toward (a crafted flow: invite for A, requested_workspace naming B).
      // Roll the whole thing back rather than commit a split-brain enrollment.
      if (acceptedWorkspaceId !== null && acceptedWorkspaceId !== resolved.workspaceId) {
        throw APPROVE_ABORT;
      }
      const deviceId = mintDeviceId();
      await tx.insert(device).values({
        id: deviceId,
        userId: approver.userId,
        displayName: row.requested_name,
        credentialSha256: row.device_code_sha256,
      });
      // Registration + the FIRST link, one fence: the grant now IS one link — further
      // workspaces each take their own explicit link from the device. Born per the ONE rule
      // (the approver's role vs the workspace's device-approval knob; invitation-woven flows
      // get no exception).
      const firstLink = await createDeviceLinkTx(tx, {
        deviceId,
        workspaceId: resolved.workspaceId,
        born: linkBornStatus(resolved.role, await deviceApprovalKnobTx(tx, resolved.workspaceId)),
        actor: { userId: approver.userId, display: approver.display },
      });
      if (firstLink === "device_revoked") {
        // Unreachable — the device row was inserted in THIS transaction — but a refused link
        // must never commit a linkless grant: roll the whole approval back.
        throw APPROVE_ABORT;
      }
      await tx.execute(
        sql`UPDATE ${deviceAuthSession}
            SET status = 'approved', approved_by = ${approver.userId}, device_id = ${deviceId},
                approved_workspace_id = ${resolved.workspaceId}
            WHERE id = ${row.id}`,
      );
      await auditInTx(tx, {
        workspaceId: resolved.workspaceId,
        actor: { userId: approver.userId, display: approver.display },
        kind: "device_approved",
        subject: deviceId,
        outcome: "ok",
        details: { requestedName: row.requested_name },
      });
      return { deviceId, requestedName: row.requested_name };
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
export async function denyDeviceAuth(
  userCode: string,
  denier: { userId: string; display: string },
): Promise<boolean> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(
      sql`SELECT id, requested_name, requested_workspace FROM ${deviceAuthSession}
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
    await tx.execute(sql`UPDATE ${deviceAuthSession} SET status = 'denied' WHERE id = ${row.id}`);
    await auditInTx(tx, {
      workspaceId: resolved.workspaceId,
      actor: { userId: denier.userId, display: denier.display },
      kind: "device_denied",
      subject: row.requested_name,
      outcome: "ok",
    });
    return true;
  });
}

/** The verify page's resolved request: what is asking, the code for the glance-check, and —
 * when the flow carries an invite token that still resolves — the workspace the invitation
 * would join (disclosed to the code-holder, who is the token-holder's own terminal). The
 * invitation's role rides along so the approval copy can say honestly whether the link will
 * await an owner. */
export interface PendingDeviceAuthView {
  requestedName: string;
  requestedWorkspace: string;
  userCode: string;
  inviteWorkspace: { name: string; displayName: string; role: string } | null;
}

/** The verify page's lookup: the pending request a typed user_code names (display only). */
export async function pendingDeviceAuth(userCode: string): Promise<PendingDeviceAuthView | null> {
  return pendingDeviceAuthWhere(sql`user_code = ${userCode}`);
}

/**
 * The loopback auto-open's lookup: the pending request whose device-code HASH the CLI put in
 * the URL it opened (hex of the same SHA-256 this store already keys the row by — the code
 * itself never enters a URL; a preimage is infeasible, so the challenge identifies without
 * revealing). A malformed challenge is simply a miss.
 */
export async function pendingDeviceAuthByChallenge(
  challengeHex: string,
): Promise<PendingDeviceAuthView | null> {
  if (!/^[0-9a-f]{64}$/.test(challengeHex)) {
    return null;
  }
  return pendingDeviceAuthWhere(sql`device_code_sha256 = decode(${challengeHex}, 'hex')`);
}

async function pendingDeviceAuthWhere(
  cond: ReturnType<typeof sql>,
): Promise<PendingDeviceAuthView | null> {
  const rows = await getDb().execute(
    sql`SELECT s.requested_name, s.requested_workspace, s.user_code,
               w.name AS invite_ws_name, w.display_name AS invite_ws_display,
               i.role AS invite_role
        FROM ${deviceAuthSession} s
        LEFT JOIN web.invitation i ON i.token_sha256 = s.invite_token_sha256
          AND i.status = 'pending' AND (i.expires_at IS NULL OR i.expires_at > now())
        LEFT JOIN ${workspace} w ON w.id = i.workspace_id
        WHERE ${cond} AND s.status = 'pending' AND s.expires_at > now()`,
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

// ── The device lane's actor resolve ─────────────────────────────────────────────────────────

export interface DeviceActorRow {
  deviceId: string;
  userId: string;
  userDisplay: string;
  role: "owner" | "reviewer" | "member";
  /** The device↔workspace link's status — a LIVE row is standing; 'active' is authorization. */
  linkStatus: DeviceLinkStatus;
}

/**
 * credential-hash → device → user → seat → LIVE LINK, one query, fail-closed: a revoked
 * device, a seatless owner, or an unlinked device all resolve to nothing (the route answers
 * the uniform wire 404 — NO row is byte-indistinguishable from a workspace that never
 * existed). A PENDING link resolves WITH its status: exactly two routes answer typed for it
 * (the guard folds everything else to the 404). The hash is computed in Postgres;
 * last_seen_at rides along.
 */
export async function deviceActor(
  workspaceId: string,
  credential: string,
): Promise<DeviceActorRow | null> {
  const rows = await getDb().execute(
    sql`UPDATE ${device} d SET last_seen_at = now()
        FROM ${seat} s, web."user" u, web.device_link dl
        WHERE d.credential_sha256 = ${sha256OfText(credential)}
          AND d.revoked_at IS NULL
          AND u.id = d.user_id
          AND s.user_id = d.user_id AND s.workspace_id = ${workspaceId}
          AND dl.device_id = d.id AND dl.workspace_id = ${workspaceId}
        RETURNING d.id AS device_id, d.user_id,
          -- The display rule (app/lib/person-display.ts): a blank name falls back to the email.
          COALESCE(NULLIF(btrim(u.name), ''), u.email) AS user_display, s.role,
          dl.status AS link_status`,
  );
  const row = rows.rows[0] as
    | {
        device_id: string;
        user_id: string;
        user_display: string;
        role: DeviceActorRow["role"];
        link_status: DeviceLinkStatus;
      }
    | undefined;
  if (!row) {
    return null;
  }
  return {
    deviceId: row.device_id,
    userId: row.user_id,
    userDisplay: row.user_display,
    role: row.role,
    linkStatus: row.link_status,
  };
}

/**
 * Self-service revocation — SELF-ONLY by design (a device is a possession; no owner arm
 * reaches into someone else's pocket), effective immediately and FINAL (the trigger refuses
 * any un-revoke). In the SAME transaction every one of the device's links is severed and its
 * per-workspace reported state deleted — one `device_unlinked` audit row per link (cause:
 * device revoked/signed out); bytes already on the machine stay there.
 *
 * A registration is workspace-LESS — a possession of ONE user. The honest audit scope of the
 * revocation event itself stays every one of the owner's seat workspaces (revocation is an
 * event in each), NOT some single boot workspace the actor may hold no seat in. Zero seats ⇒
 * zero device_revoked rows — no workspace's trail is touched.
 */
export async function revokeOwnDevice(
  actor: { userId: string; display: string },
  deviceId: string,
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
    await severDeviceLinksTx(tx, {
      where: sql`device_id = ${deviceId}`,
      actor: { userId: actor.userId, display: actor.display },
      cause: "device_revoked",
    });
    const seats = await tx.execute(
      sql`SELECT workspace_id FROM ${seat} WHERE user_id = ${actor.userId}`,
    );
    for (const row of seats.rows as { workspace_id: string }[]) {
      await auditInTx(tx, {
        workspaceId: row.workspace_id,
        actor: { userId: actor.userId, display: actor.display },
        kind: "device_revoked",
        subject: deviceId,
        outcome: "ok",
      });
    }
    return true;
  });
}

/**
 * SELF unlink — the device's owner severs ONE link (from the account page): the link row and
 * that workspace's reported state for that device go together; `device_unlinked` (cause:
 * self). Self-only by the WHERE clause itself — a foreign device id matches nothing, the same
 * answer an unknown one gets. Bytes already on the machine stay there; relinking later is
 * allowed and re-reports fresh.
 */
export async function selfUnlinkDevice(
  actor: { userId: string; display: string },
  deviceId: string,
  workspaceId: string,
): Promise<"unlinked" | "unknown_link"> {
  return await getDb().transaction(async (tx) => {
    const severed = await severDeviceLinksTx(tx, {
      where: sql`device_id = ${deviceId} AND workspace_id = ${workspaceId}
        AND device_id IN (SELECT id FROM web.device WHERE user_id = ${actor.userId})`,
      actor: { userId: actor.userId, display: actor.display },
      cause: "self",
    });
    return severed.length > 0 ? "unlinked" : "unknown_link";
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
  hint: DeviceGrantHint | null;
  /** Active bundles the default channels deliver to every member — the pre-accept summary. */
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
 * FOR-UPDATE fence of accept, decline, and the device-approval weave. */
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
      hint: DeviceGrantHint | null;
      alreadyMember: boolean;
      /** Set on the DEVICE-lane accept only: the accepting device's link, created (or found)
       * in the same fence — born per the ONE rule, no invitation exception. */
      linkStatus?: DeviceLinkStatus;
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
 * effects AFTER the seat (the seat-anchoring invariant: a follow without the workspace stays
 * unrepresentable), audit — all under the caller's FOR-UPDATE lock on the invitation row, so
 * two racing accepts serialize and exactly one consumes.
 *
 * `mailboxProven` marks the account-minting path, where possession of the mailed token IS the
 * mailbox proof: the fence is satisfied and the account's email_verified flips true here.
 */
async function acceptInvitationTx(
  tx: Tx,
  inv: LockedInvitation,
  account: SessionAccount,
  opts: { mailboxProven: boolean; deviceId?: string },
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
  // Hint effects — AFTER the seat row, same transaction. The hinted thing may have been
  // deleted since the invite (the FK cleared the column) or archived; then nothing lands.
  let hint: DeviceGrantHint | null = null;
  if (inv.hintBundleId !== null) {
    const named = await tx.execute(
      sql`SELECT kind, name FROM web.bundle
          WHERE id = ${inv.hintBundleId} AND workspace_id = ${inv.workspaceId}
            AND status = 'active'`,
    );
    const row = named.rows[0] as { kind: string; name: string } | undefined;
    if (row) {
      await tx.execute(
        sql`INSERT INTO web.bundle_subscription (user_id, workspace_id, bundle_id, state)
            VALUES (${account.userId}, ${inv.workspaceId}, ${inv.hintBundleId}, 'following')
            ON CONFLICT (user_id, bundle_id)
            DO UPDATE SET state = 'following', updated_at = now()`,
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
        sql`INSERT INTO web.channel_member (channel_id, workspace_id, user_id, added_by)
            VALUES (${inv.hintChannelId}, ${inv.workspaceId}, ${account.userId},
                    ${inv.invitedBy})
            ON CONFLICT (channel_id, user_id) DO NOTHING`,
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
  // The DEVICE-lane accept also links the accepting device in the same fence — born per the
  // ONE rule against the person's ACTUAL seat role (an already-member keeps their real role);
  // invitation-woven links get no exception, so a member's link under an 'on' knob is pending.
  let linkStatus: DeviceLinkStatus | undefined;
  if (opts.deviceId !== undefined) {
    // LOCKED and REQUIRED: an already-member accept holds no lock on its existing seat row (the
    // earlier insert no-ops on conflict), so a concurrent seat removal could delete the seat and
    // sever links BETWEEN a bare read and this link creation — leaving a fresh link that survives
    // the removal. FOR UPDATE serializes with removeSeat's own seat lock; a vanished seat aborts
    // the WHOLE accept (token unconsumed) into the uniform miss.
    const seatRow = await tx.execute(
      sql`SELECT role FROM ${seat}
          WHERE workspace_id = ${inv.workspaceId} AND user_id = ${account.userId}
          FOR UPDATE`,
    );
    const role = (seatRow.rows[0] as { role: "owner" | "reviewer" | "member" } | undefined)?.role;
    if (role === undefined) {
      throw ACCEPT_SEAT_GONE;
    }
    const link = await createDeviceLinkTx(tx, {
      deviceId: opts.deviceId,
      workspaceId: inv.workspaceId,
      born: linkBornStatus(role, await deviceApprovalKnobTx(tx, inv.workspaceId)),
      actor: { userId: account.userId, display: account.display },
    });
    if (link === "device_revoked") {
      // The presented device was revoked between the guard and this fence. Roll the WHOLE
      // accept back (a bare return would COMMIT the consumed token + seat) — the caller
      // answers the uniform miss, exactly what the dead credential would have received had it
      // arrived a moment later; the invitation stays live for a live device or the browser.
      throw ACCEPT_DEVICE_REVOKED;
    }
    linkStatus = link.status;
  }
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
    ...(linkStatus === undefined ? {} : { linkStatus }),
  };
}

/** The device-lane accept's in-transaction abort: the presented device turned out revoked and
 * the whole accept must ROLL BACK (a bare return commits) — caught at the boundary → "gone". */
const ACCEPT_DEVICE_REVOKED = Symbol("invite-accept-device-revoked");
const ACCEPT_SEAT_GONE = Symbol("invite-accept-seat-gone");

/** The invitation page's accept: lock the live row by token, run the fence. The device lane
 * passes its resolved `deviceId` so the accepting device's link lands in the same fence. */
export async function acceptInvitationByToken(
  token: string,
  actor: { userId: string; display: string },
  opts: { mailboxProven: boolean; deviceId?: string },
): Promise<InviteAcceptOutcome> {
  try {
    return await getDb().transaction(async (tx) => {
      const inv = await lockPendingInvitationTx(tx, sql`i.token_sha256 = ${sha256OfText(token)}`);
      if (inv === null) {
        return { outcome: "gone" };
      }
      return acceptInvitationTx(tx, inv, await sessionAccountTx(tx, actor), opts);
    });
  } catch (error) {
    // The revoked-device and vanished-seat rollbacks surface as the uniform "gone" (the
    // invitation is untouched and stays redeemable); any other error is a real fault and
    // propagates.
    if (error === ACCEPT_DEVICE_REVOKED || error === ACCEPT_SEAT_GONE) {
      return { outcome: "gone" };
    }
    throw error;
  }
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

/** A person-scoped resolve's answer: the account facts plus WHICH device presented them. */
export type DevicePersonRow = SessionAccount & { deviceId: string };

/**
 * The PERSON-scoped device resolve: credential → device → user, NO seat or link requirement —
 * the guard of the lane ops whose caller has (or may have) no standing in the target workspace
 * yet: the invitation accept, the link describe/apply, and the global self-revoke. Fail-closed
 * on a revoked device; last_seen_at rides along; the resolved device id rides too (the link
 * ceremonies act on THIS device, never a client-asserted one).
 */
export async function devicePerson(credential: string): Promise<DevicePersonRow | null> {
  const rows = await getDb().execute(
    sql`UPDATE ${device} d SET last_seen_at = now()
        FROM web."user" u
        WHERE d.credential_sha256 = ${sha256OfText(credential)}
          AND d.revoked_at IS NULL
          AND u.id = d.user_id
        RETURNING d.id AS device_id, d.user_id,
          COALESCE(NULLIF(btrim(u.name), ''), u.email) AS user_display,
          u.email, u.email_verified`,
  );
  const row = rows.rows[0] as
    | {
        device_id: string;
        user_id: string;
        user_display: string;
        email: string;
        email_verified: boolean;
      }
    | undefined;
  if (!row) {
    return null;
  }
  return {
    deviceId: row.device_id,
    userId: row.user_id,
    display: row.user_display,
    email: row.email,
    emailVerified: row.email_verified,
  };
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
    // THE SERIALIZATION POINT with the link ceremonies: lock the TARGET's seat row BEFORE any
    // severing. `applyDeviceLink` locks this same row (FOR UPDATE) before inserting its link,
    // so a concurrent apply either committed first — its link is visible to the sever below
    // (each statement reads a fresh snapshot) — or blocks here and finds no seat after this
    // commit (NOT_A_MEMBER). Severing before taking this lock would let an in-flight apply
    // commit an ACTIVE link that survives the removal on an unseated member, silently resuming
    // delivery on a later re-seat. The detach inserts must still PRECEDE the seat delete (the
    // delete cascades the memberships/subscriptions the entitlement predicate reads).
    const targetSeat = await tx.execute(
      sql`SELECT 1 FROM ${seat}
          WHERE workspace_id = ${workspaceId} AND user_id = ${targetUserId}
          FOR UPDATE`,
    );
    if (targetSeat.rows.length === 0) {
      return "missing";
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
    // The removed person's devices are unlinked from THIS workspace in the same fence — their
    // links and per-workspace reported state go with the seat (a re-invited + relinked device
    // re-reports fresh); one device_unlinked audit row per link, cause-tagged.
    await severDeviceLinksTx(tx, {
      where: sql`workspace_id = ${workspaceId}
        AND device_id IN (SELECT id FROM web.device WHERE user_id = ${targetUserId})`,
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
