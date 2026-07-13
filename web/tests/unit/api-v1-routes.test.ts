import { Client } from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { action as channelsLoader } from "@/routes/api.v1.channel-membership";
import { action as channelProtAction } from "@/routes/api.v1.channel-protection";
import { loader as channelIndexLoader } from "@/routes/api.v1.channels";
import { action as curationAction } from "@/routes/api.v1.curation";
import { action as exclusionsAction } from "@/routes/api.v1.exclusions";
import { action as followsAction } from "@/routes/api.v1.follows";
import { action as invitationsAction } from "@/routes/api.v1.invitations";
import { loader as meLoader, action as meWrongMethod } from "@/routes/api.v1.me";
import { action as noticesAction } from "@/routes/api.v1.notices-ack";
import { loader as reportWrongMethod } from "@/routes/api.v1.report";
import { action as skillProtAction } from "@/routes/api.v1.skill-protection";
import { loader as reachLoader } from "@/routes/api.v1.skill-reach";
import { applyPlaneDdl } from "../helpers/plane-ddl";
import { installTestEnv } from "./helpers/test-env";

/**
 * The device-lane member row-op routes, driven end to end against a REAL scratch Postgres stood up
 * from the in-repo authority migrations — AS the scoped `topos_web` role, so 0019's grants (the row
 * reads, the guarded `topos_*` EXECUTE, the column-grain DML) are exercised exactly as production
 * runs them. Migrations apply AS `topos_plane` (the owner) so the grant block fires; rows are seeded
 * by a superuser client (bypassing grants); the ROUTE loaders/actions are then invoked directly with
 * constructed Requests, proving the WIRE BYTES field-for-field.
 *
 * The two byte-decorated reads (`/proposals`, `/skills/{skill}/log`) are NOT here — they read git
 * commit author/message from the vault's byte custody and stay on the vault's splat forwarder.
 */

const ROOT_URL =
  process.env.TEST_DATABASE_URL ?? "postgres://postgres:postgres@localhost:5439/postgres";
const SCRATCH = `web_devlane_${Date.now()}_${Math.floor(Math.random() * 100000)}`;

function urlFor(role: string, secret: string, db: string): string {
  const u = new URL(ROOT_URL);
  u.username = role;
  u.password = secret;
  u.pathname = `/${db}`;
  return u.toString();
}
const scratchName = SCRATCH;
// Passwords match the rest of the suite (`plane`/`web`) — the roles are cluster-global, so a
// divergent convention here would clash with the Rust e2e / db-setup on a shared local Postgres.
const planeUrl = () => urlFor("topos_plane", "plane", scratchName);
const webUrl = () => urlFor("topos_web", "web", scratchName);
const superScratchUrl = () => {
  const u = new URL(ROOT_URL);
  u.pathname = `/${scratchName}`;
  return u.toString();
};

const WS = "w_dev";
const ORIGIN = "http://x";
const NOW_ISO = "2026-07-01T00:00:00Z";

// One long-lived superuser client on the scratch DB for seeding + out-of-band probes.
let admin: Client;

async function rootExec(sql: string): Promise<void> {
  const c = new Client({ connectionString: ROOT_URL });
  await c.connect();
  try {
    await c.query(sql);
  } finally {
    await c.end();
  }
}

// ── request driving ──────────────────────────────────────────────────────────────────────────────

type RouteHandler = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<Response>;

function req(
  method: string,
  path: string,
  opts: { cred?: string; body?: unknown; rawBody?: string } = {},
): Request {
  const headers: Record<string, string> = {};
  if (opts.cred !== undefined) {
    headers.authorization = `Bearer ${opts.cred}`;
  }
  const init: RequestInit = { method, headers };
  if (opts.rawBody !== undefined) {
    headers["content-type"] = "application/json";
    init.body = opts.rawBody;
  } else if (opts.body !== undefined) {
    headers["content-type"] = "application/json";
    init.body = JSON.stringify(opts.body);
  }
  return new Request(`${ORIGIN}${path}`, init);
}

async function drive(
  h: RouteHandler,
  request: Request,
  params: Record<string, string | undefined>,
): Promise<Response> {
  try {
    return await h({ request, params });
  } catch (e) {
    if (e instanceof Response) {
      return e;
    }
    throw e;
  }
}

// ── expected wire bodies ─────────────────────────────────────────────────────────────────────────

