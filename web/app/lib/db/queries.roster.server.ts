import { and, asc, eq, inArray, sql } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import {
  auditInTx,
  mintInvitationId,
  mintInviteToken,
  supersedeDeclinedInvitationTx,
} from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { personDisplaySql } from "@/lib/db/person-display.server";
import { invitation, seat } from "@/lib/db/schema.app";
import { user } from "@/lib/db/schema.auth";

/**
 * The ROSTER data access — the members page's reads plus the INVITATION writes. A seat IS
 * membership (user id–keyed); an invitation is a claim on a FUTURE user in its own table —
 * holding one admits nothing, and the verified sign-up ceremony converts it into a seat
 * (identity.server.ts's bindInvitedSeats). The seat mutations themselves (role change, leave,
 * removal — the last-owner-fenced ceremonies) live in identity.server.ts; this module never
 * duplicates them.
 *
 * Inviting REQUIRES armed mail: the mailbox round-trip is the identity rung, so the route
 * refuses invites on an unarmed deployment (the honest "configure mail" prompt) — this layer
 * only writes the rows it is asked to.
 */

/** One roster row as the members panel renders it. */
export interface RosterSeat {
  userId: string;
  display: string;
  /** The login/display address — an attribute, never an authority key. */
  email: string;
  role: "owner" | "reviewer" | "member";
  invitedBy: string | null;
  createdAt: Date;
}

/** The workspace's full roster, seat order. */
export async function rosterOf(actor: MemberActor): Promise<RosterSeat[]> {
  const rows = await getDb()
    .select({
      userId: seat.userId,
      display: personDisplaySql(user),
      email: user.email,
      role: seat.role,
      invitedBy: seat.invitedBy,
      createdAt: seat.createdAt,
    })
    .from(seat)
    .innerJoin(user, eq(user.id, seat.userId))
    .where(eq(seat.workspaceId, actor.workspaceId))
    .orderBy(asc(seat.createdAt), asc(seat.userId));
  return rows.map((r) => ({ ...r, role: r.role as RosterSeat["role"] }));
}

export type InvitationRow = typeof invitation.$inferSelect;

/**
 * The OPEN invitations the members page shows: pending ones (with expiry), plus DECLINED ones —
 * the recorded "no thanks" the inviter should see (re-inviting the address supersedes the
 * declined row). Pending first, then by age.
 */
export async function pendingInvitationsOf(actor: MemberActor): Promise<InvitationRow[]> {
  return getDb()
    .select()
    .from(invitation)
    .where(
      and(
        eq(invitation.workspaceId, actor.workspaceId),
        inArray(invitation.status, ["pending", "declined"]),
      ),
    )
    .orderBy(sql`(${invitation.status} = 'pending') desc`, asc(invitation.createdAt));
}

/** Invitations lapse after seven days; re-inviting re-arms the clock. */
export const INVITATION_TTL_MS = 7 * 24 * 60 * 60 * 1000;

/** One freshly minted invitation: the folded address + the single-use link token (the caller
 * composes the mailed URL; only the token's hash is stored). */
export interface MintedInvitation {
  email: string;
  token: string;
}

/** The optional first-destination hint, pre-resolved to a row id in the actor's workspace. */
export interface InviteHintRef {
  bundleId?: string;
  channelId?: string;
}

export type InviteOutcome =
  | { outcome: "invited"; minted: MintedInvitation[] }
  | { outcome: "owner_role_required" }
  | { outcome: "bad_email" };

const MAX_EMAIL_LEN = 128;
/** Path-safe + the mailbox specials — the same closed charset the wire folds. */
const EMAIL_CHARSET = /^[A-Za-z0-9_.@+-]+$/;

/** Fold an address to its canonical lowercase form, or null when malformed. */
export function foldInviteEmail(email: string): string | null {
  const trimmed = email.trim();
  if (trimmed.length === 0 || trimmed.length > MAX_EMAIL_LEN || !EMAIL_CHARSET.test(trimmed)) {
    return null;
  }
  return trimmed.toLowerCase();
}

/**
 * Seat one or more addresses as PENDING invitations (7-day lapse; re-inviting upserts onto the
 * pending partial-unique, re-arms the clock + the inviter attribution, and mints a FRESH link
 * token — the old link dies with its hash). A prior DECLINED row for the address is superseded
 * (deleted; the audit trail keeps the record). The OWNER gate runs HERE against the actor's
 * role (inviting is owner-only, like revoking); the mail-armed gate and the notice send run in
 * the route. Every write lands its audit row in the same transaction.
 */
export async function createInvitations(
  actor: MemberActor,
  emails: string[],
  hint: InviteHintRef = {},
): Promise<InviteOutcome> {
  if (actor.role !== "owner") {
    return { outcome: "owner_role_required" };
  }
  const folded: string[] = [];
  for (const email of emails) {
    const canonical = foldInviteEmail(email);
    if (canonical === null) {
      return { outcome: "bad_email" };
    }
    folded.push(canonical);
  }
  const expiresAt = new Date(Date.now() + INVITATION_TTL_MS);
  const minted: MintedInvitation[] = [];
  await getDb().transaction(async (tx) => {
    for (const email of folded) {
      const token = mintInviteToken();
      await supersedeDeclinedInvitationTx(tx, actor.workspaceId, email);
      await tx.execute(sql`
        insert into ${invitation}
          (id, workspace_id, email, role, status, invited_by, expires_at,
           token_sha256, hint_bundle_id, hint_channel_id)
        values (${mintInvitationId()}, ${actor.workspaceId}, ${email}, 'member', 'pending',
                ${actor.userId}, ${expiresAt}, sha256(convert_to(${token}, 'UTF8')),
                ${hint.bundleId ?? null}, ${hint.channelId ?? null})
        on conflict (email, workspace_id) where status = 'pending'
        do update set invited_by = excluded.invited_by, expires_at = excluded.expires_at,
                      token_sha256 = excluded.token_sha256,
                      hint_bundle_id = excluded.hint_bundle_id,
                      hint_channel_id = excluded.hint_channel_id,
                      created_at = now()
      `);
      await auditInTx(tx, {
        workspaceId: actor.workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "invitation_created",
        subject: email,
        outcome: "ok",
      });
      minted.push({ email, token });
    }
  });
  return { outcome: "invited", minted };
}

export type RevokeInvitationOutcome = "revoked" | "missing";

/** Revoke ONE pending invitation (owner arm on the members page). */
export async function revokeInvitation(
  actor: MemberActor,
  invitationId: string,
): Promise<RevokeInvitationOutcome> {
  return await getDb().transaction(async (tx) => {
    const rows = await tx.execute(sql`
      update ${invitation} set status = 'revoked'
      where id = ${invitationId} and workspace_id = ${actor.workspaceId} and status = 'pending'
      returning email
    `);
    const row = rows.rows[0] as { email: string } | undefined;
    if (row === undefined) {
      return "missing";
    }
    await auditInTx(tx, {
      workspaceId: actor.workspaceId,
      actor: { userId: actor.userId, display: actor.display },
      kind: "invitation_revoked",
      subject: row.email,
      outcome: "ok",
    });
    return "revoked";
  });
}
