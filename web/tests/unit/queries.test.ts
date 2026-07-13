import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { MemberActor, OwnerActor, UserActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The DAL against a REAL scratch Postgres (created in beforeAll, dropped in afterAll) on the
 * session cluster. Base URL: TEST_DATABASE_URL if set, else the superuser session URL — the
 * scratch database is created off it and owned by that superuser, so no per-table grants are
 * needed to seed the authority tables directly AND to read them through the DAL.
 *
 * The scratch database carries BOTH sides of the seam: the web tier's own tables (created below,
 * mirroring schema.app.ts) AND schema `plane` stood up from the in-repo authority migrations
 * (plane-ddl.ts applies crates/plane-store/migrations/*.sql — the 0010 canonical-principal CHECK
 * and the guarded `topos_*` policy functions included, so a non-canonical seed fails loudly and
 * `inviteMembers` runs the REAL `topos_invite`). The database's search_path is set to
 * `plane, public` so the DAL's unqualified guarded-function call resolves and the web tables in
 * `public` resolve too.
 *
 * Actors are minted here by CAST — the one thing production code must never do (the brand is
 * module-private to guards.server.ts). The helpers mirror the guards' invariants: normalized
 * emails, and the roster-only MemberActor (a confirmed plane seat carrying the directory's role).
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:postgres@localhost:5439/topos_test";
const SCRATCH = `web_queries_test_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

const user = (email: string): UserActor => ({ email: email.trim().toLowerCase() }) as UserActor;
const member = (ws: string, email = "member@example.com"): MemberActor =>
  ({
    email: email.trim().toLowerCase(),
    workspaceId: ws,
    role: "member",
  }) as MemberActor;
const owner = (ws: string, email = "owner@example.com"): OwnerActor =>
  ({
    email: email.trim().toLowerCase(),
    workspaceId: ws,
    role: "owner",
  }) as OwnerActor;

function scratchUrl(): string {
  const url = new URL(ADMIN_URL);
  url.pathname = `/${SCRATCH}`;
  return url.toString();
}

async function adminQuery(sql: string): Promise<void> {
  const client = new Client({ connectionString: ADMIN_URL });
  await client.connect();
  try {
    await client.query(sql);
  } finally {
    await client.end();
  }
}

/** Raw SQL against the scratch DB via the app pool (same superuser; plane.* fully qualified). */
async function scratchQuery<Row extends Record<string, unknown> = Record<string, unknown>>(
  sql: string,
  params: unknown[] = [],
): Promise<Row[]> {
  const { getPool } = await import("@/lib/db/index.server");
  const result = await getPool().query(sql, params);
  return result.rows as Row[];
}

/** Seed one plane workspace row (columns the OSS DDL requires; TEXT ISO-8601 created_at). */
async function seedWorkspace(
  ws: string,
  displayName: string,
  name: string,
  createdAt = "2026-07-01T00:00:00Z",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
     VALUES ($1, $2, 'unverified', 'cloud', $3, $4)`,
    [ws, displayName, createdAt, name],
  );
}

/** Seed one roster seat. Principals must be canonical lowercase — the 0010 CHECK is live. */
async function seedSeat(
  ws: string,
  principal: string,
  role: "owner" | "reviewer" | "member",
  status: "invited" | "confirmed",
  addedAt: string,
  invitedBy?: string,
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, invited_by, added_at)
     VALUES ($1, $2, $3, $4, $5, $6)`,
    [ws, principal, role, status, invitedBy ?? null, addedAt],
  );
}

/** Seed one catalog entry (the identity surface). status defaults 'active'. */
async function seedCatalog(
  ws: string,
  skillId: string,
  name: string,
  displayName: string | null = null,
  status: "active" | "archived" | "deleted" = "active",
  createdAt = "2026-07-01T00:00:00Z",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.catalog (workspace_id, skill_id, name, display_name, status, created_at)
     VALUES ($1, $2, $3, $4, $5, $6)`,
    [ws, skillId, name, displayName, status, createdAt],
  );
}

/** Seed one provenance row; a NULL bundle_digest is a deliberate, schema-honest case. */
async function seedCommit(
  ws: string,
  skillId: string,
  commitHex: string,
  bundleDigestHex: string | null,
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.skill_commit (workspace_id, commit_id, skill_id, bundle_digest)
     VALUES ($1, $2, $3, $4)`,
    [
      ws,
      Buffer.from(commitHex, "hex"),
      skillId,
      bundleDigestHex === null ? null : Buffer.from(bundleDigestHex, "hex"),
    ],
  );
}