const NOT_FOUND_BODY = {
  schema_version: 1,
  command: "error",
  ok: false,
  data: {},
  warnings: [],
  next_actions: [],
  error: {
    code: "NOT_FOUND",
    outcome: "PERMANENT_FAILURE",
    retryable: false,
    affected: {},
    context: { message: "not found" },
    next_actions: [],
  },
};

function badRequestBody(message: string) {
  return {
    schema_version: 1,
    command: "error",
    ok: false,
    data: {},
    warnings: [],
    next_actions: [],
    error: {
      code: "BAD_REQUEST",
      outcome: "PERMANENT_FAILURE",
      retryable: false,
      affected: {},
      context: { message },
      next_actions: [],
    },
  };
}

function okStatusBody(command: string, status: string) {
  return {
    schema_version: 1,
    command,
    ok: true,
    data: { status },
    warnings: [],
    next_actions: [],
  };
}

const DENIED_ACTIONS = [
  { code: "REQUEST_ACCESS", argv: [] },
  { code: "CONTACT_ADMIN", argv: [] },
];

function deniedBody(command: string, code: string) {
  return {
    schema_version: 1,
    command,
    ok: false,
    data: {},
    warnings: [],
    next_actions: DENIED_ACTIONS,
    error: {
      code,
      outcome: "DENIED",
      retryable: false,
      affected: {},
      context: {},
      next_actions: DENIED_ACTIONS,
    },
  };
}

async function deliverySkillIds(person: string, device: string): Promise<string[]> {
  const { rows } = await admin.query<{ body: { skills?: { skill_id: string }[] } }>(
    "SELECT topos_delivery($1, $2, $3) AS body",
    [WS, person, device],
  );
  return (rows[0]?.body.skills ?? []).map((s) => s.skill_id).sort();
}

// ── fixture ──────────────────────────────────────────────────────────────────────────────────────

const PUBKEY = Buffer.alloc(32, 7);

