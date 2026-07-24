import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { action as catchAllAction, loader as catchAllLoader } from "@/routes/api.v1.$";
import { action as channelProtAction } from "@/routes/api.v1.channel-protection";
import { loader as channelsLoader, action as channelsWrongMethod } from "@/routes/api.v1.channels";
import { action as curationAction } from "@/routes/api.v1.curation";
import { loader as deliveryLoader, action as deliveryWrongMethod } from "@/routes/api.v1.delivery";
import { action as invitationsAction } from "@/routes/api.v1.invitations";
import { action as deviceAuthorizeAction } from "@/routes/api.v1.login-authorize";
import { action as deviceTokenAction } from "@/routes/api.v1.login-token";
import { loader as meLoader, action as meWrongMethod } from "@/routes/api.v1.me";
import { action as noticesAction } from "@/routes/api.v1.notices-ack";
import {
  action as profileChannelAction,
  loader as profileChannelWrongMethod,
} from "@/routes/api.v1.profile-channel";
import {
  action as profileSkillAction,
  loader as profileSkillWrongMethod,
} from "@/routes/api.v1.profile-skill";
import { action as reportAction, loader as reportWrongMethod } from "@/routes/api.v1.report";
import { loader as skillCurrentLoader } from "@/routes/api.v1.skill-current";
import { action as skillProtAction } from "@/routes/api.v1.skill-protection";
import { loader as reachLoader } from "@/routes/api.v1.skill-reach";
import { loader as skillsIndexLoader } from "@/routes/api.v1.skills-index";
import {
  createScratchDb,
  placeBundle,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedChannel,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The SERVED device-lane routes end to end against a REAL scratch Postgres — no vault. The
 * identities are minted through the REAL ceremonies (setup claim → seats → the gh-style device
 * flow's approve → the promoted bearer credential), so `requireSessionActor` resolves exactly as
 * production does; the route loaders/actions are then invoked directly with constructed
 * Requests, proving the WIRE BYTES field-for-field. The belt is off (`TOPOS_WEB_RATELIMIT=off`
 * — the env default is "on"; api-belt.test.ts owns the belt itself).
 *
 * The publish family (publish/propose/reviews/reverts) and the byte-decorated reads
 * (skill-log/object/version, the proposals lists) call the VAULT over HTTP and are not
 * unit-testable without one — the e2e stack owns them. `skill-current`'s pointer read is ALSO
 * an HTTP custody read (reads.server.ts `custodyCurrent`), so this suite runs a minimal
 * in-process stub vault serving that ONE GET — enough to prove the route's WireCurrentRecord
 * shape and its ETag/304 conditional arm.
 */

const SETUP_CODE = "devlane-setup-code-0000";
const ORIGIN = "http://x";
const V_ALPHA = "a1".repeat(32);
const V_BETA = "b1".repeat(32);
const V_ARCH = "c1".repeat(32);

let db: ScratchDb;
let wsId = "";
let stub: Server;
/** The stub vault's current-pointer answers, keyed `${ws}/${bundle}`. */
const stubCurrent = new Map<string, { version_id: string; generation: number }>();

const CREDS = {
  owner: "",
  rev: "",
  mem: "",
  stranger: "",
  revoked: "",
};
let memDeviceId = "";
let noticeId = "";

// ── request driving ──────────────────────────────────────────────────────────────────────────

type RouteHandler = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<Response> | Response;

function req(
  method: string,
  path: string,
  opts: { cred?: string; body?: unknown; rawBody?: string; headers?: Record<string, string> } = {},
): Request {
  const headers: Record<string, string> = { ...opts.headers };
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
  h: unknown,
  request: Request,
  params: Record<string, string | undefined> = {},
): Promise<Response> {
  try {
    return await (h as RouteHandler)({ request, params });
  } catch (e) {
    if (e instanceof Response) {
      return e;
    }
    throw e;
  }
}

// ── expected wire bodies ─────────────────────────────────────────────────────────────────────

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
  { code: "REQUEST_ACCESS", argv: [], mutates: false, needs_network: false },
  { code: "CONTACT_ADMIN", argv: [], mutates: false, needs_network: false },
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

async function expectUniform404(res: Response): Promise<void> {
  expect(res.status).toBe(404);
  expect(await res.json()).toEqual(NOT_FOUND_BODY);
}

/** The member's delivered skill ids, through the REAL delivery route. */
async function deliveredSkillIds(cred: string): Promise<string[]> {
  const res = await drive(
    deliveryLoader,
    req("GET", `/api/v1/workspaces/${wsId}/delivery`, { cred }),
    {
      ws: wsId,
    },
  );
  expect(res.status).toBe(200);
  const body = (await res.json()) as { skills: { skill_id: string }[] };
  return body.skills.map((s) => s.skill_id).sort();
}

// ── fixture ──────────────────────────────────────────────────────────────────────────────────

/** The full login ceremony: start → approve (as `userId`) → poll; the device_code IS the credential. */
async function mintCredential(
  userId: string,
  display: string,
  requestedName: string,
): Promise<{ credential: string; sessionId: string }> {
  const identity = await import("@/lib/db/identity.server");
  const flow = await identity.startLoginFlow(requestedName, "team");
  await identity.approveLoginFlow(flow.userCode, { userId, display });
  const granted = await identity.pollLoginFlow(flow.flowCode);
  if (granted.status !== "granted") {
    throw new Error(`device mint failed: ${granted.status}`);
  }
  return { credential: flow.flowCode, sessionId: granted.sessionId };
}

beforeAll(async () => {
  // The stub vault: ONE GET — the current-pointer read skill-current forwards to.
  stub = createServer((request, response) => {
    const match = request.url?.match(
      /^\/internal\/v1\/workspaces\/([^/]+)\/bundles\/([^/]+)\/current$/,
    );
    const hit = match ? stubCurrent.get(`${match[1]}/${match[2]}`) : undefined;
    if (request.method === "GET" && hit !== undefined) {
      response.writeHead(200, { "content-type": "application/json" });
      response.end(
        JSON.stringify({
          ...hit,
          moved_at_ms: 1700000000000,
          moved_by_display: "seed",
          bundle_digest: "d".repeat(64),
        }),
      );
      return;
    }
    response.writeHead(404, { "content-type": "application/json" });
    response.end(JSON.stringify({ code: "NOT_FOUND" }));
  });
  await new Promise<void>((resolve) => stub.listen(0, "127.0.0.1", resolve));
  const port = (stub.address() as AddressInfo).port;

  db = await createScratchDb("web_devlane", {
    TOPOS_SETUP_CODE: SETUP_CODE,
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: `http://127.0.0.1:${port}`,
  });
  const identity = await import("@/lib/db/identity.server");
  await identity.ensureSetup(ORIGIN);
  wsId = (await identity.theWorkspace())?.id ?? "";

  // Seats through the REAL ceremonies where one exists: the claim mints the first owner.
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_rev", "Reviewer", "rev@example.com");
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seedUser(db, "u_stranger", "Stranger", "stranger@example.com");
  const claimed = await identity.consumeClaim(SETUP_CODE, "u_owner", "Owner");
  if (claimed === null) {
    throw new Error("claim seed failed");
  }
  await seatUser(db, wsId, "u_rev", "reviewer", "u_owner");
  await seatUser(db, wsId, "u_mem", "member", "u_owner");

  // Devices via the gh-style flow — REAL credentials, one revoked, one seatless. Approval
  // requires a seat, so the stranger's credential is minted while seated and the seat is then
  // removed — the real-world path to a live credential whose person lost admission.
  CREDS.owner = (await mintCredential("u_owner", "Owner", "owner-laptop")).credential;
  CREDS.rev = (await mintCredential("u_rev", "Reviewer", "rev-laptop")).credential;
  const mem = await mintCredential("u_mem", "Member", "mem-laptop");
  CREDS.mem = mem.credential;
  memDeviceId = mem.sessionId;
  await seatUser(db, wsId, "u_stranger", "member", "u_owner");
  CREDS.stranger = (await mintCredential("u_stranger", "Stranger", "stranger-box")).credential;
  await db.q(`DELETE FROM web.seat WHERE workspace_id = $1 AND user_id = 'u_stranger'`, [wsId]);
  const doomed = await mintCredential("u_mem", "Member", "mem-old-laptop");
  await identity.revokeOwnSession({ userId: "u_mem", display: "Member" }, doomed.sessionId);
  CREDS.revoked = doomed.credential;

  // Catalog: alpha (in `eng`), beta (nowhere yet), one archived, one pointer-less.
  await seedBundle(db, wsId, "s_alpha", "alpha", { versionId: V_ALPHA });
  await seedBundle(db, wsId, "s_beta", "beta", { versionId: V_BETA });
  await seedBundle(db, wsId, "s_arch", "oldname-archived-2026-07-01", {
    status: "archived",
    baseName: "oldname",
    versionId: V_ARCH,
  });
  await seedBundle(db, wsId, "s_gamma", "gamma", { withPointer: false });
  stubCurrent.set(`${wsId}/s_alpha`, { version_id: V_ALPHA, generation: 1 });

  // Channels: eng (member seated, alpha placed), empty ops, curated locked.
  await seedChannel(db, wsId, "c_eng", "eng");
  await seedChannel(db, wsId, "c_ops", "ops");
  await seedChannel(db, wsId, "c_locked", "locked", "curated");
  await placeBundle(db, wsId, "c_eng", "s_alpha");
  await db.q(
    `INSERT INTO web.profile_entry (channel_id, workspace_id, user_id, mode)
     VALUES ('c_eng', $1, 'u_mem', 'include')`,
    [wsId],
  );
  // The owner's profile includes alpha directly (so its reach counts two people).
  await db.q(
    `INSERT INTO web.profile_entry (user_id, workspace_id, bundle_id, mode)
     VALUES ('u_owner', $1, 's_alpha', 'include')`,
    [wsId],
  );
  // An unacked verdict notice for the member.
  const noticeRows = await db.q<{ id: string }>(
    `INSERT INTO web.notice (user_id, workspace_id, kind, payload)
     VALUES ('u_mem', $1, 'verdict', '{"skill_id":"s_alpha","actor":"Reviewer","outcome":"approve"}'::jsonb)
     RETURNING id`,
    [wsId],
  );
  noticeId = String(noticeRows[0]?.id);
}, 60000);

afterAll(async () => {
  await new Promise<void>((resolve, reject) => stub.close((e) => (e ? reject(e) : resolve())));
  await db.drop();
});

// ── (a) describe reads: success bodies EQUAL the wire shapes ─────────────────────────────────

describe("describe reads", () => {
  it("GET /me — owner (no invited_by; the genesis seat has none)", async () => {
    const res = await drive(
      meLoader,
      req("GET", `/api/v1/workspaces/${wsId}/me`, { cred: CREDS.owner }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    expect(await res.json()).toEqual({
      workspace_id: wsId,
      name: "team",
      display_name: "team",
      address: "http://x",
      principal: "Owner",
      role: "owner",
      session_status: "active",
    });
  });

  it("GET /me — member (invited_by carries the inviter's login address)", async () => {
    const res = await drive(
      meLoader,
      req("GET", `/api/v1/workspaces/${wsId}/me`, { cred: CREDS.mem }),
      { ws: wsId },
    );
    expect(await res.json()).toEqual({
      workspace_id: wsId,
      name: "team",
      display_name: "team",
      address: "http://x",
      principal: "Member",
      role: "member",
      session_status: "active",
      invited_by: "owner@example.com",
    });
  });

  it("GET /channels — name-sorted, the default included, membership + counts + placed skills", async () => {
    const res = await drive(
      channelsLoader,
      req("GET", `/api/v1/workspaces/${wsId}/channels`, { cred: CREDS.mem }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({
      channels: [
        {
          name: "eng",
          mode: "open",
          builtin: false,
          included: true,
                    skills: [{ skill_id: "s_alpha", name: "alpha" }],
        },
        {
          name: "everyone",
          mode: "open",
          builtin: true,
          included: true,
                    skills: [],
        },
        {
          name: "locked",
          mode: "curated",
          builtin: false,
          included: false,
                    skills: [],
        },
        { name: "ops", mode: "open", builtin: false, included: false, skills: [] },
      ],
    });
  });

  it("GET /skills/{skill}/reach — alpha reaches two people, two live devices", async () => {
    const res = await drive(
      reachLoader,
      req("GET", `/api/v1/workspaces/${wsId}/skills/s_alpha/reach`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual({ persons: 2, sessions: 2 });
  });

  it("GET /skills/{skill}/reach — an unknown skill id is the uniform 404", async () => {
    const res = await drive(
      reachLoader,
      req("GET", `/api/v1/workspaces/${wsId}/skills/s_nope/reach`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_nope" },
    );
    await expectUniform404(res);
  });
});

// ── (b) the uniform 404 on EVERY route ───────────────────────────────────────────────────────

interface RouteCase {
  name: string;
  h: unknown;
  method: string;
  params: Record<string, string>;
  path: string;
  body?: unknown;
}

const ALL_ROUTES: RouteCase[] = [
  { name: "me", h: meLoader, method: "GET", params: {}, path: "/me" },
  { name: "delivery", h: deliveryLoader, method: "GET", params: {}, path: "/delivery" },
  { name: "channels", h: channelsLoader, method: "GET", params: {}, path: "/channels" },
  { name: "skills-index", h: skillsIndexLoader, method: "GET", params: {}, path: "/skills" },
  {
    name: "skill-current",
    h: skillCurrentLoader,
    method: "GET",
    params: { skill: "s_alpha" },
    path: "/skills/s_alpha/current",
  },
  {
    name: "reach",
    h: reachLoader,
    method: "GET",
    params: { skill: "s_alpha" },
    path: "/skills/s_alpha/reach",
  },
  {
    name: "report",
    h: reportAction,
    method: "PUT",
    params: {},
    path: "/report",
    body: { schema_version: 1, applied: [] },
  },
  {
    name: "notices",
    h: noticesAction,
    method: "POST",
    params: {},
    path: "/notices/ack",
    body: { ids: ["1"] },
  },
  {
    name: "invitations",
    h: invitationsAction,
    method: "POST",
    params: {},
    path: "/invitations",
    body: { emails: ["x@y.z"] },
  },
  {
    name: "profile-include",
    h: profileSkillAction,
    method: "PUT",
    params: { skill: "s_alpha" },
    path: "/profile/skills/s_alpha",
  },
  {
    name: "profile-remove",
    h: profileSkillAction,
    method: "DELETE",
    params: { skill: "s_alpha" },
    path: "/profile/skills/s_alpha",
  },
  {
    name: "profile-channel-include",
    h: profileChannelAction,
    method: "PUT",
    params: { channel: "ops" },
    path: "/profile/channels/ops",
  },
  {
    name: "profile-channel-remove",
    h: profileChannelAction,
    method: "DELETE",
    params: { channel: "ops" },
    path: "/profile/channels/ops",
  },
  {
    name: "place",
    h: curationAction,
    method: "PUT",
    params: { channel: "eng", skill: "s_alpha" },
    path: "/channels/eng/skills/s_alpha",
  },
  {
    name: "unplace",
    h: curationAction,
    method: "DELETE",
    params: { channel: "eng", skill: "s_alpha" },
    path: "/channels/eng/skills/s_alpha",
  },
  {
    name: "skill-protect",
    h: skillProtAction,
    method: "PUT",
    params: { skill: "s_alpha" },
    path: "/skills/s_alpha/protection",
    body: { level: "reviewed" },
  },
  {
    name: "channel-protect",
    h: channelProtAction,
    method: "PUT",
    params: { channel: "eng" },
    path: "/channels/eng/protection",
    body: { level: "curated" },
  },
];

describe("the uniform 404 (indistinguishable from a missing credential)", () => {
  it.each(ALL_ROUTES)("$name — an unknown credential is the uniform 404", async (rc) => {
    const res = await drive(
      rc.h,
      req(rc.method, `/api/v1/workspaces/${wsId}${rc.path}`, { cred: "cred-nope", body: rc.body }),
      { ws: wsId, ...rc.params },
    );
    await expectUniform404(res);
  });

  it("no Authorization header → 404 (a GET read)", async () => {
    const res = await drive(meLoader, req("GET", `/api/v1/workspaces/${wsId}/me`), { ws: wsId });
    await expectUniform404(res);
  });

  it("an ENDED session's credential → 404 (a write)", async () => {
    const res = await drive(
      profileSkillAction,
      req("PUT", `/api/v1/workspaces/${wsId}/profile/skills/s_alpha`, { cred: CREDS.revoked }),
      { ws: wsId, skill: "s_alpha" },
    );
    await expectUniform404(res);
  });

  it("a SEATLESS user's credential → 404 (a read)", async () => {
    const res = await drive(
      meLoader,
      req("GET", `/api/v1/workspaces/${wsId}/me`, { cred: CREDS.stranger }),
      { ws: wsId },
    );
    await expectUniform404(res);
  });

  it("a valid credential against the WRONG workspace id → 404", async () => {
    const res = await drive(
      meLoader,
      req("GET", "/api/v1/workspaces/w_other/me", { cred: CREDS.mem }),
      { ws: "w_other" },
    );
    await expectUniform404(res);
  });

  it("an unsupported method on a served action → the uniform 404 (no method oracle)", async () => {
    const res = await drive(
      profileSkillAction,
      req("POST", `/api/v1/workspaces/${wsId}/profile/skills/s_alpha`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_alpha" },
    );
    await expectUniform404(res);
  });

  it("wrong-method exports answer the uniform 404, never react-router's 400/405", async () => {
    // React-router routes a mutation on a loader-only route (and a GET on an action-only one)
    // into the OTHER export — each pinned to the one envelope a miss speaks.
    for (const wrongMethod of [
      meWrongMethod,
      deliveryWrongMethod,
      channelsWrongMethod,
      reportWrongMethod,
      profileSkillWrongMethod,
      profileChannelWrongMethod,
    ]) {
      await expectUniform404(wrongMethod());
    }
  });

  it("the /api/v1 catch-all answers the uniform 404 on BOTH methods — no path echo", async () => {
    await expectUniform404(await drive(catchAllLoader, req("GET", "/api/v1/anything/else"), {}));
    await expectUniform404(
      await drive(catchAllAction, req("POST", "/api/v1/anything/else", { body: {} }), {}),
    );
  });

  it("a profile include on an unknown skill folds to the uniform 404 (never an existence oracle)", async () => {
    const res = await drive(
      profileSkillAction,
      req("PUT", `/api/v1/workspaces/${wsId}/profile/skills/s_nope`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_nope" },
    );
    await expectUniform404(res);
  });
});

// ── (c) validation 400s (and the ordering: body-first vs level-before-auth) ─────────────────

describe("validation 400s", () => {
  it("notices/ack — a non-JSON body is a 400 (before auth)", async () => {
    const res = await drive(
      noticesAction,
      req("POST", `/api/v1/workspaces/${wsId}/notices/ack`, {
        cred: "cred-nope",
        rawBody: "{ not json",
      }),
      { ws: wsId },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed JSON body"));
  });

  it("notices/ack — a body missing `ids` is a 400", async () => {
    const res = await drive(
      noticesAction,
      req("POST", `/api/v1/workspaces/${wsId}/notices/ack`, { cred: CREDS.mem, body: {} }),
      { ws: wsId },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed notices ack body"));
  });

  it("skill protection — a wrong level is a 400 with a VALID credential (pinned message)", async () => {
    const res = await drive(
      skillProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/skills/s_alpha/protection`, {
        cred: CREDS.owner,
        body: { level: "bogus" },
      }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(
      badRequestBody("a skill protection level must be `reviewed` or `open`"),
    );
  });

  it("skill protection — a wrong level is a 400 EVEN with a bad credential (level check precedes auth; a bad level is never a membership signal)", async () => {
    const res = await drive(
      skillProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/skills/s_alpha/protection`, {
        cred: "cred-nope",
        body: { level: "bogus" },
      }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(
      badRequestBody("a skill protection level must be `reviewed` or `open`"),
    );
  });

  it("skill protection — a MALFORMED body with a bad credential is still 400 (body precedes auth)", async () => {
    const res = await drive(
      skillProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/skills/s_alpha/protection`, {
        cred: "cred-nope",
        rawBody: "{ not json",
      }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed JSON body"));
  });

  it("channel protection — a wrong level is a 400 with the channel-specific message", async () => {
    const res = await drive(
      channelProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/ops/protection`, {
        cred: CREDS.owner,
        body: { level: "reviewed" },
      }),
      { ws: wsId, channel: "ops" },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(
      badRequestBody("a channel protection level must be `curated` or `open`"),
    );
  });

  it("invitations — a body missing `emails` is a 400 (before auth)", async () => {
    const res = await drive(
      invitationsAction,
      req("POST", `/api/v1/workspaces/${wsId}/invitations`, {
        cred: "cred-nope",
        body: { channels: [] },
      }),
      { ws: wsId },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed invitation body: emails"));
  });

  it("report — a malformed entry is a 400 naming the field (before auth)", async () => {
    const res = await drive(
      reportAction,
      req("PUT", `/api/v1/workspaces/${wsId}/report`, {
        cred: "cred-nope",
        body: { schema_version: 1, applied: [{ skill_id: "s_alpha", version_id: "short" }] },
      }),
      { ws: wsId },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(
      badRequestBody("malformed report entry: version_id must be 64-char lowercase hex"),
    );
  });

  it("report — a missing schema_version is a 400", async () => {
    const res = await drive(
      reportAction,
      req("PUT", `/api/v1/workspaces/${wsId}/report`, { cred: CREDS.mem, body: { applied: [] } }),
      { ws: wsId },
    );
    expect(res.status).toBe(400);
    expect(await res.json()).toEqual(badRequestBody("malformed report body: schema_version"));
  });

  it("device flows — malformed bodies are 400s naming the op", async () => {
    const authorize = await drive(
      deviceAuthorizeAction,
      req("POST", "/api/v1/login/authorize", { body: { requested_name: "" } }),
      {},
    );
    expect(authorize.status).toBe(400);
    expect(await authorize.json()).toEqual(badRequestBody("malformed login authorize body"));
    const token = await drive(
      deviceTokenAction,
      req("POST", "/api/v1/login/token", { body: {} }),
      {},
    );
    expect(token.status).toBe(400);
    expect(await token.json()).toEqual(badRequestBody("malformed login token body"));
  });
});

// ── (d) the 200-DENIED codes for the typed refusals ──────────────────────────────────────────

describe("200 DENIED (a member's refusal names WHY, never a 403)", () => {
  it("a member tightening a skill to `reviewed` → REVIEWER_ROLE_REQUIRED", async () => {
    const res = await drive(
      skillProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/skills/s_alpha/protection`, {
        cred: CREDS.mem,
        body: { level: "reviewed" },
      }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual(deniedBody("protect", "REVIEWER_ROLE_REQUIRED"));
  });

  it("a REVIEWER loosening a skill to `open` → OWNER_ROLE_REQUIRED (loosening is owner-grade)", async () => {
    const res = await drive(
      skillProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/skills/s_alpha/protection`, {
        cred: CREDS.rev,
        body: { level: "open" },
      }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(await res.json()).toEqual(deniedBody("protect", "OWNER_ROLE_REQUIRED"));
  });

  it("a member tightening a channel to `curated` → REVIEWER_ROLE_REQUIRED", async () => {
    const res = await drive(
      channelProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/ops/protection`, {
        cred: CREDS.mem,
        body: { level: "curated" },
      }),
      { ws: wsId, channel: "ops" },
    );
    expect(await res.json()).toEqual(deniedBody("protect", "REVIEWER_ROLE_REQUIRED"));
  });

  it("including an ARCHIVED skill → SKILL_NOT_ACTIVE", async () => {
    const res = await drive(
      profileSkillAction,
      req("PUT", `/api/v1/workspaces/${wsId}/profile/skills/s_arch`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_arch" },
    );
    expect(await res.json()).toEqual(deniedBody("add", "SKILL_NOT_ACTIVE"));
  });

  it("a member curating into a CURATED channel → CURATED_ROLE_REQUIRED", async () => {
    const res = await drive(
      curationAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/locked/skills/s_beta`, { cred: CREDS.mem }),
      { ws: wsId, channel: "locked", skill: "s_beta" },
    );
    expect(await res.json()).toEqual(deniedBody("channel", "CURATED_ROLE_REQUIRED"));
  });

  it("a new channel name that violates the charset → BAD_NAME", async () => {
    const res = await drive(
      curationAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/Bad_Name/skills/s_beta`, { cred: CREDS.mem }),
      { ws: wsId, channel: "Bad_Name", skill: "s_beta" },
    );
    expect(await res.json()).toEqual(deniedBody("channel", "BAD_NAME"));
  });

  it("inviting on the unarmed default deployment → MAIL_NOT_CONFIGURED, nothing written", async () => {
    // The test env carries no TOPOS_MAIL_SMTP_* — exactly a fresh self-host: the mailbox
    // round-trip IS the invited sign-up's identity rung, so the op refuses typed.
    const res = await drive(
      invitationsAction,
      req("POST", `/api/v1/workspaces/${wsId}/invitations`, {
        cred: CREDS.owner,
        body: { emails: ["new@acme.com"] },
      }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual(deniedBody("invite", "MAIL_NOT_CONFIGURED"));
    expect(await db.q(`SELECT 1 FROM web.invitation WHERE workspace_id = $1`, [wsId])).toHaveLength(
      0,
    );
  });
});

// ── (e) delivery + the subscription writes that move it ─────────────────────────────────────

describe("delivery", () => {
  it("GET /delivery — the full body shape, no-store, one snapshot", async () => {
    const res = await drive(
      deliveryLoader,
      req("GET", `/api/v1/workspaces/${wsId}/delivery`, { cred: CREDS.mem }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as Record<string, unknown>;
    expect(body.schema_version).toBe(1);
    expect(body.workspace_id).toBe(wsId);
    expect(body.session_status).toBe("active");
    expect(body.staleness_window_ms).toBe(604800000);
    expect(body.proposals_awaiting).toBe(0);
    const skills = body.skills as Record<string, unknown>[];
    expect(skills).toHaveLength(1);
    expect(skills[0]).toMatchObject({
      skill_id: "s_alpha",
      name: "alpha",
      kind: "skill",
      protection: "open",
      version_id: V_ALPHA,
      bundle_digest: "d".repeat(64),
      generation: 1,
      via: { channels: ["eng"], direct: false },
    });
    expect(typeof skills[0]?.updated_at).toBe("number");
    const notices = body.notices as Record<string, unknown>[];
    expect(notices).toHaveLength(1);
    expect(notices[0]).toMatchObject({
      id: noticeId,
      kind: "verdict",
      skill_id: "s_alpha",
      skill_name: "alpha",
      actor: "Reviewer",
      outcome: "approve",
    });
  });

  it("include → remove move `beta` in and out of the member's delivery", async () => {
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);

    const included = await drive(
      profileSkillAction,
      req("PUT", `/api/v1/workspaces/${wsId}/profile/skills/s_beta`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_beta" },
    );
    expect(await included.json()).toEqual(okStatusBody("add", "included"));
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha", "s_beta"]);

    // Nothing broader provides beta, so the removal deletes the include line outright.
    const removed = await drive(
      profileSkillAction,
      req("DELETE", `/api/v1/workspaces/${wsId}/profile/skills/s_beta`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_beta" },
    );
    expect(await removed.json()).toEqual(okStatusBody("remove", "removed"));
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);

    // alpha IS provided broader (the eng channel the profile includes) — removing it records
    // the EXCLUDE line, disclosed in the answer.
    const excluded = await drive(
      profileSkillAction,
      req("DELETE", `/api/v1/workspaces/${wsId}/profile/skills/s_alpha`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(await excluded.json()).toEqual(okStatusBody("remove", "excluded"));
    expect(await deliveredSkillIds(CREDS.mem)).toEqual([]);
    // Re-including flips the stance back.
    const reIncluded = await drive(
      profileSkillAction,
      req("PUT", `/api/v1/workspaces/${wsId}/profile/skills/s_alpha`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(await reIncluded.json()).toEqual(okStatusBody("add", "included"));
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);
  });
});

// ── (f) the profile's channel lines (the DEFAULT channel's exclude arm included) ────────────

describe("profile channel lines", () => {
  it("include, remove, then remove-again (not_in_profile) on a named channel", async () => {
    const included = await drive(
      profileChannelAction,
      req("PUT", `/api/v1/workspaces/${wsId}/profile/channels/ops`, { cred: CREDS.mem }),
      { ws: wsId, channel: "ops" },
    );
    expect(await included.json()).toEqual(okStatusBody("add", "included"));
    const removed = await drive(
      profileChannelAction,
      req("DELETE", `/api/v1/workspaces/${wsId}/profile/channels/ops`, { cred: CREDS.mem }),
      { ws: wsId, channel: "ops" },
    );
    expect(await removed.json()).toEqual(okStatusBody("remove", "removed"));
    const again = await drive(
      profileChannelAction,
      req("DELETE", `/api/v1/workspaces/${wsId}/profile/channels/ops`, { cred: CREDS.mem }),
      { ws: wsId, channel: "ops" },
    );
    expect(await again.json()).toEqual(okStatusBody("remove", "not_in_profile"));
  });

  it("the DEFAULT channel: remove records the EXCLUDE line, include clears it", async () => {
    const removed = await drive(
      profileChannelAction,
      req("DELETE", `/api/v1/workspaces/${wsId}/profile/channels/everyone`, { cred: CREDS.mem }),
      { ws: wsId, channel: "everyone" },
    );
    expect(await removed.json()).toEqual(okStatusBody("remove", "excluded"));
    expect(
      await db.q(
        `SELECT 1 FROM web.profile_entry WHERE user_id = 'u_mem' AND mode = 'exclude' AND channel_id IS NOT NULL`,
      ),
    ).toHaveLength(1);

    const included = await drive(
      profileChannelAction,
      req("PUT", `/api/v1/workspaces/${wsId}/profile/channels/everyone`, { cred: CREDS.mem }),
      { ws: wsId, channel: "everyone" },
    );
    expect(await included.json()).toEqual(okStatusBody("add", "included"));
    expect(
      await db.q(
        `SELECT 1 FROM web.profile_entry WHERE user_id = 'u_mem' AND mode = 'exclude' AND channel_id IS NOT NULL`,
      ),
    ).toHaveLength(0);
  });

  it("an unknown channel is the uniform 404", async () => {
    const res = await drive(
      profileChannelAction,
      req("PUT", `/api/v1/workspaces/${wsId}/profile/channels/nope`, { cred: CREDS.mem }),
      { ws: wsId, channel: "nope" },
    );
    await expectUniform404(res);
  });
});

// ── (g) curation ─────────────────────────────────────────────────────────────────────────────

describe("curation", () => {
  it("create-on-first-use, place, remove, then remove-again (not_placed)", async () => {
    const created = await drive(
      curationAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/incubator/skills/s_beta`, {
        cred: CREDS.mem,
      }),
      { ws: wsId, channel: "incubator", skill: "s_beta" },
    );
    expect(await created.json()).toEqual(okStatusBody("channel", "created"));
    const placed = await drive(
      curationAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/incubator/skills/s_alpha`, {
        cred: CREDS.mem,
      }),
      { ws: wsId, channel: "incubator", skill: "s_alpha" },
    );
    expect(await placed.json()).toEqual(okStatusBody("channel", "placed"));
    const removed = await drive(
      curationAction,
      req("DELETE", `/api/v1/workspaces/${wsId}/channels/incubator/skills/s_alpha`, {
        cred: CREDS.mem,
      }),
      { ws: wsId, channel: "incubator", skill: "s_alpha" },
    );
    expect(await removed.json()).toEqual(okStatusBody("channel", "removed"));
    const notPlaced = await drive(
      curationAction,
      req("DELETE", `/api/v1/workspaces/${wsId}/channels/incubator/skills/s_alpha`, {
        cred: CREDS.mem,
      }),
      { ws: wsId, channel: "incubator", skill: "s_alpha" },
    );
    expect(await notPlaced.json()).toEqual(okStatusBody("channel", "not_placed"));
  });

  it("a reviewer curates a CURATED channel; an inactive bundle refuses typed", async () => {
    const placed = await drive(
      curationAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/locked/skills/s_beta`, { cred: CREDS.rev }),
      { ws: wsId, channel: "locked", skill: "s_beta" },
    );
    expect(await placed.json()).toEqual(okStatusBody("channel", "placed"));
    const archived = await drive(
      curationAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/ops/skills/s_arch`, { cred: CREDS.mem }),
      { ws: wsId, channel: "ops", skill: "s_arch" },
    );
    expect(await archived.json()).toEqual(deniedBody("channel", "SKILL_NOT_ACTIVE"));
  });
});

// ── (h) protections: the OK arms ─────────────────────────────────────────────────────────────

describe("protection writes", () => {
  it("owner and reviewer tighten a skill (set); the row flips", async () => {
    const owner = await drive(
      skillProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/skills/s_beta/protection`, {
        cred: CREDS.owner,
        body: { level: "reviewed" },
      }),
      { ws: wsId, skill: "s_beta" },
    );
    expect(await owner.json()).toEqual(okStatusBody("protect", "set"));
    const rows = await db.q<{ protection: string }>(
      `SELECT protection FROM web.bundle WHERE id = 's_beta'`,
    );
    expect(rows[0]?.protection).toBe("reviewed");
    const reviewer = await drive(
      skillProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/skills/s_beta/protection`, {
        cred: CREDS.rev,
        body: { level: "reviewed" },
      }),
      { ws: wsId, skill: "s_beta" },
    );
    expect(await reviewer.json()).toEqual(okStatusBody("protect", "set"));
    // Loosening back is the OWNER's act.
    const loosened = await drive(
      skillProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/skills/s_beta/protection`, {
        cred: CREDS.owner,
        body: { level: "open" },
      }),
      { ws: wsId, skill: "s_beta" },
    );
    expect(await loosened.json()).toEqual(okStatusBody("protect", "set"));
  });

  it("owner loosens the curated channel to open (set); the mode flips", async () => {
    const res = await drive(
      channelProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/locked/protection`, {
        cred: CREDS.owner,
        body: { level: "open" },
      }),
      { ws: wsId, channel: "locked" },
    );
    expect(await res.json()).toEqual(okStatusBody("protect", "set"));
    const rows = await db.q<{ mode: string }>(`SELECT mode FROM web.channel WHERE id = 'c_locked'`);
    expect(rows[0]?.mode).toBe("open");
    // Tighten it back (reviewer-grade) for any later reader.
    const tightened = await drive(
      channelProtAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/locked/protection`, {
        cred: CREDS.rev,
        body: { level: "curated" },
      }),
      { ws: wsId, channel: "locked" },
    );
    expect(await tightened.json()).toEqual(okStatusBody("protect", "set"));
  });
});

// ── (i) notices ack ──────────────────────────────────────────────────────────────────────────

describe("notices ack", () => {
  it("acks the caller's own notice (flips acked_at) and answers { status: acked }", async () => {
    const before = await db.q<{ acked_at: string | null }>(
      `SELECT acked_at FROM web.notice WHERE id = $1`,
      [noticeId],
    );
    expect(before[0]?.acked_at).toBeNull();

    const res = await drive(
      noticesAction,
      req("POST", `/api/v1/workspaces/${wsId}/notices/ack`, {
        cred: CREDS.mem,
        body: { ids: [noticeId] },
      }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual(okStatusBody("notices", "acked"));

    const after = await db.q<{ acked_at: string | null }>(
      `SELECT acked_at FROM web.notice WHERE id = $1`,
      [noticeId],
    );
    expect(after[0]?.acked_at).not.toBeNull();
  });
});

// ── (j) the applied-state report ─────────────────────────────────────────────────────────────

describe("report", () => {
  it("PUT /report → 204; a complete snapshot, workspace-membership-checked per bundle", async () => {
    const res = await drive(
      reportAction,
      req("PUT", `/api/v1/workspaces/${wsId}/report`, {
        cred: CREDS.mem,
        body: {
          schema_version: 1,
          applied: [
            { skill_id: "s_alpha", version_id: V_ALPHA },
            // Project-layer demand is legitimate: any catalog bundle may be reported.
            { skill_id: "s_beta", version_id: V_BETA },
            // Not a bundle of THIS workspace — the report must drop it.
            { skill_id: "s_foreign", version_id: V_ALPHA },
          ],
        },
      }),
      { ws: wsId },
    );
    expect(res.status).toBe(204);
    const rows = await db.q<{ bundle_id: string; applied_version_id: string }>(
      `SELECT bundle_id, applied_version_id FROM web.session_bundle_state WHERE session_id = $1 ORDER BY bundle_id`,
      [memDeviceId],
    );
    expect(rows).toEqual([
      { bundle_id: "s_alpha", applied_version_id: V_ALPHA },
      { bundle_id: "s_beta", applied_version_id: V_BETA },
    ]);
  });
});

// ── (k) the login flow's two unauthenticated doors ───────────────────────────────────────────

describe("login authorize + token", () => {
  it("authorize: an unknown workspace name is the uniform 404 (this install serves exactly one)", async () => {
    const res = await drive(
      deviceAuthorizeAction,
      req("POST", "/api/v1/login/authorize", {
        body: { requested_name: "ci-box", workspace: "acme" },
      }),
      {},
    );
    await expectUniform404(res);
  });

  it("authorize: an EMPTY workspace names the origin's own — the flow records the install's slug", async () => {
    const res = await drive(
      deviceAuthorizeAction,
      req("POST", "/api/v1/login/authorize", {
        body: { requested_name: "origin-box", workspace: "" },
      }),
      {},
    );
    expect(res.status).toBe(200);
    const flow = (await res.json()) as { user_code: string };
    const rows = await db.q<{ requested_workspace: string }>(
      `SELECT requested_workspace FROM web.login_flow WHERE user_code = $1`,
      [flow.user_code],
    );
    expect(rows[0]?.requested_workspace).toBe("team");
  });

  it("authorize → token: pending, then granted echoing the device_code as the credential", async () => {
    const started = await drive(
      deviceAuthorizeAction,
      req("POST", "/api/v1/login/authorize", {
        body: { requested_name: "ci-box", workspace: "team" },
      }),
      {},
    );
    expect(started.status).toBe(200);
    const flow = (await started.json()) as Record<string, unknown>;
    expect(Object.keys(flow).sort()).toEqual([
      "device_code",
      "expires_in_secs",
      "interval_secs",
      "user_code",
      "verification_uri",
    ]);
    // The code never enters ANY URL — the bare /verify address is all the wire carries.
    expect(flow.verification_uri).toBe("http://x/verify");
    expect(flow.expires_in_secs).toBe(900);
    expect(flow.interval_secs).toBe(5);

    const pending = await drive(
      deviceTokenAction,
      req("POST", "/api/v1/login/token", { body: { device_code: flow.device_code } }),
      {},
    );
    expect(await pending.json()).toEqual({ status: "pending" });

    // The matched non-empty name is recorded on the flow row verbatim.
    const flowRows = await db.q<{ requested_workspace: string }>(
      `SELECT requested_workspace FROM web.login_flow WHERE user_code = $1`,
      [flow.user_code as string],
    );
    expect(flowRows[0]?.requested_workspace).toBe("team");

    // The human approves at /verify (the ceremony's data layer stands in for the page).
    const identity = await import("@/lib/db/identity.server");
    const approved = await identity.approveLoginFlow(flow.user_code as string, {
      userId: "u_owner",
      display: "Owner",
    });
    expect(approved).not.toBeNull();

    const granted = await drive(
      deviceTokenAction,
      req("POST", "/api/v1/login/token", { body: { device_code: flow.device_code } }),
      {},
    );
    expect(await granted.json()).toEqual({
      status: "granted",
      credential: flow.device_code,
      session_id: approved?.sessionId,
      session_status: "active",
      workspace: { workspace_id: wsId, name: "team", display_name: "team" },
    });

    // The grant REPEATS (idempotent): a re-poll after a client crash re-delivers the same
    // credential, since the credential IS the presented device code.
    const again = await drive(
      deviceTokenAction,
      req("POST", "/api/v1/login/token", { body: { device_code: flow.device_code } }),
      {},
    );
    expect(await again.json()).toEqual({
      status: "granted",
      credential: flow.device_code,
      session_id: approved?.sessionId,
      session_status: "active",
      workspace: { workspace_id: wsId, name: "team", display_name: "team" },
    });
    const me = await drive(
      meLoader,
      req("GET", `/api/v1/workspaces/${wsId}/me`, { cred: flow.device_code as string }),
      { ws: wsId },
    );
    expect(me.status).toBe(200);
  });
});

// ── (l) the catalog reads ────────────────────────────────────────────────────────────────────

describe("skills index", () => {
  it("GET /skills — every pointered bundle (archived included, deleted/pointer-less not), id order", async () => {
    const res = await drive(
      skillsIndexLoader,
      req("GET", `/api/v1/workspaces/${wsId}/skills`, { cred: CREDS.mem }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as { skills: Record<string, unknown>[] };
    expect(body.skills.map((s) => s.skill_id)).toEqual(["s_alpha", "s_arch", "s_beta"]);
    expect(body.skills[0]).toMatchObject({
      skill_id: "s_alpha",
      name: "alpha",
      kind: "skill",
      status: "active",
      version_id: V_ALPHA,
      bundle_digest: "d".repeat(64),
      generation: 1,
      open_proposals: 0,
    });
    expect(typeof body.skills[0]?.updated_at).toBe("number");
    expect(body.skills[1]).toMatchObject({ skill_id: "s_arch", status: "archived" });
  });
});

describe("skill current (the conditional-GET currency read)", () => {
  it("200 with the WireCurrentRecord shape + the generation ETag, no-store", async () => {
    const res = await drive(
      skillCurrentLoader,
      req("GET", `/api/v1/workspaces/${wsId}/skills/s_alpha/current`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(res.status).toBe(200);
    expect(res.headers.get("etag")).toBe('"1"');
    expect(res.headers.get("cache-control")).toBe("no-store");
    expect(await res.json()).toEqual({
      schema_version: 1,
      scope: { workspace_id: wsId, skill_id: "s_alpha" },
      record: { version_id: V_ALPHA, generation: 1 },
    });
  });

  it("a matching If-None-Match answers 304 with the same ETag and no body", async () => {
    const res = await drive(
      skillCurrentLoader,
      req("GET", `/api/v1/workspaces/${wsId}/skills/s_alpha/current`, {
        cred: CREDS.mem,
        headers: { "if-none-match": '"1"' },
      }),
      { ws: wsId, skill: "s_alpha" },
    );
    expect(res.status).toBe(304);
    expect(res.headers.get("etag")).toBe('"1"');
    expect(await res.text()).toBe("");
  });

  it("a bundle with NO published version — and an unknown one — are the uniform 404", async () => {
    const unpublished = await drive(
      skillCurrentLoader,
      req("GET", `/api/v1/workspaces/${wsId}/skills/s_gamma/current`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_gamma" },
    );
    await expectUniform404(unpublished);
    const unknown = await drive(
      skillCurrentLoader,
      req("GET", `/api/v1/workspaces/${wsId}/skills/s_nope/current`, { cred: CREDS.mem }),
      { ws: wsId, skill: "s_nope" },
    );
    await expectUniform404(unknown);
  });
});