/** Seed one current pointer (updated_at is BIGINT epoch-MILLISECONDS; `record` stays NULL). */
async function seedCurrent(
  ws: string,
  skillId: string,
  commitHex: string,
  epoch: number,
  seq: number,
  updatedAtMs: number,
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.current (workspace_id, skill_id, commit_id, epoch, seq, record, updated_at)
     VALUES ($1, $2, $3, $4, $5, NULL, $6)`,
    [ws, skillId, Buffer.from(commitHex, "hex"), epoch, seq, updatedAtMs],
  );
}

/** Seed one proposal row (base_commit_id carries no FK — the commit id doubles for it). */
async function seedProposal(
  ws: string,
  id: string,
  skillId: string,
  commitHex: string,
  baseEpoch: number,
  baseSeq: number,
  status: "open" | "accepted" | "rejected" | "closed",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.proposals
       (workspace_id, id, skill_id, commit_id, base_commit_id, base_epoch, base_seq, status, proposer, resolved_by, created_at)
     VALUES ($1, $2, $3, $4, $4, $5, $6, $7, 'dev-unit', NULL, '2026-07-01T00:00:00Z')`,
    [ws, id, skillId, Buffer.from(commitHex, "hex"), baseEpoch, baseSeq, status],
  );
}

/** Seed/override a workspace policy row (knobs the DAL reads; the invite gate reads invite_policy). */
async function seedPolicy(
  ws: string,
  reviewRequired: number,
  invitePolicy: "members" | "owners" = "members",
  stalenessWindowMs = 604800000,
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace_policy (workspace_id, review_required, invite_policy, staleness_window_ms)
     VALUES ($1, $2, $3, $4)
     ON CONFLICT (workspace_id) DO UPDATE
       SET review_required = EXCLUDED.review_required, invite_policy = EXCLUDED.invite_policy`,
    [ws, reviewRequired, invitePolicy, stalenessWindowMs],
  );
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  // The DAL calls the guarded functions unqualified and reads its own tables unqualified, so the
  // database resolves both `plane` (functions + qualified authority tables) and `public` (web).
  await adminQuery(`ALTER DATABASE ${SCRATCH} SET search_path TO plane, public`);
  installTestEnv({ DATABASE_URL: scratchUrl() });
  // Stand up schema `plane` from the in-repo authority migrations — the REAL authority DDL.
  await applyPlaneDdl(scratchUrl());
  // The web tier's own tables (mirroring schema.app.ts) in `public`.
  const web = new Client({ connectionString: scratchUrl() });
  await web.connect();
  try {
    await web.query(`
      CREATE TABLE public.policy_event (
        id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
        workspace_id text NOT NULL,
        review_required boolean NOT NULL,
        set_by text NOT NULL,
        set_at timestamptz NOT NULL DEFAULT now(),
        outcome text NOT NULL,
        CONSTRAINT policy_event_outcome_check CHECK (outcome IN ('ok', 'denied', 'error'))
      );
      CREATE TABLE public.proposal_comment (
        id uuid PRIMARY KEY,
        workspace_id text NOT NULL,
        skill_id text NOT NULL,
        version_id text NOT NULL,
        author_email text NOT NULL,
        body text NOT NULL,
        created_at timestamptz NOT NULL DEFAULT now(),
        CONSTRAINT proposal_comment_body_check CHECK (char_length(body) BETWEEN 1 AND 4000)
      );
      CREATE INDEX proposal_comment_thread_idx
        ON public.proposal_comment (workspace_id, skill_id, version_id, created_at);
    `);
  } finally {
    await web.end();
  }
});

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE ${SCRATCH} WITH (FORCE)`);
});

async function q() {
  return import("@/lib/db/queries.server");
}

