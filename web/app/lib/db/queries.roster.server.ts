import { and, asc, eq, sql } from "drizzle-orm";
import type { MemberActor } from "@/lib/auth/guards.server";
import { auditInTx, mintInvitationId } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
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
      display: user.name,
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

/** The PENDING invitations (status + expiry render on the members page), newest first. */
export async function pendingInvitationsOf(actor: MemberActor): Promise<InvitationRow[]> {
  return getDb()
    .select()
    .from(invitation)
    .where(and(eq(invitation.workspaceId, actor.workspaceId), eq(invitation.status, "pending")))
    .orderBy(asc(invitation.createdAt));
}

/** Invitations lapse after seven days; re-inviting re-arms the clock. */
export const INVITATION_TTL_MS = 7 * 24 * 60 * 60 * 1000;

export type InviteOutcome = "invited" | "owner_role_required" | "bad_email";

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
 * pending partial-unique and re-arms the clock + the inviter attribution). The invite-policy
 * gate runs HERE against the actor's role (policy 'owners' refuses a plain member); the
 * mail-armed gate and the notice send run in the route. Every write lands its audit row in the
 * same transaction.
 */
export async function createInvitations(
  actor: MemberActor,
  emails: string[],
  invitePolicy: "members" | "owners",
): Promise<InviteOutcome> {
  if (invitePolicy === "owners" && actor.role !== "owner") {
    return "owner_role_required";
  }
  const folded: string[] = [];
  for (const email of emails) {
    const canonical = foldInviteEmail(email);
    if (canonical === null) {
      return "bad_email";
    }
    folded.push(canonical);
  }
  const expiresAt = new Date(Date.now() + INVITATION_TTL_MS);
  await getDb().transaction(async (tx) => {
    for (const email of folded) {
      await tx.execute(sql`
        insert into ${invitation} (id, workspace_id, email, role, status, invited_by, expires_at)
        values (${mintInvitationId()}, ${actor.workspaceId}, ${email}, 'member', 'pending', ${actor.userId}, ${expiresAt})
        on conflict (email, workspace_id) where status = 'pending'
        do update set invited_by = excluded.invited_by, expires_at = excluded.expires_at,
                      created_at = now()
      `);
      await auditInTx(tx, {
        workspaceId: actor.workspaceId,
        actor: { userId: actor.userId, display: actor.display },
        kind: "invitation_created",
        subject: email,
        outcome: "ok",
      });
    }
  });
  return "invited";
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
