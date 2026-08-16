import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The member cap's SEAT-MINT backstop (`members`) — the accept ceremonies against a REAL
 * scratch Postgres: a full workspace refuses the accept (`workspace_full`) and CONSUMES
 * NOTHING (the invitation stays pending, so a wider limit lets the same link succeed later);
 * an accepter who already holds a seat is never refused (nothing grows); the invited sign-up's
 * binding leg SKIPS a full workspace instead of consuming its invitation. Absent limit — the
 * OSS default — is a no-op throughout.
 */

const seam = vi.hoisted(() => ({
  limits: {} as Record<string, number>,
}));
vi.mock("@/composition.server", () => ({
  composition: {
    tenancy: "single",
    registration: "gated",
    reservedWorkspaceNames: [],
    entitlements: {
      forWorkspace: async () => ({
        allows: () => true,
        limit: (key: string) => seam.limits[key] ?? null,
      }),
    },
  },
}));

let db: ScratchDb;
let wsId = "";
let tokenCounter = 0;

async function identity() {
  return import("@/lib/db/identity.server");
}

/** Seed one live pending invitation; returns the plaintext token the accept presents. */
async function seedInvitation(email: string): Promise<string> {
  tokenCounter += 1;
  const token = `tok-${tokenCounter}-${email}`;
  await db.q(
    `INSERT INTO web.invitation (id, workspace_id, email, role, status, expires_at, token_sha256)
     VALUES ($1, $2, $3, 'member', 'pending', now() + interval '7 days',
             sha256(convert_to($4, 'UTF8')))`,
    [`i_${tokenCounter}`, wsId, email, token],
  );
  return token;
}

async function seatCount(): Promise<number> {
  const rows = await db.q<{ n: number }>(
    `SELECT count(*)::int AS n FROM web.seat WHERE workspace_id = $1`,
    [wsId],
  );
  return rows[0]?.n ?? 0;
}

async function invitationStatus(email: string): Promise<string | undefined> {
  const rows = await db.q<{ status: string }>(
    `SELECT status FROM web.invitation WHERE email = $1 ORDER BY created_at DESC LIMIT 1`,
    [email],
  );
  return rows[0]?.status;
}

beforeAll(async () => {
  db = await createScratchDb("web_memcap");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("acceptInvitationByToken under a members limit", () => {
  it("admits while there is room, refuses `workspace_full` at the limit — the row stays pending", async () => {
    seam.limits.members = 2;
    try {
      await seedUser(db, "u_bea", "Bea", "bea@example.com");
      const tokenBea = await seedInvitation("bea@example.com");
      const admitted = await (await identity()).acceptInvitationByToken(
        tokenBea,
        { userId: "u_bea", display: "Bea" },
        { mailboxProven: true },
      );
      expect(admitted.outcome).toBe("accepted");
      expect(await seatCount()).toBe(2);

      await seedUser(db, "u_cal", "Cal", "cal@example.com");
      const tokenCal = await seedInvitation("cal@example.com");
      const refused = await (await identity()).acceptInvitationByToken(
        tokenCal,
        { userId: "u_cal", display: "Cal" },
        { mailboxProven: true },
      );
      expect(refused.outcome).toBe("workspace_full");
      expect(await seatCount()).toBe(2);
      // NOTHING consumed: the invitation stands, so a wider limit admits the SAME link.
      expect(await invitationStatus("cal@example.com")).toBe("pending");
      // The MAILBOX PROOF survives the refusal: possession of the mailed token proved the
      // address whatever the seat count says, so the account is not a verification
      // round-trip poorer when it comes back.
      const verified = await db.q<{ email_verified: boolean }>(
        `SELECT email_verified FROM web."user" WHERE id = 'u_cal'`,
      );
      expect(verified[0]?.email_verified).toBe(true);
      seam.limits.members = 3;
      const admittedLater = await (await identity()).acceptInvitationByToken(
        tokenCal,
        { userId: "u_cal", display: "Cal" },
        { mailboxProven: true },
      );
      expect(admittedLater.outcome).toBe("accepted");
      expect(await seatCount()).toBe(3);
    } finally {
      delete seam.limits.members;
    }
  });

  it("an accepter who ALREADY holds a seat is never refused at the limit (nothing grows)", async () => {
    seam.limits.members = 3;
    try {
      const token = await seedInvitation("bea@example.com");
      const again = await (await identity()).acceptInvitationByToken(
        token,
        { userId: "u_bea", display: "Bea" },
        { mailboxProven: true },
      );
      expect(again.outcome).toBe("accepted");
      if (again.outcome === "accepted") {
        expect(again.alreadyMember).toBe(true);
      }
      expect(await seatCount()).toBe(3);
    } finally {
      delete seam.limits.members;
    }
  });
});

describe("bindInvitedSeats under a members limit", () => {
  it("skips a full workspace — the invitation stays pending and binds once there is room", async () => {
    await seedUser(db, "u_dee", "Dee", "dee@example.com");
    await seedInvitation("dee@example.com");
    seam.limits.members = 3;
    try {
      const boundAtLimit = await (await identity()).bindInvitedSeats(
        "u_dee",
        "dee@example.com",
        "Dee",
      );
      expect(boundAtLimit).toBe(0);
      expect(await seatCount()).toBe(3);
      expect(await invitationStatus("dee@example.com")).toBe("pending");
      seam.limits.members = 10;
      const boundLater = await (await identity()).bindInvitedSeats(
        "u_dee",
        "dee@example.com",
        "Dee",
      );
      expect(boundLater).toBe(1);
      expect(await seatCount()).toBe(4);
      expect(await invitationStatus("dee@example.com")).toBe("accepted");
    } finally {
      delete seam.limits.members;
    }
  });
});