describe("plane DDL fidelity", () => {
  it("the 0010 canonical-principal CHECK is live — a mixed-case seed fails loudly", async () => {
    await seedWorkspace("w_check", "Check", "check");
    await expect(
      seedSeat("w_check", "Mixed@Example.com", "member", "confirmed", "2026-07-01T00:00:00Z"),
    ).rejects.toThrow(/workspace_member_principal_canonical/);
  });
});

describe("planeMembershipsFor (the dashboard roster read)", () => {
  it("lists the email's seats in seat order with the address; only confirmed seats are navigable", async () => {
    const queries = await q();
    const email = "maya@dash.example.com";
    await seedWorkspace("w_dash_conf", "Confirmed WS", "confirmed-ws");
    await seedSeat("w_dash_conf", email, "owner", "confirmed", "2026-07-01T00:00:01Z");
    await seedWorkspace("w_dash_inv", "Invited WS", "invited-ws");
    await seedSeat("w_dash_inv", email, "member", "invited", "2026-07-01T00:00:02Z");

    const rows = await queries.planeMembershipsFor(user(email));
    expect(rows).toEqual([
      {
        id: "w_dash_conf",
        displayName: "Confirmed WS",
        address: "confirmed-ws",
        role: "owner",
        status: "confirmed",
        navigable: true,
      },
      {
        id: "w_dash_inv",
        displayName: "Invited WS",
        address: "invited-ws",
        role: "member",
        status: "invited",
        // An invite promises index visibility, never admission.
        navigable: false,
      },
    ]);
  });

  it("falls back to the workspace id for display name AND address when the workspace row is missing", async () => {
    const queries = await q();
    const email = "orphan@dash.example.com";
    // A seat can outlive its workspace row.
    await seedSeat("w_dash_ghost", email, "member", "confirmed", "2026-07-01T00:00:01Z");
    const rows = await queries.planeMembershipsFor(user(email));
    expect(rows.map((r) => [r.id, r.displayName, r.address])).toEqual([
      ["w_dash_ghost", "w_dash_ghost", "w_dash_ghost"],
    ]);
  });

  it("returns [] for an email with no seats", async () => {
    const queries = await q();
    expect(await queries.planeMembershipsFor(user("nobody@dash.example.com"))).toEqual([]);
  });
});

describe("planeMembership (the guard's roster probe)", () => {
  it("returns the actor's own seat and undefined for none", async () => {
    const queries = await q();
    await seedWorkspace("w_seat", "Seat", "seat");
    await seedSeat("w_seat", "seated@example.com", "reviewer", "invited", "2026-07-01T00:00:01Z");
    expect(await queries.planeMembership(user("Seated@Example.com"), "w_seat")).toEqual({
      role: "reviewer",
      status: "invited",
    });
    expect(await queries.planeMembership(user("stranger@example.com"), "w_seat")).toBeUndefined();
    expect(await queries.planeMembership(user("seated@example.com"), "w_other")).toBeUndefined();
  });
});

describe("planeWorkspaceById", () => {
  it("returns the plane row (with the address) and enforces the workspace scope", async () => {
    const queries = await q();
    await seedWorkspace("w_scope", "Scope", "scope", "2026-07-02T00:00:00Z");
    const row = await queries.planeWorkspaceById(member("w_scope"), "w_scope");
    expect(row).toEqual({
      workspaceId: "w_scope",
      displayName: "Scope",
      verifiedDomain: null,
      verifiedDomainStatus: "unverified",
      deploymentMode: "cloud",
      createdAt: "2026-07-02T00:00:00Z",
      name: "scope",
    });
    // A wrong-workspace member actor fails loudly.
    await expect(queries.planeWorkspaceById(member("w_other"), "w_scope")).rejects.toThrow(
      /workspace-scope mismatch/,
    );
    expect(await queries.planeWorkspaceById(member("w_never"), "w_never")).toBeUndefined();
  });
});

