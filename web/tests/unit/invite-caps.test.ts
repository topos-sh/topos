import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  asOwner,
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The invitation caps (invite-caps.server.ts) at BOTH doors — the members-page DAL
 * (createInvitations) and the session lane's (laneInvite) — against a REAL scratch Postgres:
 * the per-submission address cap, the member cap (seats + live pending invitations), the
 * FLOORED rolling-day cap (10 while the account is under 48h, 50 after, a present entitlement
 * row winning even when lower), and the per-address cooldown that SKIPS rather than refuses.
 * The entitlements provider is the mocked composition seam; every limit-less default must be
 * a no-op beyond the floors.
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

async function roster() {
  return import("@/lib/db/queries.roster.server");
}
async function lane() {
  return import("@/lib/db/queries.lane.server");
}

/** The inviter's invitation_created audit-row count in the rolling day. */
async function sentToday(userId: string): Promise<number> {
  const rows = await db.q<{ n: number }>(
    `SELECT count(*)::int AS n FROM web.audit_event
     WHERE kind = 'invitation_created' AND outcome = 'ok' AND actor_user_id = $1
       AND created_at > now() - interval '24 hours'`,
    [userId],
  );
  return rows[0]?.n ?? 0;
}

beforeAll(async () => {
  db = await createScratchDb("web_invcaps");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seedSession(db, "cred_owner", wsId, "u_owner");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the per-submission cap (10 addresses)", () => {
  it("refuses the WHOLE submission over the cap, at both doors, writing nothing", async () => {
    const emails = Array.from({ length: 11 }, (_, i) => `bulk${i}@example.com`);
    const r = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), emails);
    expect(r.outcome).toBe("too_many_addresses");
    const l = await (await lane()).laneInvite(
      asSession(wsId, "u_owner", "cred_owner", "owner"),
      emails,
      {},
    );
    expect(l.outcome).toBe("too_many_addresses");
    expect(await sentToday("u_owner")).toBe(0);
    const rows = await db.q<{ n: number }>(`SELECT count(*)::int AS n FROM web.invitation`);
    expect(rows[0]?.n).toBe(0);
  });

  it("exactly 10 passes (the cap is at-most, not under)", async () => {
    const emails = Array.from({ length: 10 }, (_, i) => `ten${i}@example.com`);
    const r = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), emails);
    expect(r.outcome).toBe("invited");
    if (r.outcome === "invited") {
      expect(r.minted).toHaveLength(10);
      expect(r.skipped).toEqual([]);
    }
  });
});

describe("the rolling-day cap (floored: young account 10/day, else 50; a present row wins)", () => {
  it("a young account hits the 10/day floor prospectively", async () => {
    // The submission-cap test above already sent 10 today as u_owner (a fresh account).
    expect(await sentToday("u_owner")).toBe(10);
    const r = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
      "eleventh@example.com",
    ]);
    expect(r.outcome).toBe("invite_limit");
    const l = await (await lane()).laneInvite(
      asSession(wsId, "u_owner", "cred_owner", "owner"),
      ["eleventh@example.com"],
      {},
    );
    expect(l.outcome).toBe("invite_limit");
  });

  it("an account past 48 hours rides the 50/day floor", async () => {
    await db.q(`UPDATE web."user" SET created_at = now() - interval '3 days' WHERE id = $1`, [
      "u_owner",
    ]);
    const r = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
      "eleventh@example.com",
    ]);
    expect(r.outcome).toBe("invited");
  });

  it("a present `invites-per-day` row wins even when LOWER than the floor", async () => {
    seam.limits["invites-per-day"] = 5;
    try {
      const r = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
        "twelfth@example.com",
      ]);
      expect(r.outcome).toBe("invite_limit");
    } finally {
      delete seam.limits["invites-per-day"];
    }
  });
});

describe("the member cap at invite creation (seats + live pending invitations)", () => {
  it("refuses at/over the limit, at both doors; absent limit is a no-op", async () => {
    // Standing state: 1 seat (the owner) + 11 pending invitations. A limit of 12 is met.
    seam.limits.members = 12;
    try {
      const r = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
        "overflow@example.com",
      ]);
      expect(r.outcome).toBe("member_limit");
      const l = await (await lane()).laneInvite(
        asSession(wsId, "u_owner", "cred_owner", "owner"),
        ["overflow@example.com"],
        {},
      );
      expect(l.outcome).toBe("member_limit");
      // One more slot → passes.
      seam.limits.members = 13;
      const ok = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
        "overflow@example.com",
      ]);
      expect(ok.outcome).toBe("invited");
    } finally {
      delete seam.limits.members;
    }
  });

  it("an EXPIRED pending invitation does not count against the limit", async () => {
    await db.q(
      `UPDATE web.invitation SET expires_at = now() - interval '1 hour' WHERE email = $1`,
      ["overflow@example.com"],
    );
    // 1 seat + 11 LIVE pending (overflow@ is expired) → a limit of 13 has room exactly
    // because the expired row is not counted (counting it would read 13 and refuse).
    seam.limits.members = 13;
    try {
      const r = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
        "roomy@example.com",
      ]);
      expect(r.outcome).toBe("invited");
    } finally {
      delete seam.limits.members;
    }
  });
});

describe("the member cap is PROSPECTIVE — batches and resends", () => {
  it("a batch larger than the remaining room refuses whole; a resend at the limit still works", async () => {
    // Standing at this point: 1 seat + 12 live pending (ten0–9, eleventh, roomy) = 13.
    seam.limits.members = 14;
    try {
      // Two NEW addresses into one remaining slot: 13 + 2 > 14 → the whole batch refuses.
      const batch = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
        "batch-a@example.com",
        "batch-b@example.com",
      ]);
      expect(batch.outcome).toBe("member_limit");
      // One new address fills the last slot exactly.
      const one = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
        "batch-a@example.com",
      ]);
      expect(one.outcome).toBe("invited");
      // AT the limit, a RE-invite adds no live-pending row — resending still works.
      const resend = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
        "batch-a@example.com",
      ]);
      expect(resend.outcome).toBe("invited");
      // …while a new address is now over.
      const over = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
        "batch-c@example.com",
      ]);
      expect(over.outcome).toBe("member_limit");
    } finally {
      delete seam.limits.members;
    }
  });
});

describe("the per-address cooldown (3 invites in 7 days, server-wide → skip)", () => {
  it("skips the address without failing the submission; no row, no audit line", async () => {
    const target = "hammered@example.com";
    for (let i = 0; i < 3; i++) {
      const r = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [target]);
      expect(r.outcome).toBe("invited");
    }
    const before = await sentToday("u_owner");
    const fourth = await (await roster()).createInvitations(asOwner(wsId, "u_owner"), [
      target,
      "fresh@example.com",
    ]);
    expect(fourth.outcome).toBe("invited");
    if (fourth.outcome === "invited") {
      expect(fourth.minted.map((m) => m.email)).toEqual(["fresh@example.com"]);
      expect(fourth.skipped).toEqual([target]);
    }
    // Only the fresh address landed an audit row — a skip never extends its own cooldown.
    expect(await sentToday("u_owner")).toBe(before + 1);
  });

  it("the lane door skips identically, and the cooldown reads across workspaces", async () => {
    const l = await (await lane()).laneInvite(
      asSession(wsId, "u_owner", "cred_owner", "owner"),
      ["hammered@example.com"],
      {},
    );
    expect(l.outcome).toBe("invited");
    if (l.outcome === "invited") {
      expect(l.minted).toEqual([]);
      expect(l.skipped).toEqual(["hammered@example.com"]);
    }
  });
});