async function seed(): Promise<void> {
  const q = (sql: string, params: unknown[] = []) => admin.query(sql, params);
  await q(
    `INSERT INTO plane.workspace (workspace_id, display_name, verified_domain_status, deployment_mode, created_at, name)
     VALUES ($1, 'Acme', 'unverified', 'cloud', $2, 'acme')`,
    [WS, NOW_ISO],
  );
  await q(
    `INSERT INTO plane.workspace_policy (workspace_id, review_required, invite_policy, staleness_window_ms)
     VALUES ($1, 0, 'members', 604800000)`,
    [WS],
  );
  // Seats: an owner (no inviter), a reviewer, a member (both invited by the owner).
  const seat = (principal: string, role: string, invitedBy: string | null) =>
    q(
      `INSERT INTO plane.workspace_member (workspace_id, principal, role, status, invited_by, added_at)
       VALUES ($1, $2, $3, 'confirmed', $4, $5)`,
      [WS, principal, role, invitedBy, NOW_ISO],
    );
  await seat("owner@acme.com", "owner", null);
  await seat("rev@acme.com", "reviewer", "owner@acme.com");
  await seat("mem@acme.com", "member", "owner@acme.com");
  // Devices — the credential's sha256 is computed IN SQL (this tier holds no hashing code).
  const device = (dk: string, principal: string, cred: string, revoked = 0) =>
    q(
      `INSERT INTO plane.device_registry (workspace_id, device_key_id, public_key, principal, revoked, credential_sha256)
       VALUES ($1, $2, $3, $4, $5, sha256(convert_to($6, 'UTF8')))`,
      [WS, dk, PUBKEY, principal, revoked, cred],
    );
  await device("dev-owner", "owner@acme.com", "cred-owner");
  await device("dev-rev", "rev@acme.com", "cred-rev");
  await device("dev-mem", "mem@acme.com", "cred-mem");
  await device("dev-revoked", "mem@acme.com", "cred-revoked", 1);
  // A device whose principal holds NO seat — the non-member 404 probe.
  await device("dev-stranger", "stranger@acme.com", "cred-stranger");
  // Catalog: two active skills (each with a current pointer) + one archived (freed base name).
  const catalog = (id: string, name: string, status: string, baseName: string | null) =>
    q(
      `INSERT INTO plane.catalog (workspace_id, skill_id, name, display_name, status, base_name, created_at)
       VALUES ($1, $2, $3, NULL, $4, $5, $6)`,
      [WS, id, name, status, baseName, NOW_ISO],
    );
  const current = async (id: string, hex: string) => {
    const commit = Buffer.from(hex, "hex");
    await q(
      "INSERT INTO plane.skill_commit (workspace_id, commit_id, skill_id, bundle_digest) VALUES ($1, $2, $3, NULL)",
      [WS, commit, id],
    );
    await q(
      "INSERT INTO plane.current (workspace_id, skill_id, commit_id, epoch, seq, updated_at) VALUES ($1, $2, $3, 1, 1, 1700000000000)",
      [WS, id, commit],
    );
  };
  await catalog("s_a", "alpha", "active", null);
  await current("s_a", "a1".repeat(32));
  await catalog("s_b", "beta", "active", null);
  await current("s_b", "b1".repeat(32));
  await catalog("s_arch", "oldname-arch", "archived", "oldname");
  // Channels: the structural everyone, an open channel `eng` (member seated, `alpha` placed), an
  // empty open `ops`, and a curated `locked`.
  const channel = (id: string, name: string, mode: string, builtin: number) =>
    q(
      `INSERT INTO plane.channels (workspace_id, channel_id, name, mode, builtin, created_by, created_at)
       VALUES ($1, $2, $3, $4, $5, 'owner@acme.com', $6)`,
      [WS, id, name, mode, builtin, NOW_ISO],
    );
  await channel("everyone", "everyone", "open", 1);
  await channel("eng", "eng", "open", 0);
  await channel("ops", "ops", "open", 0);
  await channel("locked", "locked", "curated", 0);
  await q(
    `INSERT INTO plane.channel_skills (workspace_id, channel_id, skill_id, added_by, added_at)
     VALUES ($1, 'eng', 's_a', 'owner@acme.com', $2)`,
    [WS, NOW_ISO],
  );
  await q(
    `INSERT INTO plane.channel_members (workspace_id, channel_id, principal, added_by, added_at)
     VALUES ($1, 'eng', 'mem@acme.com', 'owner@acme.com', $2)`,
    [WS, NOW_ISO],
  );
  // Owner directly follows alpha (so its reach counts two people, two devices).
  await q(
    `INSERT INTO plane.skill_follows (workspace_id, principal, skill_id, created_at)
     VALUES ($1, 'owner@acme.com', 's_a', $2)`,
    [WS, NOW_ISO],
  );
  // A notice for the member, unacked.
  await q(
    `INSERT INTO plane.notices (workspace_id, id, principal, kind, skill_id, actor, outcome, created_at, acked_at)
     VALUES ($1, 'ntc-1', 'mem@acme.com', 'verdict', 's_a', 'rev@acme.com', 'approve', $2, NULL)`,
    [WS, NOW_ISO],
  );
}

beforeAll(async () => {
  await rootExec(`CREATE DATABASE ${scratchName}`);
  // Roles are cluster-global — create if absent, then ENFORCE the password every run. A bare
  // `create-if-absent` would leave a stale password in place when the role already exists from
  // another suite (or an earlier run) with a different one, and the app pool then can't
  // authenticate. A transaction-scoped advisory lock serializes the mutation so two suites
  // ALTERing the same `pg_authid` tuple at once don't raise "tuple concurrently updated".
  await rootExec(`DO $$ BEGIN
    PERFORM pg_advisory_xact_lock(hashtext('topos_role_setup'));
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'topos_plane')
      THEN ALTER ROLE topos_plane LOGIN PASSWORD 'plane';
      ELSE CREATE ROLE topos_plane LOGIN PASSWORD 'plane'; END IF;
    IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'topos_web')
      THEN ALTER ROLE topos_web LOGIN PASSWORD 'web';
      ELSE CREATE ROLE topos_web LOGIN PASSWORD 'web'; END IF;
  END $$;`);
  await rootExec(`ALTER DATABASE ${scratchName} OWNER TO topos_plane`);
  await rootExec(`ALTER DATABASE ${scratchName} SET search_path TO plane, public`);
  // Apply the migrations AS topos_plane so 0019's grant block fires (current_user = topos_plane).
  await applyPlaneDdl(planeUrl());

  admin = new Client({ connectionString: superScratchUrl() });
  await admin.connect();
  await seed();

  // The app pool connects AS topos_web (the scoped role) — the belt off for determinism.
  installTestEnv({ DATABASE_URL: webUrl(), TOPOS_WEB_RATELIMIT: "off" });
});

