import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The ROSTER DAL (queries.roster.server.ts) + the invited sign-up's binding leg
 * (identity.server.ts bindInvitedSeats) against a REAL scratch Postgres. A seat IS membership;
 * an invitation is a claim on a FUTURE user in its own table — pending, 7-day-lapsed, re-armed
 * by a re-invite through the pending partial-unique upsert, and converted into a seat ONLY by
 * the verified sign-up ceremony (an expired one binds nothing).
 */

let db: ScratchDb;
let wsId = "";

const DAY_MS = 24 * 60 * 60 * 1000;

async function q() {
  return import("@/lib/db/queries.roster.server");
}

beforeAll(async () => {
  db = await createScratchDb("web_roster");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_ana", "Ana", "ana@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seatUser(db, wsId, "u_ana", "member", "u_owner");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("rosterOf (the members panel read)", () => {
  it("joins seats to their user rows in seat order — display + email are attributes, ids the keys", async () => {
    const queries = await q();
    const rows = await queries.rosterOf(asMember(wsId, "u_ana"));
    expect(rows.map((r) => [r.userId, r.display, r.email, r.role, r.invitedBy])).toEqual([
      ["u_owner", "Owner", "owner@example.com", "owner", null],
      ["u_ana", "Ana", "ana@example.com", "member", "u_owner"],
    ]);
    expect(rows.every((r) => r.createdAt instanceof Date)).toBe(true);
  });
});

describe("foldInviteEmail (the canonical fold)", () => {
  it("lowercases within the closed charset; anything outside is null", async () => {
    const queries = await q();
    expect(queries.foldInviteEmail("  New@Acme.COM ")).toBe("new@acme.com");
    expect(queries.foldInviteEmail("a_b.c+d@x-y.z")).toBe("a_b.c+d@x-y.z");
    expect(queries.foldInviteEmail("")).toBeNull();
    expect(queries.foldInviteEmail("has space@x.y")).toBeNull();
    expect(queries.foldInviteEmail("café@x.y")).toBeNull();
    expect(queries.foldInviteEmail(`${"a".repeat(129)}@x.y`)).toBeNull();
  });
});

describe("createInvitations", () => {
  it("seats pending 7-day claims (folded emails), audited; the members policy admits a member", async () => {
    const queries = await q();
    const before = Date.now();
    expect(
      await queries.createInvitations(
        asMember(wsId, "u_ana", "member", "Ana"),
        ["New@Acme.COM"],
        "members",
      ),
    ).toBe("invited");
    const rows = await db.q<{
      email: string;
      status: string;
      invited_by: string;
      expires_at: Date;
    }>(`SELECT email, status, invited_by, expires_at FROM web.invitation WHERE workspace_id = $1`, [
      wsId,
    ]);
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({
      email: "new@acme.com",
      status: "pending",
      invited_by: "u_ana",
    });
    // The 7-day lapse clock, measured from the write.
    const expiresMs = new Date(rows[0]?.expires_at as unknown as string).getTime();
    expect(expiresMs).toBeGreaterThanOrEqual(before + 7 * DAY_MS - 5000);
    expect(expiresMs).toBeLessThanOrEqual(Date.now() + 7 * DAY_MS + 5000);
    const audit = await db.q(
      `SELECT 1 FROM web.audit_event WHERE workspace_id = $1 AND kind = 'invitation_created' AND subject = 'new@acme.com'`,
      [wsId],
    );
    expect(audit).toHaveLength(1);
  });

  it("under an owners-only policy a plain member is owner_role_required — nothing written", async () => {
    const queries = await q();
    expect(
      await queries.createInvitations(asMember(wsId, "u_ana"), ["gate@acme.com"], "owners"),
    ).toBe("owner_role_required");
    expect(
      await queries.createInvitations(
        asMember(wsId, "u_owner", "owner"),
        ["gate@acme.com"],
        "owners",
      ),
    ).toBe("invited");
  });

  it("a malformed address folds to bad_email — the whole batch refuses, nothing written", async () => {
    const queries = await q();
    expect(
      await queries.createInvitations(
        asMember(wsId, "u_ana"),
        ["ok@acme.com", "bad email"],
        "members",
      ),
    ).toBe("bad_email");
    expect(await db.q(`SELECT 1 FROM web.invitation WHERE email = 'ok@acme.com'`)).toHaveLength(0);
  });

  it("a re-invite re-arms the lapse clock and the inviter through the pending partial-unique upsert", async () => {
    const queries = await q();
    // Age the pending row artificially, then re-invite as a different member.
    await db.q(
      `UPDATE web.invitation SET expires_at = now() + interval '1 day', invited_by = 'u_ana'
       WHERE email = 'new@acme.com'`,
    );
    expect(
      await queries.createInvitations(
        asMember(wsId, "u_owner", "owner", "Owner"),
        ["new@acme.com"],
        "members",
      ),
    ).toBe("invited");
    const rows = await db.q<{ invited_by: string; fresh: boolean; n: string }>(
      `SELECT invited_by, expires_at > now() + interval '6 days' AS fresh,
              (SELECT count(*)::text FROM web.invitation WHERE email = 'new@acme.com') AS n
       FROM web.invitation WHERE email = 'new@acme.com' AND status = 'pending'`,
    );
    expect(rows).toHaveLength(1);
    expect(rows[0]).toMatchObject({ invited_by: "u_owner", fresh: true, n: "1" });
  });
});

describe("pendingInvitationsOf / revokeInvitation", () => {
  it("lists pending rows only; revoke flips ONE pending row and misses on anything else", async () => {
    const queries = await q();
    const pending = await queries.pendingInvitationsOf(asMember(wsId, "u_ana"));
    expect(pending.map((r) => r.email).sort()).toEqual(["gate@acme.com", "new@acme.com"]);

    const target = pending.find((r) => r.email === "gate@acme.com");
    expect(
      await queries.revokeInvitation(asMember(wsId, "u_owner", "owner"), target?.id ?? ""),
    ).toBe("revoked");
    // Re-revoking the same row (no longer pending) and an unknown id both miss.
    expect(
      await queries.revokeInvitation(asMember(wsId, "u_owner", "owner"), target?.id ?? ""),
    ).toBe("missing");
    expect(await queries.revokeInvitation(asMember(wsId, "u_owner", "owner"), "inv_nope")).toBe(
      "missing",
    );
    const after = await queries.pendingInvitationsOf(asMember(wsId, "u_ana"));
    expect(after.map((r) => r.email)).toEqual(["new@acme.com"]);
    const audit = await db.q(
      `SELECT 1 FROM web.audit_event WHERE kind = 'invitation_revoked' AND subject = 'gate@acme.com'`,
    );
    expect(audit).toHaveLength(1);
  });
});

describe("bindInvitedSeats (the verified sign-up's binding leg)", () => {
  it("a live pending invitation binds a seat and flips to accepted", async () => {
    const identity = await import("@/lib/db/identity.server");
    await seedUser(db, "u_new", "New", "new@acme.com");
    expect(await identity.bindInvitedSeats("u_new", "New@Acme.com", "New")).toBe(1);
    const seats = await db.q<{ role: string; invited_by: string }>(
      `SELECT role, invited_by FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_new'`,
      [wsId],
    );
    expect(seats).toEqual([{ role: "member", invited_by: "u_owner" }]);
    const inv = await db.q<{ status: string; accepted_by: string }>(
      `SELECT status, accepted_by FROM web.invitation WHERE email = 'new@acme.com'`,
    );
    expect(inv).toEqual([{ status: "accepted", accepted_by: "u_new" }]);
  });

  it("a pending but EXPIRED invitation binds nothing — the lapse clock is real", async () => {
    const queries = await q();
    const identity = await import("@/lib/db/identity.server");
    expect(
      await queries.createInvitations(
        asMember(wsId, "u_owner", "owner"),
        ["late@acme.com"],
        "members",
      ),
    ).toBe("invited");
    await db.q(
      `UPDATE web.invitation SET expires_at = now() - interval '1 minute' WHERE email = 'late@acme.com'`,
    );
    await seedUser(db, "u_late", "Late", "late@acme.com");
    expect(await identity.bindInvitedSeats("u_late", "late@acme.com", "Late")).toBe(0);
    expect(
      await db.q(`SELECT 1 FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_late'`, [wsId]),
    ).toHaveLength(0);
    // The row stays pending-but-dead: never accepted, never a seat.
    const inv = await db.q<{ status: string }>(
      `SELECT status FROM web.invitation WHERE email = 'late@acme.com'`,
    );
    expect(inv).toEqual([{ status: "pending" }]);
  });
});
