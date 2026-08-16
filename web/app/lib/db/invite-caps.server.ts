import { and, eq, inArray, sql } from "drizzle-orm";
import { composition } from "@/composition.server";
import type { getDb } from "./index.server";
import { invitation } from "./schema.app";

/**
 * The invitation caps — ONE spelling shared by both invitation doors (the members page's
 * `createInvitations` and the session lane's `laneInvite`), enforced INSIDE their existing
 * transactions. These checks ARE the enforcement: route actions bypass every in-process
 * limiter (rate-limit.server.ts says so), so a pre-check throttle outside the transaction
 * would be a decoration, not a cap. The OSS defaults keep everything but the floored daily
 * cap a no-op — a self-hosted team never meets the member cap or a lowered daily cap.
 *
 * Three caps, in the order a submission meets them:
 *  1. per-submission — at most MAX_INVITES_PER_SUBMISSION addresses; over refuses WHOLE.
 *  2. member cap (`members`) — seats + live pending invitations at/over the limit refuses.
 *  3. per-account daily — `invitation_created` audit rows in a rolling day (the same
 *     append-only counting the workspace-create floor uses; rides `audit_actor_user`).
 *     FLOORED: an absent `invites-per-day` row means 10/day while the account is under 48
 *     hours old, else 50/day — and a present row wins even when lower.
 *
 * Plus one per-ADDRESS cooldown that is not a refusal at all: an address invited repeatedly
 * (server-wide — mail leaves the server, not a workspace) is SKIPPED for a while, and the
 * receipt says so per address. Skipped addresses write no rows, so a skip never extends its
 * own cooldown.
 */

/** How many addresses one submission may carry. */
export const MAX_INVITES_PER_SUBMISSION = 10;

/** The floored daily caps (see `invites-per-day` in the entitlements seam). */
const DAILY_FLOOR_YOUNG_ACCOUNT = 10;
const DAILY_FLOOR = 50;
const YOUNG_ACCOUNT_HOURS = 48;

/** The per-address cooldown: this many `invitation_created` rows in the window skips the address. */
const COOLDOWN_MAX_INVITES = 3;
const COOLDOWN_WINDOW_DAYS = 7;

/** The transaction handle the checks run under (any caller's tx). */
type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

export type InviteCapRefusal = "too_many_addresses" | "member_limit" | "invite_limit";

/** The per-submission cap — pure input policy, checked before any transaction opens. */
export function submissionCapRefusal(addressCount: number): "too_many_addresses" | null {
  return addressCount > MAX_INVITES_PER_SUBMISSION ? "too_many_addresses" : null;
}

/**
 * The stateful caps, INSIDE the caller's transaction AND serialized by advisory transaction
 * locks — a count read in a transaction is still a check-then-act race without one (two
 * concurrent submissions would both read the same audit total and both commit past the cap).
 * Lock ORDER is fixed everywhere (inviter → workspace members → addresses, sorted; the accept
 * ceremonies take their members lock BEFORE any invitation-row lock for the same reason) so
 * concurrent transactions can never deadlock. `null` clears both caps. BOTH counts are
 * PROSPECTIVE — what stands plus what THIS submission adds — so a batch can never step over a
 * cap that its first address alone would have cleared: the member count adds the submission's
 * distinct addresses that do not already hold a live pending row (a re-invite adds nothing, so
 * resending at the limit still works), and the daily count adds the whole submission.
 */
export async function inviteCapRefusalInTx(
  tx: Tx,
  args: { workspaceId: string; actorUserId: string; emails: string[] },
): Promise<InviteCapRefusal | null> {
  // Entitlements resolve BEFORE any lock is taken (see the provider contract in
  // topos-web/entitlements.ts) — a provider answer must never extend how long a lock is held.
  const entitlements = await composition.entitlements.forWorkspace(args.workspaceId);
  // The per-inviter lock next — the daily counter always has a floor, so every submission
  // takes it: two concurrent submissions from one account serialize here, and the second
  // reads the first's committed audit rows (held to commit; per-account, so different
  // inviters never queue behind each other).
  await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${`invites:${args.actorUserId}`}))`);
  const distinct = [...new Set(args.emails)];

  const memberLimit = entitlements.limit("members");
  if (memberLimit !== null) {
    // The SAME key the seat-mint backstop takes (`members:<ws>`), so invite-time and
    // accept-time counts serialize with each other, not only with themselves.
    await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${`members:${args.workspaceId}`}))`);
    const standingRows = await tx.execute(
      sql`SELECT
            (SELECT count(*)::int FROM web.seat WHERE workspace_id = ${args.workspaceId})
          + (SELECT count(*)::int FROM web.invitation
              WHERE workspace_id = ${args.workspaceId} AND status = 'pending'
                AND (expires_at IS NULL OR expires_at > now())) AS n`,
    );
    const standing = (standingRows.rows[0] as { n: number } | undefined)?.n ?? 0;
    // What this submission ADDS: its distinct addresses minus those already live-pending here
    // (the upsert re-arms those rows without adding one). Cooldown skips are not subtracted —
    // they are decided later, so the check stays conservative.
    const alreadyPending = await tx
      .select({ email: invitation.email })
      .from(invitation)
      .where(
        and(
          eq(invitation.workspaceId, args.workspaceId),
          eq(invitation.status, "pending"),
          inArray(invitation.email, distinct),
          sql`(${invitation.expiresAt} IS NULL OR ${invitation.expiresAt} > now())`,
        ),
      );
    const adds = distinct.length - new Set(alreadyPending.map((r) => r.email)).size;
    if (standing + adds > memberLimit) {
      return "member_limit";
    }
  }

  // The daily cap always has a value: the entitlement row when present (it wins even when
  // lower), else the account-age floor.
  let dailyCap = entitlements.limit("invites-per-day");
  if (dailyCap === null) {
    const age = await tx.execute(
      sql`SELECT created_at > now() - make_interval(hours => ${YOUNG_ACCOUNT_HOURS}) AS young
          FROM web."user" WHERE id = ${args.actorUserId}`,
    );
    const young = (age.rows[0] as { young: boolean } | undefined)?.young ?? true;
    dailyCap = young ? DAILY_FLOOR_YOUNG_ACCOUNT : DAILY_FLOOR;
  }
  const recent = await tx.execute(
    sql`SELECT count(*)::int AS n FROM web.audit_event
        WHERE kind = 'invitation_created' AND outcome = 'ok'
          AND actor_user_id = ${args.actorUserId}
          AND created_at > now() - interval '24 hours'`,
  );
  const sent = (recent.rows[0] as { n: number } | undefined)?.n ?? 0;
  if (sent + args.emails.length > dailyCap) {
    return "invite_limit";
  }
  return null;
}