describe("rosterOf (the members panel read)", () => {
  it("returns the workspace roster in seat order (added_at, then principal)", async () => {
    const queries = await q();
    await seedWorkspace("w_roster", "Roster", "roster");
    await seedSeat(
      "w_roster",
      "b@example.com",
      "member",
      "invited",
      "2026-07-04T00:00:02Z",
      "a@example.com",
    );
    await seedSeat("w_roster", "a@example.com", "owner", "confirmed", "2026-07-04T00:00:01Z");
    const rows = await queries.rosterOf(member("w_roster"));
    expect(rows).toEqual([
      {
        workspaceId: "w_roster",
        principal: "a@example.com",
        role: "owner",
        status: "confirmed",
        invitedBy: null,
        addedAt: "2026-07-04T00:00:01Z",
      },
      {
        workspaceId: "w_roster",
        principal: "b@example.com",
        role: "member",
        status: "invited",
        invitedBy: "a@example.com",
        addedAt: "2026-07-04T00:00:02Z",
      },
    ]);
  });
});

describe("workspacePolicyOf", () => {
  it("returns the policy row with the knobs, undefined when absent", async () => {
    const queries = await q();
    await seedWorkspace("w_pol_read", "PolRead", "pol-read");
    expect(await queries.workspacePolicyOf(member("w_pol_read"))).toBeUndefined();
    await seedPolicy("w_pol_read", 1, "owners", 3600000);
    expect(await queries.workspacePolicyOf(member("w_pol_read"))).toEqual({
      workspaceId: "w_pol_read",
      reviewRequired: 1,
      invitePolicy: "owners",
      stalenessWindowMs: 3600000,
    });
  });
});

