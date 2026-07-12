import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The channels DAL against a REAL scratch Postgres carrying the in-repo authority migrations — so
 * the guarded `topos_channel_rename` / `topos_channel_delete` functions, the structural-`everyone`
 * trigger guards, and the trigger-emitted `channel_events` audit are the REAL DDL, not a mock.
 * Actors are minted by CAST (the brand is module-private to guards.server.ts); the DAL reads only
 * `actor.email` + `actor.workspaceId`, and the database re-runs every role gate itself, so casting
 * a member email into an owner-shaped actor exercises the DB's own refusal.
 */
const ADMIN_URL =
  process.env.TEST_DATABASE_URL ?? "postgresql://postgres:postgres@localhost:5439/topos_test";
const SCRATCH = `web_channels_test_${Date.now()}_${Math.floor(Math.random() * 10000)}`;

const member = (ws: string, email = "member@example.com"): MemberActor =>
  ({ email: email.trim().toLowerCase(), workspaceId: ws, role: "member" }) as MemberActor;
const owner = (ws: string, email = "owner@example.com"): OwnerActor =>
  ({ email: email.trim().toLowerCase(), workspaceId: ws, role: "owner" }) as OwnerActor;

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

async function scratchQuery<Row extends Record<string, unknown> = Record<string, unknown>>(
  sql: string,
  params: unknown[] = [],
): Promise<Row[]> {
  const { getPool } = await import("@/lib/db/index.server");
  const result = await getPool().query(sql, params);
  return result.rows as Row[];
}

async function seedWorkspace(ws: string, displayName: string, name: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
     VALUES ($1, $2, 'unverified', 'cloud', '2026-07-01T00:00:00Z', $3)`,
    [ws, displayName, name],
  );
}

/** One roster seat. Principals must be canonical lowercase — the 0010/0015 CHECK is live. */
async function seedSeat(
  ws: string,
  principal: string,
  role: "owner" | "reviewer" | "member",
  status: "invited" | "confirmed",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, added_at)
     VALUES ($1, $2, $3, $4, '2026-07-01T00:00:00Z')`,
    [ws, principal, role, status],
  );
}

/** The catalog identity row a channel reference FKs onto. */
async function seedCatalog(ws: string, skillId: string, name: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.catalog (workspace_id, skill_id, name, status, created_at)
     VALUES ($1, $2, $3, 'active', '2026-07-01T00:00:00Z')`,
    [ws, skillId, name],
  );
}

/** Create the structural `everyone` channel the same way genesis does (builtin = 1). */
async function ensureEveryone(ws: string): Promise<void> {
  await scratchQuery(`SELECT topos_ensure_everyone($1, '2026-07-01T00:00:00Z')`, [ws]);
}

async function seedChannel(
  ws: string,
  channelId: string,
  name: string,
  mode: "open" | "curated" = "open",
): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.channels (workspace_id, channel_id, name, mode, builtin, created_by, created_at)
     VALUES ($1, $2, $3, $4, 0, 'owner@example.com', '2026-07-01T00:00:00Z')`,
    [ws, channelId, name, mode],
  );
}