/**
 * Take the members-cap advisory lock ALONE — no count. The accept ceremonies call this BEFORE
 * locking their invitation row, so the one lock order (members advisory → invitation row)
 * holds against the invite door, which takes the same advisory key and then upserts rows: an
 * accept that locked its row first while an invite held the advisory key and waited on that
 * row would be a deadlock. A no-op when no `members` limit exists (the OSS default), exactly
 * like the count itself; re-acquiring inside the same transaction later is free.
 */
export async function lockMemberCapInTx(tx: Tx, workspaceId: string): Promise<void> {
  const entitlements = await composition.entitlements.forWorkspace(workspaceId);
  if (entitlements.limit("members") === null) {
    return;
  }
  await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${`members:${workspaceId}`}))`);
}

/**
 * The member cap at SEAT MINT — the backstop behind the invite-time check, inside the accept
 * ceremony's transaction. A no-op when no `members` limit exists (the OSS default). When one
 * does: an advisory transaction lock serializes concurrent accepts against the same workspace
 * (taken BEFORE the ceremony's invitation-row lock via `lockMemberCapInTx`; re-acquired here
 * for callers with no row lock at all), a user already seated is never refused (the seat
 * insert is idempotent — nothing grows), and a full workspace refuses. The workspace-birth
 * owner seat never comes through here — it is exempt by construction.
 */
export async function memberCapReachedInTx(
  tx: Tx,
  workspaceId: string,
  userId: string,
): Promise<boolean> {
  const entitlements = await composition.entitlements.forWorkspace(workspaceId);
  const limit = entitlements.limit("members");
  if (limit === null) {
    return false;
  }
  await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${`members:${workspaceId}`}))`);
  const rows = await tx.execute(
    sql`SELECT
          (SELECT count(*)::int FROM web.seat WHERE workspace_id = ${workspaceId}) AS seats,
          EXISTS(SELECT 1 FROM web.seat
                  WHERE workspace_id = ${workspaceId} AND user_id = ${userId}) AS seated`,
  );
  const row = rows.rows[0] as { seats: number; seated: boolean } | undefined;
  if (row === undefined || row.seated) {
    return false;
  }
  return row.seats >= limit;
}

/**
 * Serialize this submission's ADDRESSES against every concurrent submission touching any of
 * them — taken once, before a door's mint loop, in SORTED unique order (the fixed global
 * order is what makes two overlapping submissions queue instead of deadlock). Without these
 * locks, two concurrent submissions to the same address could both read a cooldown count of
 * COOLDOWN_MAX_INVITES − 1 and both send.
 */
export async function lockInviteAddressesInTx(tx: Tx, emails: string[]): Promise<void> {
  for (const email of [...new Set(emails)].sort()) {
    await tx.execute(sql`SELECT pg_advisory_xact_lock(hashtext(${`invite-addr:${email}`}))`);
  }
}

/**
 * The per-address cooldown, inside the caller's transaction and behind
 * `lockInviteAddressesInTx`: whether THIS address was invited COOLDOWN_MAX_INVITES times or
 * more in the window, server-wide. True = skip the address (not a submission error); the
 * caller reports it as `already invited recently`.
 */
export async function inviteCooldownActiveInTx(tx: Tx, email: string): Promise<boolean> {
  const rows = await tx.execute(
    sql`SELECT count(*)::int AS n FROM web.audit_event
        WHERE kind = 'invitation_created' AND outcome = 'ok'
          AND subject = ${email}
          AND created_at > now() - make_interval(days => ${COOLDOWN_WINDOW_DAYS})`,
  );
  return ((rows.rows[0] as { n: number } | undefined)?.n ?? 0) >= COOLDOWN_MAX_INVITES;
}