describe("skillIndexOf / skillIndexRow (the DB skill catalog)", () => {
  const V_A = "aa".repeat(32);
  const V_B = "bb".repeat(32);
  const DIGEST_B = "d1".repeat(32);
  const MS = Date.parse("2026-07-01T12:00:00Z");

  it("lists active catalog entries in NAME order; a pointer-less row renders honestly (nulls)", async () => {
    const queries = await q();
    // "beta" before "alpha" is seeded first on purpose (the index must sort by catalog NAME);
    // "gamma" has a catalog row but NO current pointer — the unpublished state, not an omission.
    await seedCommit("w_idx", "s_beta", V_B, DIGEST_B);
    await seedCurrent("w_idx", "s_beta", V_B, 2, 7, MS + 1000);
    await seedCatalog("w_idx", "s_beta", "beta", null);
    await seedCommit("w_idx", "s_alpha", V_A, null);
    await seedCurrent("w_idx", "s_alpha", V_A, 1, 1, MS);
    await seedCatalog("w_idx", "s_alpha", "alpha", "Deploy Helper");
    await seedCatalog("w_idx", "s_gamma", "gamma", null);

    const rows = await queries.skillIndexOf(member("w_idx"), "w_idx");
    expect(rows).toEqual([
      {
        skillId: "s_alpha",
        name: "alpha",
        displayName: "Deploy Helper",
        status: "active",
        kind: "skill",
        versionId: V_A,
        epoch: 1,
        seq: 1,
        updatedAtMs: MS,
        bundleDigest: null,
        openProposals: 0,
      },
      {
        skillId: "s_beta",
        name: "beta",
        displayName: null,
        status: "active",
        kind: "skill",
        versionId: V_B,
        epoch: 2,
        seq: 7,
        updatedAtMs: MS + 1000,
        bundleDigest: DIGEST_B,
        openProposals: 0,
      },
      {
        skillId: "s_gamma",
        name: "gamma",
        displayName: null,
        status: "active",
        kind: "skill",
        versionId: null,
        epoch: null,
        seq: null,
        updatedAtMs: null,
        bundleDigest: null,
        openProposals: 0,
      },
    ]);
  });

  it("excludes archived/deleted catalog rows", async () => {
    const queries = await q();
    await seedCatalog("w_life", "s_live", "live");
    await seedCatalog("w_life", "s_arch", "arch", null, "archived");
    await seedCatalog("w_life", "s_del", "del", null, "deleted");
    const rows = await queries.skillIndexOf(member("w_life"), "w_life");
    expect(rows.map((r) => r.name)).toEqual(["live"]);
  });

  it("counts ONLY open proposals whose base equals current — the display-only staleness mirror", async () => {
    const queries = await q();
    await seedCommit("w_cnt", "s_x", V_A, DIGEST_B);
    await seedCurrent("w_cnt", "s_x", V_A, 2, 7, MS);
    await seedCatalog("w_cnt", "s_x", "x");
    await seedCommit("w_cnt", "s_y", V_B, DIGEST_B);
    await seedCurrent("w_cnt", "s_y", V_B, 1, 3, MS);
    await seedCatalog("w_cnt", "s_y", "y");
    const prop = "ee".repeat(32);
    await seedCommit("w_cnt", "s_x", prop, DIGEST_B);
    // Counts: open on the live base.
    await seedProposal("w_cnt", "10000000-0000-4000-8000-000000000001", "s_x", prop, 2, 7, "open");
    // STALE: open, but the base is behind current — excluded, like the vault's list.
    await seedProposal("w_cnt", "10000000-0000-4000-8000-000000000002", "s_x", prop, 2, 6, "open");
    // Terminal: on the live base but not open — excluded.
    await seedProposal(
      "w_cnt",
      "10000000-0000-4000-8000-000000000003",
      "s_x",
      prop,
      2,
      7,
      "rejected",
    );
    // Another skill's live open proposal lands on ITS row only.
    const propY = "ef".repeat(32);
    await seedCommit("w_cnt", "s_y", propY, DIGEST_B);
    await seedProposal("w_cnt", "10000000-0000-4000-8000-000000000004", "s_y", propY, 1, 3, "open");

    const rows = await queries.skillIndexOf(member("w_cnt"), "w_cnt");
    expect(rows.map((r) => [r.name, r.openProposals])).toEqual([
      ["x", 1],
      ["y", 1],
    ]);
  });

  it("skillIndexRow resolves ONE catalog row by NAME with its count; undefined when uncataloged", async () => {
    const queries = await q();
    await seedCommit("w_one", "s_solo", V_A, DIGEST_B);
    await seedCurrent("w_one", "s_solo", V_A, 1, 2, MS);
    await seedCatalog("w_one", "s_solo", "solo");
    const row = await queries.skillIndexRow(member("w_one"), "solo");
    expect(row).toEqual({
      skillId: "s_solo",
      name: "solo",
      displayName: null,
      status: "active",
      kind: "skill",
      versionId: V_A,
      epoch: 1,
      seq: 2,
      updatedAtMs: MS,
      bundleDigest: DIGEST_B,
      openProposals: 0,
    });
    expect(await queries.skillIndexRow(member("w_one"), "never")).toBeUndefined();
  });

  it("enforces the workspace scope on the index read", async () => {
    const queries = await q();
    await seedCatalog("w_scoped", "s_s", "s");
    await expect(queries.skillIndexOf(member("w_other"), "w_scoped")).rejects.toThrow(
      /workspace-scope mismatch/,
    );
  });
});