afterAll(async () => {
  await admin.end();
  const { getPool } = await import("@/lib/db/index.server");
  await getPool().end();
  await rootExec(`DROP DATABASE IF EXISTS ${scratchName} WITH (FORCE)`);
});

// ── (a) describe reads: success bodies EQUAL the wire shapes ────────────────────────────────────────

describe("describe reads", () => {
  it("GET /me — owner (no invited_by; the genesis seat has none)", async () => {
    const res = await drive(
      meLoader as unknown as RouteHandler,
      req("GET", `/api/v1/workspaces/${WS}/me`, { cred: "cred-owner" }),
      { ws: WS },
    );
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    expect(await res.json()).toEqual({
      workspace_id: WS,
      name: "acme",
      display_name: "Acme",
      address: "http://x/acme",
      principal: "owner@acme.com",
      role: "owner",
      invite_policy: "members",
    });
  });

  it("GET /me — member (invited_by present)", async () => {
    const res = await drive(
      meLoader as unknown as RouteHandler,
      req("GET", `/api/v1/workspaces/${WS}/me`, { cred: "cred-mem" }),
      { ws: WS },
    );
    expect(await res.json()).toEqual({
      workspace_id: WS,
      name: "acme",
      display_name: "Acme",
      address: "http://x/acme",
      principal: "mem@acme.com",
      role: "member",
      invited_by: "owner@acme.com",
      invite_policy: "members",
    });
  });

  it("GET /channels — everyone + eng, name-sorted, membership + counts", async () => {
    const res = await drive(
      channelIndexLoader as unknown as RouteHandler,
      req("GET", `/api/v1/workspaces/${WS}/channels`, { cred: "cred-mem" }),
      { ws: WS },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({
      channels: [
        {
          name: "eng",
          mode: "open",
          builtin: false,
          member: true,
          member_count: 1,
          skills: [{ skill_id: "s_a", name: "alpha" }],
        },
        {
          name: "everyone",
          mode: "open",
          builtin: true,
          member: true,
          member_count: 3,
          skills: [],
        },
        {
          name: "locked",
          mode: "curated",
          builtin: false,
          member: false,
          member_count: 0,
          skills: [],
        },
        { name: "ops", mode: "open", builtin: false, member: false, member_count: 0, skills: [] },
      ],
    });
  });

  it("GET /skills/{id}/reach — alpha reaches two people, two devices", async () => {
    const res = await drive(
      reachLoader as unknown as RouteHandler,
      req("GET", `/api/v1/workspaces/${WS}/skills/s_a/reach`, { cred: "cred-mem" }),
      { ws: WS, skill: "s_a" },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ persons: 2, devices: 2 });
  });

  it("GET /skills/{id}/reach — an unknown skill id is the uniform 404", async () => {
    const res = await drive(
      reachLoader as unknown as RouteHandler,
      req("GET", `/api/v1/workspaces/${WS}/skills/s_nope/reach`, { cred: "cred-mem" }),
      { ws: WS, skill: "s_nope" },
    );
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual(NOT_FOUND_BODY);
  });
});

// ── (b) the uniform 404 on EVERY route ──────────────────────────────────────────────────────────────

interface RouteCase {
  name: string;
  h: RouteHandler;
  method: string;
  params: Record<string, string>;
  path: string;
  body?: unknown;
}

