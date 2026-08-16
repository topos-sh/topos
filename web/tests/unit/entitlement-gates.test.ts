import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  asOwner,
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The per-workspace entitlement gates against a REAL scratch Postgres, all three consulted
 * through the mocked composition seam:
 *  - `reviews` (allows) — ENABLING review protection refuses at every door (the lane's
 *    skill/channel protect, the web pin, the workspace default); disabling never does, and an
 *    already-'reviewed' bundle keeps its row (no retroactive strip).
 *  - `bundles` (limit) — a NEW bundle identity at the cap refuses BEFORE any custody call
 *    (publishGenesisBundle answers `refused` with no vault in the room at all); absent = no-op.
 *  - `history-days` (limit) — a version older than the window reads outside it; absent = no-op;
 *    a version the mirror does not hold is never this check's refusal.
 */

const seam = vi.hoisted(() => ({
  limits: {} as Record<string, number>,
  allows: {} as Record<string, boolean>,
}));
vi.mock("@/composition.server", () => ({
  composition: {
    tenancy: "single",
    registration: "gated",
    reservedWorkspaceNames: [],
    entitlements: {
      forWorkspace: async () => ({
        allows: (key: string) => seam.allows[key] ?? true,
        limit: (key: string) => seam.limits[key] ?? null,
      }),
    },
  },
}));

let db: ScratchDb;
let wsId = "";

beforeAll(async () => {
  db = await createScratchDb("web_entgates");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seedSession(db, "cred_owner", wsId, "u_owner");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the reviews gate (`allows('reviews')`)", () => {
  it("withheld: every ENABLE door refuses; loosening still lands; standing rows keep working", async () => {
    const lane = await import("@/lib/db/queries.lane.server");
    const lifecycle = await import("@/lib/db/queries.lifecycle.server");
    const queries = await import("@/lib/db/queries.server");
    const owner = asOwner(wsId, "u_owner");
    const session = asSession(wsId, "u_owner", "cred_owner", "owner");
    await seedBundle(db, wsId, "b_guarded", "guarded");
    // A bundle protected BEFORE the entitlement was withdrawn.
    await db.q(`UPDATE web.bundle SET protection = 'reviewed' WHERE id = 'b_guarded'`);

    seam.allows.reviews = false;
    try {
      expect(await lane.laneProtectBundle(session, "b_guarded", "reviewed")).toBe(
        "reviews_unavailable",
      );
      expect(await lane.laneProtectChannel(session, "everyone", "curated")).toBe(
        "reviews_unavailable",
      );
      expect(await lifecycle.setBundleProtection(owner, "b_guarded", "reviewed")).toEqual({
        outcome: "reviews_unavailable",
      });
      expect(await queries.setReviewDefault(owner, true)).toBe("reviews_unavailable");
      // NO retroactive strip: the standing pin is untouched by the refusals above.
      const rows = await db.q<{ protection: string | null }>(
        `SELECT protection FROM web.bundle WHERE id = 'b_guarded'`,
      );
      expect(rows[0]?.protection).toBe("reviewed");
      // Loosening is never gated.
      expect(await lane.laneProtectBundle(session, "b_guarded", "open")).toBe("set");
      expect(await queries.setReviewDefault(owner, false)).toBe("set");
      expect(await lifecycle.setBundleProtection(owner, "b_guarded", null)).toEqual({
        outcome: "set",
      });
    } finally {
      delete seam.allows.reviews;
    }
  });

  it("open (the OSS default): enabling lands as before", async () => {
    const lane = await import("@/lib/db/queries.lane.server");
    expect(
      await lane.laneProtectBundle(
        asSession(wsId, "u_owner", "cred_owner", "owner"),
        "b_guarded",
        "reviewed",
      ),
    ).toBe("set");
  });
});