describe("inviteMembers (the guarded topos_invite roster write)", () => {
  it("a confirmed owner invites: outcome 'invited' and a real invited seat lands", async () => {
    const queries = await q();
    await seedWorkspace("w_inv", "Invite WS", "invite-ws");
    await seedSeat("w_inv", "boss@example.com", "owner", "confirmed", "2026-07-01T00:00:01Z");

    const outcome = await queries.inviteMembers(owner("w_inv", "boss@example.com"), [
      "recruit@example.com",
    ]);
    expect(outcome).toBe("invited");

    const rows = await scratchQuery<{ role: string; status: string; invited_by: string }>(
      `SELECT role, status, invited_by FROM plane.workspace_member
       WHERE workspace_id = 'w_inv' AND principal = 'recruit@example.com'`,
    );
    expect(rows).toEqual([{ role: "member", status: "invited", invited_by: "boss@example.com" }]);
  });

  it("a non-member acting is 'member_required' — no seat is written", async () => {
    const queries = await q();
    await seedWorkspace("w_inv_gate", "Gate", "gate");
    const outcome = await queries.inviteMembers(member("w_inv_gate", "stranger@example.com"), [
      "x@example.com",
    ]);
    expect(outcome).toBe("member_required");
    const rows = await scratchQuery(
      `SELECT 1 FROM plane.workspace_member WHERE workspace_id = 'w_inv_gate'`,
    );
    expect(rows).toHaveLength(0);
  });

  it("under an owners-only invite policy, a plain member is 'owner_role_required'", async () => {
    const queries = await q();
    await seedWorkspace("w_inv_pol", "Pol", "inv-pol");
    await seedSeat("w_inv_pol", "plain@example.com", "member", "confirmed", "2026-07-01T00:00:01Z");
    await seedPolicy("w_inv_pol", 0, "owners");
    const outcome = await queries.inviteMembers(member("w_inv_pol", "plain@example.com"), [
      "y@example.com",
    ]);
    expect(outcome).toBe("owner_role_required");
  });

  it("an unknown channel is 'unknown_channel' (resolve-all-or-apply-none) — no seat is written", async () => {
    const queries = await q();
    await seedWorkspace("w_inv_ch", "Chan", "inv-ch");
    await seedSeat("w_inv_ch", "boss@example.com", "owner", "confirmed", "2026-07-01T00:00:01Z");
    const outcome = await queries.inviteMembers(
      owner("w_inv_ch", "boss@example.com"),
      ["z@example.com"],
      ["no-such-channel"],
    );
    expect(outcome).toBe("unknown_channel");
    const rows = await scratchQuery(
      `SELECT 1 FROM plane.workspace_member WHERE workspace_id = 'w_inv_ch' AND principal = 'z@example.com'`,
    );
    expect(rows).toHaveLength(0);
  });
});

describe("policy events", () => {
  it("records every attempt and returns the newest, scoped to the actor's workspace", async () => {
    const queries = await q();
    await queries.recordPolicyEvent(owner("w_pol", "Boss@Example.com"), true, "ok");
    await queries.recordPolicyEvent(owner("w_pol", "boss@example.com"), false, "denied");
    const last = await queries.lastPolicyEvent(member("w_pol"), "w_pol");
    expect(last?.reviewRequired).toBe(false);
    expect(last?.outcome).toBe("denied");
    expect(last?.setBy).toBe("boss@example.com");
    // The scope assert holds on the read.
    await expect(queries.lastPolicyEvent(member("w_other"), "w_pol")).rejects.toThrow(
      /workspace-scope mismatch/,
    );
  });
});