const ALL_ROUTES: RouteCase[] = [
  {
    name: "me",
    h: meLoader as unknown as RouteHandler,
    method: "GET",
    params: { ws: WS },
    path: `/api/v1/workspaces/${WS}/me`,
  },
  {
    name: "channels",
    h: channelIndexLoader as unknown as RouteHandler,
    method: "GET",
    params: { ws: WS },
    path: `/api/v1/workspaces/${WS}/channels`,
  },
  {
    name: "reach",
    h: reachLoader as unknown as RouteHandler,
    method: "GET",
    params: { ws: WS, skill: "s_a" },
    path: `/api/v1/workspaces/${WS}/skills/s_a/reach`,
  },
  {
    name: "notices",
    h: noticesAction as unknown as RouteHandler,
    method: "POST",
    params: { ws: WS },
    path: `/api/v1/workspaces/${WS}/notices/ack`,
    body: { ids: ["ntc-1"] },
  },
  {
    name: "invitations",
    h: invitationsAction as unknown as RouteHandler,
    method: "POST",
    params: { ws: WS },
    path: `/api/v1/workspaces/${WS}/invitations`,
    body: { emails: ["x@y.z"] },
  },
  {
    name: "follow",
    h: followsAction as unknown as RouteHandler,
    method: "PUT",
    params: { ws: WS, skill: "s_a" },
    path: `/api/v1/workspaces/${WS}/follows/s_a`,
  },
  {
    name: "unfollow",
    h: followsAction as unknown as RouteHandler,
    method: "DELETE",
    params: { ws: WS, skill: "s_a" },
    path: `/api/v1/workspaces/${WS}/follows/s_a`,
  },
  {
    name: "exclude",
    h: exclusionsAction as unknown as RouteHandler,
    method: "PUT",
    params: { ws: WS, skill: "s_a" },
    path: `/api/v1/workspaces/${WS}/exclusions/s_a`,
  },
  {
    name: "join",
    h: channelsLoader as unknown as RouteHandler,
    method: "PUT",
    params: { ws: WS, channel: "ops" },
    path: `/api/v1/workspaces/${WS}/channels/ops/membership`,
  },
  {
    name: "leave",
    h: channelsLoader as unknown as RouteHandler,
    method: "DELETE",
    params: { ws: WS, channel: "ops" },
    path: `/api/v1/workspaces/${WS}/channels/ops/membership`,
  },
  {
    name: "place",
    h: curationAction as unknown as RouteHandler,
    method: "PUT",
    params: { ws: WS, channel: "eng", skill: "s_a" },
    path: `/api/v1/workspaces/${WS}/channels/eng/skills/s_a`,
  },
  {
    name: "unplace",
    h: curationAction as unknown as RouteHandler,
    method: "DELETE",
    params: { ws: WS, channel: "eng", skill: "s_a" },
    path: `/api/v1/workspaces/${WS}/channels/eng/skills/s_a`,
  },
  {
    name: "skill-protect",
    h: skillProtAction as unknown as RouteHandler,
    method: "PUT",
    params: { ws: WS, skill: "s_a" },
    path: `/api/v1/workspaces/${WS}/skills/s_a/protection`,
    body: { level: "reviewed" },
  },
  {
    name: "channel-protect",
    h: channelProtAction as unknown as RouteHandler,
    method: "PUT",
    params: { ws: WS, channel: "eng" },
    path: `/api/v1/workspaces/${WS}/channels/eng/protection`,
    body: { level: "curated" },
  },
];