async function seedChannelSkill(ws: string, channelId: string, skillId: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.channel_skills (workspace_id, channel_id, skill_id, added_by, added_at)
     VALUES ($1, $2, $3, 'owner@example.com', '2026-07-01T00:00:00Z')`,
    [ws, channelId, skillId],
  );
}

async function seedChannelMember(ws: string, channelId: string, principal: string): Promise<void> {
  await scratchQuery(
    `INSERT INTO plane.channel_members (workspace_id, channel_id, principal, added_by, added_at)
     VALUES ($1, $2, $3, NULL, '2026-07-01T00:00:00Z')`,
    [ws, channelId, principal],
  );
}

beforeAll(async () => {
  await adminQuery(`CREATE DATABASE ${SCRATCH}`);
  await adminQuery(`ALTER DATABASE ${SCRATCH} SET search_path TO plane, public`);
  installTestEnv({ DATABASE_URL: scratchUrl() });
  await applyPlaneDdl(scratchUrl());
});

afterAll(async () => {
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await adminQuery(`DROP DATABASE ${SCRATCH} WITH (FORCE)`);
});

async function q() {
  return import("@/lib/db/queries.channels.server");
}

describe("channelsOf", () => {
  it("lists everyone first with the confirmed-roster count; others count their own rows", async () => {
    const queries = await q();
    const ws = "w_ch";
    await seedWorkspace(ws, "Channels WS", "channels-ws");
    // Two confirmed seats + one invited — everyone's structural count is the CONFIRMED two.
    await seedSeat(ws, "owner@example.com", "owner", "confirmed");
    await seedSeat(ws, "worker@example.com", "member", "confirmed");
    await seedSeat(ws, "invited@example.com", "member", "invited");
    await ensureEveryone(ws);
    await seedCatalog(ws, "s_a", "a");
    await seedCatalog(ws, "s_b", "b");
    // A curated channel with nothing on it (proves mode + name-ordering); a normal channel with
    // two skill references and one member.
    await seedChannel(ws, "audits", "audits", "curated");
    await seedChannel(ws, "reviews", "reviews", "open");
    await seedChannelSkill(ws, "reviews", "s_a");
    await seedChannelSkill(ws, "reviews", "s_b");
    await seedChannelMember(ws, "reviews", "worker@example.com");

    const rows = await queries.channelsOf(member(ws, "worker@example.com"));
    expect(rows).toEqual([
      {
        channelId: "everyone",
        name: "everyone",
        mode: "open",
        builtin: true,
        skillCount: 0,
        memberCount: 2,
      },
      {
        channelId: "audits",
        name: "audits",
        mode: "curated",
        builtin: false,
        skillCount: 0,
        memberCount: 0,
      },
      {
        channelId: "reviews",
        name: "reviews",
        mode: "open",
        builtin: false,
        skillCount: 2,
        memberCount: 1,
      },
    ]);
  });
});

describe("channelDetail", () => {
  it("returns the skills (catalog-joined), the members, and the structural everyone note", async () => {
    const queries = await q();
    const ws = "w_detail";
    await seedWorkspace(ws, "Detail WS", "detail-ws");
    await seedSeat(ws, "owner@example.com", "owner", "confirmed");
    await seedSeat(ws, "worker@example.com", "member", "confirmed");
    await ensureEveryone(ws);
    await seedCatalog(ws, "s_deploy", "deploy");
    await seedChannel(ws, "reviews", "reviews", "open");
    await seedChannelSkill(ws, "reviews", "s_deploy");
    await seedChannelMember(ws, "reviews", "worker@example.com");

    const detail = await queries.channelDetail(member(ws), "reviews");
    expect(detail?.name).toBe("reviews");
    expect(detail?.builtin).toBe(false);
    expect(detail?.skills).toEqual([
      { skillId: "s_deploy", name: "deploy", displayName: null, status: "active" },
    ]);
    expect(detail?.members).toEqual([
      { principal: "worker@example.com", addedBy: null, addedAt: "2026-07-01T00:00:00Z" },
    ]);

    // everyone is structural: an empty member list plus the confirmed-roster count (2 here).
    const everyone = await queries.channelDetail(member(ws), "everyone");
    expect(everyone?.builtin).toBe(true);
    expect(everyone?.members).toEqual([]);
    expect(everyone?.confirmedMemberCount).toBe(2);

    // An unknown channel is undefined (the route renders the uniform 404).
    expect(await queries.channelDetail(member(ws), "no-such-channel")).toBeUndefined();
  });
});

describe("channelHistory", () => {
  it("returns trigger-emitted events newest-first with an older-exists marker; unknown 404s", async () => {
    const queries = await q();
    const ws = "w_hist";
    await seedWorkspace(ws, "History WS", "history-ws");
    await seedSeat(ws, "owner@example.com", "owner", "confirmed");
    await ensureEveryone(ws);
    await seedCatalog(ws, "s_h", "h");
    // channel_created, then skill_added — two trigger-emitted rows on this channel.
    await seedChannel(ws, "reviews", "reviews", "open");
    await seedChannelSkill(ws, "reviews", "s_h");

    const full = await queries.channelHistory(member(ws), "reviews");
    expect(full?.hasMore).toBe(false);
    // Newest first: skill_added then channel_created.
    expect(full?.events.map((e) => e.event)).toEqual(["skill_added", "channel_created"]);
    expect(full?.events[0]?.skillId).toBe("s_h");

    // The +1 probe: a window of 1 over 2 events flags older ones exist.
    const windowed = await queries.channelHistory(member(ws), "reviews", { limit: 1 });
    expect(windowed?.hasMore).toBe(true);
    expect(windowed?.events).toHaveLength(1);
    expect(windowed?.events[0]?.event).toBe("skill_added");

    expect(await queries.channelHistory(member(ws), "no-such-channel")).toBeUndefined();
  });
});

describe("renameChannel", () => {
  it("renames a channel; refuses the builtin everyone; refuses a taken or malformed name", async () => {
    const queries = await q();
    const ws = "w_rename";
    await seedWorkspace(ws, "Rename WS", "rename-ws");
    await seedSeat(ws, "owner@example.com", "owner", "confirmed");
    await ensureEveryone(ws);
    await seedChannel(ws, "reviews", "reviews", "open");
    await seedChannel(ws, "audits", "audits", "open");

    // Happy path: the row's name moves, the immutable channel_id does not.
    expect(await queries.renameChannel(owner(ws), "reviews", "reviews-v2")).toBe("renamed");
    const renamed = await scratchQuery<{ name: string }>(
      `SELECT name FROM plane.channels WHERE workspace_id = $1 AND channel_id = 'reviews'`,
      [ws],
    );
    expect(renamed[0]?.name).toBe("reviews-v2");

    // The structural everyone refuses (the trigger guard backs the function's `builtin` code).
    expect(await queries.renameChannel(owner(ws), "everyone", "all")).toBe("builtin");

    // A name another channel already holds refuses — addressed by the IMMUTABLE id (the rename
    // moved only the display name; 'reviews' stays the selector).
    expect(await queries.renameChannel(owner(ws), "reviews", "audits")).toBe("name_taken");

    // The moved-away NAME is not a selector: id-keying means a stale caller misses, never
    // retargets a freed-and-reused name.
    expect(await queries.renameChannel(owner(ws), "reviews-v2", "anything")).toBe(
      "unknown_channel",
    );

    // A malformed name refuses.
    expect(await queries.renameChannel(owner(ws), "audits", "Bad Name")).toBe("bad_name");
  });
});

describe("deleteChannel", () => {
  it("cascades references + memberships, keeps the audit trail, writes no detachment records", async () => {
    const queries = await q();
    const ws = "w_del";
    await seedWorkspace(ws, "Delete WS", "delete-ws");
    await seedSeat(ws, "owner@example.com", "owner", "confirmed");
    await seedSeat(ws, "worker@example.com", "member", "confirmed");
    await ensureEveryone(ws);
    await seedCatalog(ws, "s_x", "x");
    await seedCatalog(ws, "s_y", "y");
    await seedChannel(ws, "doomed", "doomed", "open");
    await seedChannelSkill(ws, "doomed", "s_x");
    await seedChannelSkill(ws, "doomed", "s_y");
    await seedChannelMember(ws, "doomed", "worker@example.com");

    expect(await queries.deleteChannel(owner(ws), "doomed")).toBe("deleted");

    // The channel row and its references + memberships are gone.
    const channelRows = await scratchQuery(
      `SELECT 1 FROM plane.channels WHERE workspace_id = $1 AND channel_id = 'doomed'`,
      [ws],
    );
    expect(channelRows).toHaveLength(0);
    const skillRows = await scratchQuery(
      `SELECT 1 FROM plane.channel_skills WHERE workspace_id = $1 AND channel_id = 'doomed'`,
      [ws],
    );
    expect(skillRows).toHaveLength(0);
    const memberRows = await scratchQuery(
      `SELECT 1 FROM plane.channel_members WHERE workspace_id = $1 AND channel_id = 'doomed'`,
      [ws],
    );
    expect(memberRows).toHaveLength(0);

    // The audit trail SURVIVES the deletion and carries the trigger-emitted deletion trail.
    const events = await scratchQuery<{ event: string }>(
      `SELECT event FROM plane.channel_events WHERE workspace_id = $1 AND channel_id = 'doomed' ORDER BY id`,
      [ws],
    );
    const kinds = events.map((e) => e.event);
    expect(kinds).toContain("channel_deleted");
    expect(kinds.filter((k) => k === "skill_removed")).toHaveLength(2);
    expect(kinds).toContain("member_left");

    // A channel deletion is an UPSTREAM withdrawal, never a person's own detach — no rows here.
    const detachments = await scratchQuery(
      `SELECT 1 FROM plane.skill_detachments WHERE workspace_id = $1`,
      [ws],
    );
    expect(detachments).toHaveLength(0);
  });

  it("refuses to delete the builtin everyone channel", async () => {
    const queries = await q();
    const ws = "w_del_builtin";
    await seedWorkspace(ws, "Delete Builtin WS", "delete-builtin-ws");
    await seedSeat(ws, "owner@example.com", "owner", "confirmed");
    await ensureEveryone(ws);
    expect(await queries.deleteChannel(owner(ws), "everyone")).toBe("builtin");
    const rows = await scratchQuery(
      `SELECT 1 FROM plane.channels WHERE workspace_id = $1 AND channel_id = 'everyone'`,
      [ws],
    );
    expect(rows).toHaveLength(1);
  });
});

describe("the role gate (the database is the authority, not the web actor's shape)", () => {
  it("a plain member is owner_role_required; a stranger is member_required", async () => {
    const queries = await q();
    const ws = "w_gate";
    await seedWorkspace(ws, "Gate WS", "gate-ws");
    await seedSeat(ws, "owner@example.com", "owner", "confirmed");
    await seedSeat(ws, "plain@example.com", "member", "confirmed");
    await ensureEveryone(ws);
    await seedChannel(ws, "reviews", "reviews", "open");

    // A confirmed MEMBER, even shaped as an owner actor, is refused by the DB's own gate.
    expect(await queries.renameChannel(owner(ws, "plain@example.com"), "reviews", "x")).toBe(
      "owner_role_required",
    );
    expect(await queries.deleteChannel(owner(ws, "plain@example.com"), "reviews")).toBe(
      "owner_role_required",
    );

    // No seat at all is member_required.
    expect(await queries.renameChannel(owner(ws, "nobody@example.com"), "reviews", "x")).toBe(
      "member_required",
    );
    expect(await queries.deleteChannel(owner(ws, "nobody@example.com"), "reviews")).toBe(
      "member_required",
    );
  });
});