describe("proposal comments (the review thread)", () => {
  const THREAD_VERSION = "ab".repeat(32);
  const COMMENT_A = "11111111-2222-4333-8444-555555555510";
  const COMMENT_B = "11111111-2222-4333-8444-555555555511";

  /** Raw-seed `n` comments into one thread with strictly increasing created_at. */
  async function seedComments(
    ws: string,
    skillId: string,
    versionId: string,
    n: number,
  ): Promise<void> {
    await scratchQuery(
      `INSERT INTO public.proposal_comment (id, workspace_id, skill_id, version_id, author_email, body, created_at)
       SELECT gen_random_uuid(), $1, $2, $3, 'seed@example.com', 'seed ' || i,
              now() - (interval '1 second' * ($4 + 1 - i))
       FROM generate_series(1, $4) AS s(i)`,
      [ws, skillId, versionId, n],
    );
  }

  it("appends and lists oldest-first, scoped to the actor's own workspace", async () => {
    const queries = await q();
    expect(
      await queries.insertProposalComment(member("w_thread", "ana@example.com"), {
        id: COMMENT_A,
        skillId: "s_thread",
        versionId: THREAD_VERSION,
        body: "first look: the script change is safe",
      }),
    ).toBe("inserted");
    await queries.insertProposalComment(member("w_thread", "bo@example.com"), {
      id: COMMENT_B,
      skillId: "s_thread",
      versionId: THREAD_VERSION,
      body: "second",
    });
    const thread = await queries.proposalCommentsFor(
      member("w_thread"),
      "s_thread",
      THREAD_VERSION,
    );
    expect(thread.truncated).toBe(false);
    expect(thread.comments.map((c) => [c.id, c.authorEmail, c.body])).toEqual([
      [COMMENT_A, "ana@example.com", "first look: the script change is safe"],
      [COMMENT_B, "bo@example.com", "second"],
    ]);
    // Another workspace's actor sees nothing — the scope is the actor's, not a parameter.
    expect(
      await queries.proposalCommentsFor(member("w_elsewhere"), "s_thread", THREAD_VERSION),
    ).toEqual({ comments: [], truncated: false });
  });

  it("a replayed client-minted id lands ONE row — ON CONFLICT DO NOTHING, never an error", async () => {
    const queries = await q();
    const outcome = await queries.insertProposalComment(member("w_thread", "ana@example.com"), {
      id: COMMENT_A,
      skillId: "s_thread",
      versionId: THREAD_VERSION,
      body: "a retried submit with different bytes must not duplicate or overwrite",
    });
    expect(outcome).toBe("replayed");
    const thread = await queries.proposalCommentsFor(
      member("w_thread"),
      "s_thread",
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
      .insertProposalComment(member("w_thread"), {
        id: "11111111-2222-4333-8444-555555555512",
        skillId: "s_thread",
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
    await seedComments("w_cap", "s_cap", CAP_VERSION, queries.COMMENT_THREAD_CAP - 1);

    const AT_CAP = "11111111-2222-4333-8444-555555555520";
    expect(
      await queries.insertProposalComment(member("w_cap"), {
        id: AT_CAP,
        skillId: "s_cap",
        versionId: CAP_VERSION,
        body: "the last seat in the thread",
      }),
    ).toBe("inserted");

    const OVER_CAP = "11111111-2222-4333-8444-555555555521";
    expect(
      await queries.insertProposalComment(member("w_cap"), {
        id: OVER_CAP,
        skillId: "s_cap",
        versionId: CAP_VERSION,
        body: "one too many",
      }),
    ).toBe("thread_full");
    const rows = await scratchQuery<{ n: string }>(
      `SELECT count(*)::text AS n FROM public.proposal_comment WHERE workspace_id = 'w_cap'`,
    );
    expect(rows[0]?.n).toBe(String(queries.COMMENT_THREAD_CAP));

    // A replay of an id that DID land is still the idempotent success, even at the cap.
    expect(
      await queries.insertProposalComment(member("w_cap"), {
        id: AT_CAP,
        skillId: "s_cap",
        versionId: CAP_VERSION,
        body: "retried bytes",
      }),
    ).toBe("replayed");

    // The cap is per-THREAD: the same workspace's other threads still accept comments.
    expect(
      await queries.insertProposalComment(member("w_cap"), {
        id: "11111111-2222-4333-8444-555555555522",
        skillId: "s_cap",
        versionId: "ce".repeat(32),
        body: "a different thread breathes freely",
      }),
    ).toBe("inserted");
  });

  it("the display window: newest 200 only, re-sorted ascending, truncation stated honestly", async () => {
    const queries = await q();
    const WINDOW_VERSION = "cf".repeat(32);
    await seedComments("w_window", "s_window", WINDOW_VERSION, queries.COMMENT_DISPLAY_LIMIT + 1);

    const thread = await queries.proposalCommentsFor(
      member("w_window"),
      "s_window",
      WINDOW_VERSION,
    );
    expect(thread.truncated).toBe(true);
    expect(thread.comments).toHaveLength(queries.COMMENT_DISPLAY_LIMIT);
    // The OLDEST comment fell out of the window; the rest render oldest-first.
    expect(thread.comments[0]?.body).toBe("seed 2");
    expect(thread.comments.at(-1)?.body).toBe(`seed ${queries.COMMENT_DISPLAY_LIMIT + 1}`);

    // An exactly-at-the-limit thread is NOT truncated (the one-row probe answers precisely).
    const EXACT_VERSION = "d0".repeat(32);
    await seedComments("w_window", "s_window", EXACT_VERSION, queries.COMMENT_DISPLAY_LIMIT);
    const exact = await queries.proposalCommentsFor(member("w_window"), "s_window", EXACT_VERSION);
    expect(exact.truncated).toBe(false);
    expect(exact.comments).toHaveLength(queries.COMMENT_DISPLAY_LIMIT);
  });
});