describe("the uniform 404 (indistinguishable from a missing credential)", () => {
  it.each(ALL_ROUTES)("$name — an unknown credential is the uniform 404", async (rc) => {
    const res = await drive(
      rc.h,
      req(rc.method, rc.path, { cred: "cred-nope", body: rc.body }),
      rc.params,
    );
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual(NOT_FOUND_BODY);
  });

  it("no Authorization header → 404 (a GET read)", async () => {
    const res = await drive(
      meLoader as unknown as RouteHandler,
      req("GET", `/api/v1/workspaces/${WS}/me`),
      { ws: WS },
    );
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual(NOT_FOUND_BODY);
  });

  it("a REVOKED device's credential → 404 (a write)", async () => {
    const res = await drive(
      followsAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/follows/s_a`, { cred: "cred-revoked" }),
      { ws: WS, skill: "s_a" },
    );
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual(NOT_FOUND_BODY);
  });

  it("a NON-member's credential → 404 (a read)", async () => {
    const res = await drive(
      meLoader as unknown as RouteHandler,
      req("GET", `/api/v1/workspaces/${WS}/me`, { cred: "cred-stranger" }),
      { ws: WS },
    );
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual(NOT_FOUND_BODY);
  });

  it("an unsupported method on a served path → the uniform 404 (no method oracle)", async () => {
    const res = await drive(
      followsAction as unknown as RouteHandler,
      req("POST", `/api/v1/workspaces/${WS}/follows/s_a`, { cred: "cred-mem" }),
      { ws: WS, skill: "s_a" },
    );
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual(NOT_FOUND_BODY);
  });
});

// ── (c) the 200-DENIED codes for the role-gated refusals ────────────────────────────────────────────

describe("200 DENIED (a member's refusal names WHY, never a 403)", () => {
  it("a member tightening a skill to `reviewed` → REVIEWER_ROLE_REQUIRED", async () => {
    const res = await drive(
      skillProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/skills/s_a/protection`, {
        cred: "cred-mem",
        body: { level: "reviewed" },
      }),
      { ws: WS, skill: "s_a" },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual(deniedBody("protect", "REVIEWER_ROLE_REQUIRED"));
  });

  it("a member loosening a skill to `open` → OWNER_ROLE_REQUIRED", async () => {
    const res = await drive(
      skillProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/skills/s_a/protection`, {
        cred: "cred-mem",
        body: { level: "open" },
      }),
      { ws: WS, skill: "s_a" },
    );
    expect(await res.json()).toEqual(deniedBody("protect", "OWNER_ROLE_REQUIRED"));
  });

  it("joining the builtin `everyone` → CHANNEL_BUILTIN", async () => {
    const res = await drive(
      channelsLoader as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/channels/everyone/membership`, { cred: "cred-mem" }),
      { ws: WS, channel: "everyone" },
    );
    expect(await res.json()).toEqual(deniedBody("channel", "CHANNEL_BUILTIN"));
  });

  it("following an ARCHIVED skill → SKILL_NOT_ACTIVE", async () => {
    const res = await drive(
      followsAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/follows/s_arch`, { cred: "cred-mem" }),
      { ws: WS, skill: "s_arch" },
    );
    expect(await res.json()).toEqual(deniedBody("follow", "SKILL_NOT_ACTIVE"));
  });

  it("a member curating into a CURATED channel → CURATED_ROLE_REQUIRED", async () => {
    const res = await drive(
      curationAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/channels/locked/skills/s_b`, { cred: "cred-mem" }),
      { ws: WS, channel: "locked", skill: "s_b" },
    );
    expect(await res.json()).toEqual(deniedBody("channel", "CURATED_ROLE_REQUIRED"));
  });

  it("a new channel name that violates the charset → BAD_NAME", async () => {
    const res = await drive(
      curationAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/channels/Bad_Name/skills/s_b`, { cred: "cred-mem" }),
      { ws: WS, channel: "Bad_Name", skill: "s_b" },
    );
    expect(await res.json()).toEqual(deniedBody("channel", "BAD_NAME"));
  });

  it("inviting into an unknown channel → UNKNOWN_CHANNEL (nothing written)", async () => {
    const res = await drive(
      invitationsAction as unknown as RouteHandler,
      req("POST", `/api/v1/workspaces/${WS}/invitations`, {
        cred: "cred-owner",
        body: { emails: ["z@acme.com"], channels: ["nope"] },
      }),
      { ws: WS },
    );
    expect(await res.json()).toEqual(deniedBody("invite", "UNKNOWN_CHANNEL"));
    const { rows } = await admin.query(
      "SELECT 1 FROM plane.workspace_member WHERE workspace_id = $1 AND principal = 'z@acme.com'",
      [WS],
    );
    expect(rows).toHaveLength(0);
  });
});

// ── (d) validation 400s (and the ordering: body-first vs level-after-auth) ─────────────────────────

describe("validation 400s", () => {
  it("notices/ack — a non-JSON body is a 400 (before auth)", async () => {
    const res = await drive(
      noticesAction as unknown as RouteHandler,
      req("POST", `/api/v1/workspaces/${WS}/notices/ack`, {
        cred: "cred-nope",
        rawBody: "{ not json",
      }),
      { ws: WS },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed JSON body"));
  });

  it("notices/ack — a body missing `ids` is a 400", async () => {
    const res = await drive(
      noticesAction as unknown as RouteHandler,
      req("POST", `/api/v1/workspaces/${WS}/notices/ack`, { cred: "cred-mem", body: {} }),
      { ws: WS },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed notices ack body"));
  });

  it("protection — a wrong level is a 400 with a VALID credential (pinned message)", async () => {
    const res = await drive(
      skillProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/skills/s_a/protection`, {
        cred: "cred-owner",
        body: { level: "bogus" },
      }),
      { ws: WS, skill: "s_a" },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(
      badRequestBody("a skill protection level must be `reviewed` or `open`"),
    );
  });

  it("protection — a wrong level is a 400 EVEN with a bad credential (level check precedes auth, matching the vault; a bad level is never a membership signal)", async () => {
    const res = await drive(
      skillProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/skills/s_a/protection`, {
        cred: "cred-nope",
        body: { level: "bogus" },
      }),
      { ws: WS, skill: "s_a" },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(
      badRequestBody("a skill protection level must be `reviewed` or `open`"),
    );
  });

  it("protection — a MALFORMED body with a bad credential is still 400 (body precedes auth)", async () => {
    const res = await drive(
      skillProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/skills/s_a/protection`, {
        cred: "cred-nope",
        rawBody: "{ not json",
      }),
      { ws: WS, skill: "s_a" },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed JSON body"));
  });

  it("channel protection — a wrong level is a 400 with the channel-specific message", async () => {
    const res = await drive(
      channelProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/channels/ops/protection`, {
        cred: "cred-owner",
        body: { level: "reviewed" },
      }),
      { ws: WS, channel: "ops" },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(
      badRequestBody("a channel protection level must be `curated` or `open`"),
    );
  });

  it("invitations — a malformed invitee email is a 400 (after auth)", async () => {
    const res = await drive(
      invitationsAction as unknown as RouteHandler,
      req("POST", `/api/v1/workspaces/${WS}/invitations`, {
        cred: "cred-owner",
        body: { emails: ["not a valid email"] },
      }),
      { ws: WS },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed invitee email"));
  });

  it("invitations — a body missing `emails` is a 400", async () => {
    const res = await drive(
      invitationsAction as unknown as RouteHandler,
      req("POST", `/api/v1/workspaces/${WS}/invitations`, {
        cred: "cred-owner",
        body: { channels: [] },
      }),
      { ws: WS },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed invitation body: emails"));
  });
});

// ── (e) follow / exclude / unfollow actually mutate (delivery probe before/after) ──────────────────

describe("subscription writes mutate the delivered set", () => {
  it("follow → exclude → unfollow move `beta` in and out of the member's delivery", async () => {
    // Baseline: the member gets alpha via the `eng` channel, not beta.
    expect(await deliverySkillIds("mem@acme.com", "dev-mem")).toEqual(["s_a"]);

    const followed = await drive(
      followsAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/follows/s_b`, { cred: "cred-mem" }),
      { ws: WS, skill: "s_b" },
    );
    expect(await followed.json()).toEqual(okStatusBody("follow", "followed"));
    expect(await deliverySkillIds("mem@acme.com", "dev-mem")).toEqual(["s_a", "s_b"]);

    const excluded = await drive(
      exclusionsAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/exclusions/s_b`, { cred: "cred-mem" }),
      { ws: WS, skill: "s_b" },
    );
    expect(await excluded.json()).toEqual(okStatusBody("remove", "excluded"));
    expect(await deliverySkillIds("mem@acme.com", "dev-mem")).toEqual(["s_a"]);

    const unfollowed = await drive(
      followsAction as unknown as RouteHandler,
      req("DELETE", `/api/v1/workspaces/${WS}/follows/s_b`, { cred: "cred-mem" }),
      { ws: WS, skill: "s_b" },
    );
    expect(await unfollowed.json()).toEqual(okStatusBody("unfollow", "unfollowed"));
    expect(await deliverySkillIds("mem@acme.com", "dev-mem")).toEqual(["s_a"]);
  });
});

// ── (f) notices ack flips acked_at ──────────────────────────────────────────────────────────────────

describe("notices ack", () => {
  it("acks the caller's own notice (flips acked_at) and answers { status: acked }", async () => {
    const before = await admin.query(
      "SELECT acked_at FROM plane.notices WHERE workspace_id = $1 AND id = 'ntc-1'",
      [WS],
    );
    expect(before.rows[0]?.acked_at).toBeNull();

    const res = await drive(
      noticesAction as unknown as RouteHandler,
      req("POST", `/api/v1/workspaces/${WS}/notices/ack`, {
        cred: "cred-mem",
        body: { ids: ["ntc-1"] },
      }),
      { ws: WS },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual(okStatusBody("notices", "acked"));

    const after = await admin.query(
      "SELECT acked_at FROM plane.notices WHERE workspace_id = $1 AND id = 'ntc-1'",
      [WS],
    );
    expect(after.rows[0]?.acked_at).not.toBeNull();
  });
});

// ── OK writes: the full round-trip envelopes ────────────────────────────────────────────────────────

describe("OK write envelopes", () => {
  it("invitations — a success carries the InvitationData (address + folded invited + honest mailed)", async () => {
    const res = await drive(
      invitationsAction as unknown as RouteHandler,
      req("POST", `/api/v1/workspaces/${WS}/invitations`, {
        cred: "cred-owner",
        body: { emails: ["NEW@Acme.COM"] },
      }),
      { ws: WS },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({
      schema_version: 1,
      command: "invite",
      ok: true,
      data: { address: "http://x/acme", invited: ["new@acme.com"], mailed: false },
      warnings: [],
      next_actions: [],
    });
    // The folded seat landed (invited, member).
    const { rows } = await admin.query(
      "SELECT status, role FROM plane.workspace_member WHERE workspace_id = $1 AND principal = 'new@acme.com'",
      [WS],
    );
    expect(rows[0]).toEqual({ status: "invited", role: "member" });
  });

  it("channel membership — join, leave, then leave-again (not_member)", async () => {
    const joined = await drive(
      channelsLoader as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/channels/ops/membership`, { cred: "cred-mem" }),
      { ws: WS, channel: "ops" },
    );
    expect(await joined.json()).toEqual(okStatusBody("channel", "joined"));
    const left = await drive(
      channelsLoader as unknown as RouteHandler,
      req("DELETE", `/api/v1/workspaces/${WS}/channels/ops/membership`, { cred: "cred-mem" }),
      { ws: WS, channel: "ops" },
    );
    expect(await left.json()).toEqual(okStatusBody("channel", "left"));
    const again = await drive(
      channelsLoader as unknown as RouteHandler,
      req("DELETE", `/api/v1/workspaces/${WS}/channels/ops/membership`, { cred: "cred-mem" }),
      { ws: WS, channel: "ops" },
    );
    expect(await again.json()).toEqual(okStatusBody("channel", "not_member"));
  });

  it("curation — create-on-first-use, place, remove, then remove-again (not_placed)", async () => {
    const created = await drive(
      curationAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/channels/team/skills/s_b`, { cred: "cred-mem" }),
      { ws: WS, channel: "team", skill: "s_b" },
    );
    expect(await created.json()).toEqual(okStatusBody("channel", "created"));
    const placed = await drive(
      curationAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/channels/team/skills/s_a`, { cred: "cred-mem" }),
      { ws: WS, channel: "team", skill: "s_a" },
    );
    expect(await placed.json()).toEqual(okStatusBody("channel", "placed"));
    const removed = await drive(
      curationAction as unknown as RouteHandler,
      req("DELETE", `/api/v1/workspaces/${WS}/channels/team/skills/s_a`, { cred: "cred-mem" }),
      { ws: WS, channel: "team", skill: "s_a" },
    );
    expect(await removed.json()).toEqual(okStatusBody("channel", "removed"));
    const notPlaced = await drive(
      curationAction as unknown as RouteHandler,
      req("DELETE", `/api/v1/workspaces/${WS}/channels/team/skills/s_a`, { cred: "cred-mem" }),
      { ws: WS, channel: "team", skill: "s_a" },
    );
    expect(await notPlaced.json()).toEqual(okStatusBody("channel", "not_placed"));
  });

  it("protection — owner tightens a skill (set); reviewer may too", async () => {
    const owner = await drive(
      skillProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/skills/s_b/protection`, {
        cred: "cred-owner",
        body: { level: "reviewed" },
      }),
      { ws: WS, skill: "s_b" },
    );
    expect(await owner.json()).toEqual(okStatusBody("protect", "set"));
    const reviewer = await drive(
      skillProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/skills/s_b/protection`, {
        cred: "cred-rev",
        body: { level: "reviewed" },
      }),
      { ws: WS, skill: "s_b" },
    );
    expect(await reviewer.json()).toEqual(okStatusBody("protect", "set"));
  });

  it("channel protection — owner loosens a curated channel to open (set)", async () => {
    const res = await drive(
      channelProtAction as unknown as RouteHandler,
      req("PUT", `/api/v1/workspaces/${WS}/channels/locked/protection`, {
        cred: "cred-owner",
        body: { level: "open" },
      }),
      { ws: WS, channel: "locked" },
    );
    expect(await res.json()).toEqual(okStatusBody("protect", "set"));
  });

  // The wrong-method half of each served route: react-router routes a GET to an action-only route
  // (and a mutation to a loader-only route) into the OTHER export — which here answers the uniform
  // 404, so a wrong-method probe is indistinguishable from a miss (never RR's 400/405 route-echo).
  it("wrong method on an action route (GET /report) is the uniform 404", async () => {
    const res = reportWrongMethod();
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual(NOT_FOUND_BODY);
  });

  it("wrong method on a loader route (mutation on /me) is the uniform 404", async () => {
    const res = meWrongMethod();
    expect(res.status).toBe(404);
    expect(await res.json()).toEqual(NOT_FOUND_BODY);
  });
});