describe("the bundle cap (`bundles`)", () => {
  it("a NEW identity at the cap refuses before any custody call; absent limit is a no-op", async () => {
    const genesis = await import("@/lib/api/genesis.server");
    const session = asSession(wsId, "u_owner", "cred_owner", "owner");
    const candidate = {
      files: [{ path: "SKILL.md", mode: "100644", content_base64: "aGVsbG8=" }],
      attribution: "Owner",
      message: "genesis",
    };
    // One active bundle stands (b_guarded). At a limit of 1, a new identity refuses — and no
    // vault is running in this suite, so the refusal PROVES no custody call was attempted.
    seam.limits.bundles = 1;
    try {
      const refused = await genesis.publishGenesisBundle({
        actor: session,
        kind: "skill",
        candidate,
        displayName: "capped",
        destination: null,
      });
      expect(refused.kind).toBe("refused");
      if (refused.kind === "refused") {
        expect(refused.refusal.code).toBe("BUNDLE_LIMIT_REACHED");
        expect(refused.refusal.message).toBe("This workspace is at its bundle limit.");
      }
      const rows = await db.q<{ n: number }>(
        `SELECT count(*)::int AS n FROM web.bundle WHERE workspace_id = $1`,
        [wsId],
      );
      expect(rows[0]?.n).toBe(1);
    } finally {
      delete seam.limits.bundles;
    }
  });

  it("archived bundles do not count against the cap (active rows only)", async () => {
    const custody = await import("@/lib/db/queries.custody.server");
    await db.q(
      `UPDATE web.bundle SET status = 'archived', archived_at = now() WHERE id = 'b_guarded'`,
    );
    try {
      seam.limits.bundles = 1;
      const session = asSession(wsId, "u_owner", "cred_owner", "owner");
      expect(await custody.bundleCapRefusal(session)).toBeNull();
      seam.limits.bundles = 0;
      const refused = await custody.bundleCapRefusal(session);
      expect(refused?.code).toBe("BUNDLE_LIMIT_REACHED");
    } finally {
      delete seam.limits.bundles;
      await db.q(
        `UPDATE web.bundle SET status = 'active', archived_at = NULL WHERE id = 'b_guarded'`,
      );
    }
  });
});

describe("the bundle cap covers UNARCHIVE (archived→active is the same step past it)", () => {
  it("restoring at the cap refuses typed; a wider limit restores", async () => {
    const lifecycle = await import("@/lib/db/queries.lifecycle.server");
    const owner = asOwner(wsId, "u_owner");
    // Archive b_guarded by rows (the archive ceremony's resulting shape), then stand up a
    // replacement — the archive-then-replace-then-unarchive shuffle the cap must refuse.
    await db.q(
      `UPDATE web.bundle
       SET status = 'archived', archived_at = now(), base_name = name,
           name = name || '-archived-2026-01-01'
       WHERE id = 'b_guarded'`,
    );
    await seedBundle(db, wsId, "b_replacement", "replacement");
    seam.limits.bundles = 1;
    try {
      const refused = await lifecycle.unarchiveBundle(owner, "b_guarded");
      expect(refused.outcome).toBe("bundle_limit");
      const rows = await db.q<{ status: string }>(
        `SELECT status FROM web.bundle WHERE id = 'b_guarded'`,
      );
      expect(rows[0]?.status).toBe("archived");
      seam.limits.bundles = 2;
      const restored = await lifecycle.unarchiveBundle(owner, "b_guarded");
      expect(restored.outcome).toBe("unarchived");
    } finally {
      delete seam.limits.bundles;
    }
  });
});

describe("the history window (`history-days`)", () => {
  it("older-than-window reads outside; inside and absent-limit do not; unknown versions never do", async () => {
    const custody = await import("@/lib/db/queries.custody.server");
    const session = asSession(wsId, "u_owner", "cred_owner", "owner");
    const oldV = "a1".repeat(32);
    const newV = "b2".repeat(32);
    await db.q(
      `INSERT INTO plane.version (workspace_id, bundle_id, version_id, commit_id, author_display, created_at)
       VALUES ($1, 'b_guarded', $2, $3, 'Owner', now() - interval '40 days'),
              ($1, 'b_guarded', $4, $5, 'Owner', now() - interval '5 days')`,
      [wsId, oldV, "c1".repeat(20), newV, "d1".repeat(20)],
    );
    // No window (the OSS default): nothing is outside.
    expect(await custody.versionOutsideHistoryWindow(session, "b_guarded", oldV)).toBe(false);
    seam.limits["history-days"] = 30;
    try {
      expect(await custody.versionOutsideHistoryWindow(session, "b_guarded", oldV)).toBe(true);
      expect(await custody.versionOutsideHistoryWindow(session, "b_guarded", newV)).toBe(false);
      // A version the mirror does not hold is not this check's refusal.
      expect(await custody.versionOutsideHistoryWindow(session, "b_guarded", "e3".repeat(32))).toBe(
        false,
      );
      // The page's annotation read: creation times for exactly the versions the mirror holds.
      const map = await custody.versionCreatedAtMap(session, "b_guarded", [
        oldV,
        newV,
        "e3".repeat(32),
      ]);
      expect(map.size).toBe(2);
      expect(map.get(oldV)).toBeInstanceOf(Date);
    } finally {
      delete seam.limits["history-days"];
    }
  });
});
