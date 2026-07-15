import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
  asOwner,
  asUser,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * The core DAL (queries.server.ts) against a REAL scratch Postgres carrying both sides of the
 * seam: the app's own `web` schema (the drizzle migrations) and the vault's custody DDL (schema
 * `plane`) — so the catalog reads join the REAL current_pointer/version_digest rows and the
 * proposal-comment cap runs against the live CHECKs. Actors are minted by CAST (the brand is
 * module-private to guards.server.ts); every read derives its scope FROM the actor.
 */

let db: ScratchDb;
let wsId = "";

async function q() {
  return import("@/lib/db/queries.server");
}

beforeAll(async () => {
  db = await createScratchDb("web_queries");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_ana", "Ana", "ana@example.com");
  await seedUser(db, "u_bo", "Bo", "bo@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seatUser(db, wsId, "u_ana", "member");
  await seatUser(db, wsId, "u_bo", "member");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("membershipsFor (the dashboard read)", () => {
  it("lists the seat with the workspace's display name + address; a seat always navigates", async () => {
    const queries = await q();
    expect(await queries.membershipsFor(asUser("u_ana"))).toEqual([
      { id: wsId, displayName: "team", address: "team", role: "member", navigable: true },
    ]);
  });

  it("returns [] for a user holding no seat", async () => {
    const queries = await q();
    await seedUser(db, "u_seatless", "Seatless", "seatless@example.com");
    expect(await queries.membershipsFor(asUser("u_seatless"))).toEqual([]);
  });
});

describe("workspaceById", () => {
  it("returns the row for a member and enforces the workspace scope", async () => {
    const queries = await q();
    const row = await queries.workspaceById(asMember(wsId, "u_ana"), wsId);
    expect(row?.id).toBe(wsId);
    expect(row?.name).toBe("team");
    // A wrong-workspace member actor fails loudly — a bug, never a leak.
    await expect(queries.workspaceById(asMember("w_other", "u_ana"), wsId)).rejects.toThrow(
      /workspace-scope mismatch/,
    );
  });
});

describe("skillIndexOf / skillIndexRow (the catalog reads)", () => {
  it("lists active entries in NAME order; pointer + digest ride in; a pointer-less row renders honestly (nulls)", async () => {
    const queries = await q();
    // "beta" is seeded first on purpose (the index must sort by catalog NAME); "gamma" has a
    // bundle row but NO current pointer — the unpublished state, not an omission.
    await seedBundle(db, wsId, "s_beta", "beta");
    await seedBundle(db, wsId, "s_alpha", "alpha", { displayName: "Deploy Helper" });
    await seedBundle(db, wsId, "s_gamma", "gamma", { withPointer: false });

    const rows = await queries.skillIndexOf(asMember(wsId, "u_ana"), wsId);
    expect(rows.map((r) => r.name)).toEqual(["alpha", "beta", "gamma"]);
    expect(rows[0]).toMatchObject({
      skillId: "s_alpha",
      name: "alpha",
      displayName: "Deploy Helper",
      status: "active",
      kind: "skill",
      versionId: versionIdFor("s_alpha"),
      generation: 1,
      bundleDigest: "d".repeat(64),
      openProposals: 0,
    });
    expect(typeof rows[0]?.updatedAtMs).toBe("number");
    expect(rows[2]).toMatchObject({
      skillId: "s_gamma",
      versionId: null,
      generation: null,
      updatedAtMs: null,
      bundleDigest: null,
    });
  });

  it("excludes archived/deleted bundles from the index", async () => {
    const queries = await q();
    await seedBundle(db, wsId, "s_arch", "arch-archived-2026-07-01", {
      status: "archived",
      baseName: "arch",
    });
    await seedBundle(db, wsId, "s_del", "del-gone", { status: "deleted" });
    const rows = await queries.skillIndexOf(asMember(wsId, "u_ana"), wsId);
    expect(rows.map((r) => r.name)).toEqual(["alpha", "beta", "gamma"]);
  });

  it("counts OPEN proposals per bundle; terminal rows count nothing", async () => {
    const queries = await q();
    const seedProposal = (id: string, bundleId: string, candidate: string, status: string) =>
      db.q(
        `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status, resolved_at)
         VALUES ($1, $2, $3, $4, 'u_ana', $5, CASE WHEN $5 = 'open' THEN NULL ELSE now() END)`,
        [id, wsId, bundleId, candidate, status],
      );
    await seedProposal("p_open_a", "s_alpha", "ee".repeat(32), "open");
    await seedProposal("p_open_a2", "s_alpha", "ef".repeat(32), "open");
    await seedProposal("p_rej_a", "s_alpha", "f0".repeat(32), "rejected");
    await seedProposal("p_open_b", "s_beta", "f1".repeat(32), "open");

    const rows = await queries.skillIndexOf(asMember(wsId, "u_ana"), wsId);
    expect(rows.map((r) => [r.name, r.openProposals])).toEqual([
      ["alpha", 2],
      ["beta", 1],
      ["gamma", 0],
    ]);
  });

  it("skillIndexRow resolves ONE active row by NAME with its count; undefined on a miss", async () => {
    const queries = await q();
    const row = await queries.skillIndexRow(asMember(wsId, "u_ana"), "alpha");
    expect(row).toMatchObject({ skillId: "s_alpha", name: "alpha", openProposals: 2 });
    // Archived names do not resolve here (lifecycle surfaces own them).
    expect(
      await queries.skillIndexRow(asMember(wsId, "u_ana"), "arch-archived-2026-07-01"),
    ).toBeUndefined();
    expect(await queries.skillIndexRow(asMember(wsId, "u_ana"), "never")).toBeUndefined();
  });
});

describe("proposalsOf / proposalByCandidate (the review reads)", () => {
  it("orders open first, then newest-resolved, with the proposer's display joined", async () => {
    const queries = await q();
    const rows = await queries.proposalsOf(asMember(wsId, "u_ana"), "s_alpha");
    expect(rows.map((r) => r.status)).toEqual(["open", "open", "rejected"]);
    expect(rows[0]?.proposedByDisplay).toBe("Ana");
  });

  it("proposalByCandidate keys on the candidate digest; an open row outranks a resolved one", async () => {
    const queries = await q();
    // A re-propose after reject: the same candidate holds both a rejected and an open row.
    await db.q(
      `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status)
       VALUES ('p_reopen', $1, 's_alpha', $2, 'u_bo', 'open')`,
      [wsId, "f0".repeat(32)],
    );
    const row = await queries.proposalByCandidate(
      asMember(wsId, "u_ana"),
      "s_alpha",
      "f0".repeat(32),
    );
    expect(row?.id).toBe("p_reopen");
    expect(row?.status).toBe("open");
    expect(row?.proposedByDisplay).toBe("Bo");
    expect(
      await queries.proposalByCandidate(asMember(wsId, "u_ana"), "s_alpha", "aa".repeat(32)),
    ).toBeUndefined();
  });
});

describe("proposal comments (the review thread, re-keyed to the ONE user identity)", () => {
  const THREAD_VERSION = "ab".repeat(32);
  const COMMENT_A = "11111111-2222-4333-8444-555555555510";
  const COMMENT_B = "11111111-2222-4333-8444-555555555511";

  /** Raw-seed `n` comments into one thread with strictly increasing created_at. */
  async function seedComments(bundleId: string, versionId: string, n: number): Promise<void> {
    await db.q(
      `INSERT INTO web.proposal_comment (id, workspace_id, bundle_id, version_id, author_user_id, author_display, body, created_at)
       SELECT gen_random_uuid(), $1, $2, $3, 'u_ana', 'Ana', 'seed ' || i,
              now() - (interval '1 second' * ($4 + 1 - i))
       FROM generate_series(1, $4) AS s(i)`,
      [wsId, bundleId, versionId, n],
    );
  }

  it("appends with the actor's user id + display snapshot and lists oldest-first", async () => {
    const queries = await q();
    expect(
      await queries.insertProposalComment(asMember(wsId, "u_ana", "member", "Ana"), {
        id: COMMENT_A,
        bundleId: "s_alpha",
        versionId: THREAD_VERSION,
        body: "first look: the script change is safe",
      }),
    ).toBe("inserted");
    await queries.insertProposalComment(asMember(wsId, "u_bo", "member", "Bo"), {
      id: COMMENT_B,
      bundleId: "s_alpha",
      versionId: THREAD_VERSION,
      body: "second",
    });
    const thread = await queries.proposalCommentsFor(
      asMember(wsId, "u_ana"),
      "s_alpha",
      THREAD_VERSION,
    );
    expect(thread.truncated).toBe(false);
    expect(thread.comments.map((c) => [c.id, c.authorUserId, c.authorDisplay, c.body])).toEqual([
      [COMMENT_A, "u_ana", "Ana", "first look: the script change is safe"],
      [COMMENT_B, "u_bo", "Bo", "second"],
    ]);
  });

  it("a replayed client-minted id lands ONE row — ON CONFLICT DO NOTHING, never an error", async () => {
    const queries = await q();
    const outcome = await queries.insertProposalComment(asMember(wsId, "u_ana", "member", "Ana"), {
      id: COMMENT_A,
      bundleId: "s_alpha",
      versionId: THREAD_VERSION,
      body: "a retried submit with different bytes must not duplicate or overwrite",
    });
    expect(outcome).toBe("replayed");
    const thread = await queries.proposalCommentsFor(
      asMember(wsId, "u_ana"),
      "s_alpha",
      THREAD_VERSION,
    );
    // The FIRST write's bytes stand; the replay changed nothing.
    expect(thread.comments.filter((c) => c.id === COMMENT_A).map((c) => c.body)).toEqual([
      "first look: the script change is safe",
    ]);
  });

  it("the 1..4000 body CHECK is live in the schema, not just the action belt", async () => {
    const queries = await q();
    const error: unknown = await queries
      .insertProposalComment(asMember(wsId, "u_ana"), {
        id: "11111111-2222-4333-8444-555555555512",
        bundleId: "s_alpha",
        versionId: THREAD_VERSION,
        body: "x".repeat(4001),
      })
      .then(() => undefined)
      .catch((e: unknown) => e);
    expect(error).toBeDefined();
    const cause = (error as { cause?: { constraint?: string } }).cause;
    expect(cause?.constraint).toBe("proposal_comment_body_check");
  });

  it("the atomic thread cap: comment 500 lands, 501 is thread_full, a replay stays replayed", async () => {
    const queries = await q();
    const CAP_VERSION = "cd".repeat(32);
    await seedComments("s_beta", CAP_VERSION, queries.COMMENT_THREAD_CAP - 1);

    const AT_CAP = "11111111-2222-4333-8444-555555555520";
    expect(
      await queries.insertProposalComment(asMember(wsId, "u_ana"), {
        id: AT_CAP,
        bundleId: "s_beta",
        versionId: CAP_VERSION,
        body: "the last seat in the thread",
      }),
    ).toBe("inserted");

    const OVER_CAP = "11111111-2222-4333-8444-555555555521";
    expect(
      await queries.insertProposalComment(asMember(wsId, "u_ana"), {
        id: OVER_CAP,
        bundleId: "s_beta",
        versionId: CAP_VERSION,
        body: "one too many",
      }),
    ).toBe("thread_full");
    const rows = await db.q<{ n: string }>(
      `SELECT count(*)::text AS n FROM web.proposal_comment WHERE version_id = $1`,
      [CAP_VERSION],
    );
    expect(rows[0]?.n).toBe(String(queries.COMMENT_THREAD_CAP));

    // A replay of an id that DID land is still the idempotent success, even at the cap.
    expect(
      await queries.insertProposalComment(asMember(wsId, "u_ana"), {
        id: AT_CAP,
        bundleId: "s_beta",
        versionId: CAP_VERSION,
        body: "retried bytes",
      }),
    ).toBe("replayed");

    // The cap is per-THREAD: the same workspace's other threads still accept comments.
    expect(
      await queries.insertProposalComment(asMember(wsId, "u_ana"), {
        id: "11111111-2222-4333-8444-555555555522",
        bundleId: "s_beta",
        versionId: "ce".repeat(32),
        body: "a different thread breathes freely",
      }),
    ).toBe("inserted");
  });

  it("the display window: newest 200 only, re-sorted ascending, truncation stated honestly", async () => {
    const queries = await q();
    const WINDOW_VERSION = "cf".repeat(32);
    await seedComments("s_gamma", WINDOW_VERSION, queries.COMMENT_DISPLAY_LIMIT + 1);

    const thread = await queries.proposalCommentsFor(
      asMember(wsId, "u_ana"),
      "s_gamma",
      WINDOW_VERSION,
    );
    expect(thread.truncated).toBe(true);
    expect(thread.comments).toHaveLength(queries.COMMENT_DISPLAY_LIMIT);
    // The OLDEST comment fell out of the window; the rest render oldest-first.
    expect(thread.comments[0]?.body).toBe("seed 2");
    expect(thread.comments.at(-1)?.body).toBe(`seed ${queries.COMMENT_DISPLAY_LIMIT + 1}`);
  });
});

describe("setReviewDefault (the workspace protection default)", () => {
  it("writes protection_default and lands its audit row in the same transaction", async () => {
    const queries = await q();
    expect(await queries.setReviewDefault(asOwner(wsId, "u_owner", "Owner"), true)).toBe("set");
    let rows = await db.q<{ protection_default: string }>(
      `SELECT protection_default FROM web.workspace WHERE id = $1`,
      [wsId],
    );
    expect(rows[0]?.protection_default).toBe("reviewed");

    expect(await queries.setReviewDefault(asOwner(wsId, "u_owner", "Owner"), false)).toBe("set");
    rows = await db.q(`SELECT protection_default FROM web.workspace WHERE id = $1`, [wsId]);
    expect(rows[0]?.protection_default).toBe("open");

    const audit = await db.q<{ subject: string; outcome: string; actor_user_id: string }>(
      `SELECT subject, outcome, actor_user_id FROM web.audit_event
       WHERE workspace_id = $1 AND kind = 'policy_review_default' ORDER BY id`,
      [wsId],
    );
    expect(audit).toEqual([
      { subject: "reviewed", outcome: "ok", actor_user_id: "u_owner" },
      { subject: "open", outcome: "ok", actor_user_id: "u_owner" },
    ]);
  });
});
