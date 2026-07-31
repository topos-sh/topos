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
import { action as loginConnectAction } from "@/routes/api.v1.login-connect";
import { action as deviceTokenAction } from "@/routes/api.v1.login-token";
import { loader as meLoader, action as meWrongMethod } from "@/routes/api.v1.me";
import { action as noticesAction } from "@/routes/api.v1.notices-ack";
import { action as reportAction, loader as reportWrongMethod } from "@/routes/api.v1.report";
import { loader as skillCurrentLoader } from "@/routes/api.v1.skill-current";
import { action as skillProtAction } from "@/routes/api.v1.skill-protection";
import { loader as reachLoader } from "@/routes/api.v1.skill-reach";
import { loader as skillsIndexLoader } from "@/routes/api.v1.skills-index";
import {
  assignBundleRow,
  assignChannelRow,
  createScratchDb,
  declineRow,
  placeBundle,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedChannel,
  seedSession,
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

interface DeliveredSkill {
  skill_id: string;
  via: { channels: string[]; direct: boolean; assigned_by?: string; picked?: true };
}

/** The caller's delivered skills, through the REAL delivery route. */
async function deliveredSkills(cred: string): Promise<DeliveredSkill[]> {
  const res = await drive(
    deliveryLoader,
    req("GET", `/api/v1/workspaces/${wsId}/delivery`, { cred }),
    {
      ws: wsId,
    },
  );
  expect(res.status).toBe(200);
  const body = (await res.json()) as { skills: DeliveredSkill[] };
  return body.skills;
}

/** The caller's delivered skill ids, sorted. */
async function deliveredSkillIds(cred: string): Promise<string[]> {
  return (await deliveredSkills(cred)).map((s) => s.skill_id).sort();
}

/** ONE delivered skill (with its `via` attribution), or undefined when it does not arrive. */
async function deliveredSkill(cred: string, skillId: string): Promise<DeliveredSkill | undefined> {
  return (await deliveredSkills(cred)).find((s) => s.skill_id === skillId);
}

// ── fixture ──────────────────────────────────────────────────────────────────────────────────

/** The full login ceremony: start → approve (as `userId`, picking the one seat) → poll — THE
 * POLL is the exchange that mints; the device_code IS the credential. */
async function mintCredential(
  userId: string,
  display: string,
  requestedName: string,
): Promise<{ credential: string; sessionId: string }> {
  const identity = await import("@/lib/db/identity.server");
  const flow = await identity.startLoginFlow(requestedName, "team");
  await identity.approveLoginFlow(
    flow.userCode,
    { userId, display },
    { kind: "seat", workspace: "team" },
  );
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
  // The member carries `eng` (a self-assignment of the set); the owner is assigned alpha
  // directly — so alpha's reach counts two people through two different kinds of row.
  await assignChannelRow(db, wsId, "c_eng", "u_mem");
  await assignBundleRow(db, wsId, "s_alpha", "u_owner");
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
      curationAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/eng/skills/s_beta`, { cred: CREDS.revoked }),
      { ws: wsId, channel: "eng", skill: "s_beta" },
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
      curationAction,
      req("POST", `/api/v1/workspaces/${wsId}/channels/eng/skills/s_alpha`, { cred: CREDS.mem }),
      { ws: wsId, channel: "eng", skill: "s_alpha" },
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

  it("a curation write on an unknown skill folds to the uniform 404 (never an existence oracle)", async () => {
    const res = await drive(
      curationAction,
      req("PUT", `/api/v1/workspaces/${wsId}/channels/eng/skills/s_nope`, { cred: CREDS.mem }),
      { ws: wsId, channel: "eng", skill: "s_nope" },
    );
    await expectUniform404(res);
  });

  // The RETIRED feed-writing paths. Nothing a machine presents decides what a person should
  // have any more, so these paths are not served at all — and the splat answers them
  // BYTE-IDENTICALLY to a path that never existed, with a good credential and a bad one alike.
  it.each([
    { name: "profile read", method: "GET", path: `/profile` },
    { name: "profile skill include", method: "PUT", path: `/profile/skills/s_alpha` },
    { name: "profile skill remove", method: "DELETE", path: `/profile/skills/s_alpha` },
    { name: "profile channel include", method: "PUT", path: `/profile/channels/ops` },
    { name: "profile channel remove", method: "DELETE", path: `/profile/channels/everyone` },
  ])("$name is no longer served — the splat answers it", async ({ method, path }) => {
    const url = `/api/v1/workspaces/${wsId}${path}`;
    const handler = method === "GET" ? catchAllLoader : catchAllAction;
    const served = await drive(handler, req(method, url, { cred: CREDS.mem }), {});
    expect(served.status).toBe(404);
    const body = await served.json();
    expect(body).toEqual(NOT_FOUND_BODY);
    const garbage = await drive(
      handler,
      req(method, "/api/v1/not/a/route", { cred: CREDS.mem }),
      {},
    );
    expect(garbage.status).toBe(404);
    expect(await garbage.json()).toEqual(body);
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

// ── (e) delivery over the assignment/decline model ──────────────────────────────────────────

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
    // The caller's standing declines ride the body — always present, empty here.
    expect(body.declined).toEqual([]);
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

  it("delivery follows the rows: baseline, everyone-assignment, decline, self-assignment", async () => {
    // The starting picture: alpha only, through the `eng` channel the member carries.
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);

    // The BASELINE is a row like any other: placing beta in the default channel delivers it
    // to every seat, with no per-person row anywhere.
    await db.q(
      `INSERT INTO web.channel_bundle (channel_id, workspace_id, bundle_id)
       SELECT id, workspace_id, 's_beta' FROM web.channel WHERE is_default AND workspace_id = $1`,
      [wsId],
    );
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha", "s_beta"]);
    const viaBaseline = await deliveredSkill(CREDS.mem, "s_beta");
    expect(viaBaseline?.via).toEqual({ channels: ["everyone"], direct: false });

    // A DECLINE subtracts it — the one negative row, and it beats every source.
    await declineRow(db, wsId, "u_mem", "s_beta");
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);

    // A decline holds even against a direct assignment to this person — and while it stands,
    // the body's `declined` list names the bundle so a client can narrate the stance.
    await assignBundleRow(db, wsId, "s_beta", "u_mem");
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);
    const withStance = await drive(
      deliveryLoader,
      req("GET", `/api/v1/workspaces/${wsId}/delivery`, { cred: CREDS.mem }),
      { ws: wsId },
    );
    expect(((await withStance.json()) as { declined: unknown }).declined).toEqual([
      { skill_id: "s_beta", name: "beta" },
    ]);
    // … and clearing it lets the assignment through, now flagged direct AND via the baseline —
    // the person's own row (created_by = themselves), so the self-pick fact rides.
    await db.q(`DELETE FROM web.decline WHERE user_id = 'u_mem' AND bundle_id = 's_beta'`);
    const both = await deliveredSkill(CREDS.mem, "s_beta");
    expect(both?.via).toEqual({ channels: ["everyone"], direct: true, picked: true });

    // Restore the fixture for the suites that follow.
    await db.q(`DELETE FROM web.assignment WHERE user_id = 'u_mem' AND bundle_id = 's_beta'`);
    await db.q(`DELETE FROM web.channel_bundle WHERE bundle_id = 's_beta'`);
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);
  });

  it("an EVERYONE assignment of a bundle delivers with no channel behind it", async () => {
    await assignBundleRow(db, wsId, "s_beta", null, "u_owner");
    const served = await deliveredSkill(CREDS.mem, "s_beta");
    // Someone ELSE aimed it here, so the creator's display rides — and no `picked`, which
    // marks only the caller's own row.
    expect(served?.via).toEqual({ channels: [], direct: true, assigned_by: "Owner" });
    // It reaches the owner too — one row, whole roster.
    expect(await deliveredSkillIds(CREDS.owner)).toContain("s_beta");
    await db.q(`DELETE FROM web.assignment WHERE bundle_id = 's_beta' AND user_id IS NULL`);
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);
  });

  it("a channel assigned to ONE person delivers its members to them alone", async () => {
    await placeBundle(db, wsId, "c_ops", "s_beta");
    await assignChannelRow(db, wsId, "c_ops", "u_mem", "u_owner");
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha", "s_beta"]);
    const served = await deliveredSkill(CREDS.mem, "s_beta");
    expect(served?.via).toEqual({ channels: ["ops"], direct: false });
    // The owner holds no row for `ops`, so nothing of it reaches them.
    expect(await deliveredSkillIds(CREDS.owner)).not.toContain("s_beta");
    await db.q(`DELETE FROM web.assignment WHERE user_id = 'u_mem' AND channel_id = 'c_ops'`);
    await db.q(`DELETE FROM web.channel_bundle WHERE channel_id = 'c_ops'`);
    expect(await deliveredSkillIds(CREDS.mem)).toEqual(["s_alpha"]);
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

// ── (k) the login flow's two unauthenticated doors + the lane-side connect ───────────────────

describe("login authorize + token", () => {
  it("authorize: a shape-invalid preselect is the uniform 404 (such a name can never exist)", async () => {
    const res = await drive(
      deviceAuthorizeAction,
      req("POST", "/api/v1/login/authorize", {
        body: { requested_name: "ci-box", preselect: "Not A Slug!" },
      }),
      {},
    );
    await expectUniform404(res);
  });

  it("authorize: the start is WORKSPACE-LESS — no workspace field, the flow mints regardless", async () => {
    const res = await drive(
      deviceAuthorizeAction,
      req("POST", "/api/v1/login/authorize", {
        body: { requested_name: "origin-box" },
      }),
      {},
    );
    expect(res.status).toBe(200);
    const flow = (await res.json()) as { user_code: string };
    const rows = await db.q<{ preselect_workspace: string | null }>(
      `SELECT preselect_workspace FROM web.login_flow WHERE user_code = $1`,
      [flow.user_code],
    );
    expect(rows[0]?.preselect_workspace).toBeNull();
  });

  it("authorize → approve → token: pending, then THE POLL MINTS, echoing the device_code as the credential", async () => {
    const started = await drive(
      deviceAuthorizeAction,
      req("POST", "/api/v1/login/authorize", {
        body: { requested_name: "ci-box", preselect: "team" },
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

    // The preselect is recorded on the flow row verbatim — display-only, unresolved.
    const flowRows = await db.q<{ preselect_workspace: string }>(
      `SELECT preselect_workspace FROM web.login_flow WHERE user_code = $1`,
      [flow.user_code as string],
    );
    expect(flowRows[0]?.preselect_workspace).toBe("team");

    // The human approves at /verify, picking their seat (the ceremony's data layer stands in
    // for the page). Consent + the chosen workspace land; NO session exists yet.
    const identity = await import("@/lib/db/identity.server");
    const approved = await identity.approveLoginFlow(
      flow.user_code as string,
      { userId: "u_owner", display: "Owner" },
      { kind: "seat", workspace: "team" },
    );
    expect(approved?.outcome).toBe("approved");
    expect(await db.q(`SELECT 1 FROM web.cli_session WHERE display_name = 'ci-box'`)).toHaveLength(
      0,
    );

    // THE EXCHANGE: the first poll mints and answers granted with the chosen workspace.
    const granted = await drive(
      deviceTokenAction,
      req("POST", "/api/v1/login/token", { body: { device_code: flow.device_code } }),
      {},
    );
    const grantedBody = (await granted.json()) as Record<string, unknown>;
    const sessionId = grantedBody.session_id as string;
    expect(grantedBody).toEqual({
      status: "granted",
      credential: flow.device_code,
      session_id: sessionId,
      session_status: "active",
      workspace: { workspace_id: wsId, name: "team", display_name: "team" },
    });

    // The grant REPEATS (idempotent): a re-poll after a client crash re-delivers the same
    // credential and the SAME session, since the credential IS the presented device code.
    const again = await drive(
      deviceTokenAction,
      req("POST", "/api/v1/login/token", { body: { device_code: flow.device_code } }),
      {},
    );
    expect(await again.json()).toEqual({
      status: "granted",
      credential: flow.device_code,
      session_id: sessionId,
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

describe("login connect (the lane-side second connect)", () => {
  it("an ACTIVE credential + a seat mints a FRESH session, returned exactly once", async () => {
    const res = await drive(
      loginConnectAction,
      req("POST", "/api/v1/login/connect", {
        cred: CREDS.mem,
        body: { workspace: "team", requested_name: "mem-second-box" },
      }),
      {},
    );
    expect(res.status).toBe(200);
    const body = (await res.json()) as {
      credential: string;
      session_id: string;
      session_status: string;
      workspace: { workspace_id: string; name: string; display_name: string };
    };
    // A FRESH secret — never the presented one (the flow-code promotion is the browser flow's
    // trick; the connect mints anew over an authenticated exchange).
    expect(body.credential).not.toBe(CREDS.mem);
    expect(body.session_status).toBe("active");
    expect(body.workspace).toEqual({ workspace_id: wsId, name: "team", display_name: "team" });
    // The new credential is live on the lane; the audit row carries the connect provenance.
    const me = await drive(
      meLoader,
      req("GET", `/api/v1/workspaces/${wsId}/me`, { cred: body.credential }),
      { ws: wsId },
    );
    expect(me.status).toBe(200);
    const audits = await db.q<{ details: { via?: string; requestedName?: string } }>(
      `SELECT details FROM web.audit_event
       WHERE kind = 'session_created' AND subject = $1`,
      [body.session_id],
    );
    expect(audits[0]?.details).toEqual({
      via: "connect",
      status: "active",
      requestedName: "mem-second-box",
    });
  });

  it("a slug naming anything but the install's workspace is the uniform 404", async () => {
    const res = await drive(
      loginConnectAction,
      req("POST", "/api/v1/login/connect", {
        cred: CREDS.mem,
        body: { workspace: "acme", requested_name: "wrong-ws-box" },
      }),
      {},
    );
    await expectUniform404(res);
  });

  it("a severed person's credential (seat gone, session dead) is the uniform 404", async () => {
    const res = await drive(
      loginConnectAction,
      req("POST", "/api/v1/login/connect", {
        cred: CREDS.stranger,
        body: { workspace: "team", requested_name: "stranger-box" },
      }),
      {},
    );
    await expectUniform404(res);
  });

  it("a REVOKED session's credential is the uniform 404 — resolved inside the mint fence", async () => {
    const res = await drive(
      loginConnectAction,
      req("POST", "/api/v1/login/connect", {
        cred: CREDS.revoked,
        body: { workspace: "team", requested_name: "zombie-box" },
      }),
      {},
    );
    await expectUniform404(res);
  });

  it("a PENDING session's credential is refused — only an ACTIVE session connects onward", async () => {
    await seedSession(db, "sn_conn_pending", wsId, "u_mem", "pending");
    const res = await drive(
      loginConnectAction,
      req("POST", "/api/v1/login/connect", {
        cred: "sn_conn_pending",
        body: { workspace: "team", requested_name: "held-box" },
      }),
      {},
    );
    await expectUniform404(res);
    await db.q(`DELETE FROM web.cli_session WHERE id = 'sn_conn_pending'`);
  });

  it("a wrong method and a missing bearer are the uniform 404", async () => {
    const { loader } = await import("@/routes/api.v1.login-connect");
    await expectUniform404((loader as () => Response)());
    const res = await drive(
      loginConnectAction,
      req("POST", "/api/v1/login/connect", {
        body: { workspace: "team", requested_name: "anon-box" },
      }),
      {},
    );
    await expectUniform404(res);
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

  it("GET /skills — a recorded upstream rides its entry (additive); absent rows omit the fields", async () => {
    await db.q(
      `INSERT INTO web.bundle_upstream (bundle_id, workspace_id, host, repo, path)
       VALUES ('s_alpha', $1, 'github.com', 'owner/alpha', 'skills/alpha')
       ON CONFLICT (bundle_id) DO NOTHING`,
      [wsId],
    );
    const res = await drive(
      skillsIndexLoader,
      req("GET", `/api/v1/workspaces/${wsId}/skills`, { cred: CREDS.mem }),
      { ws: wsId },
    );
    expect(res.status).toBe(200);
    const body = (await res.json()) as { skills: Record<string, unknown>[] };
    expect(body.skills[0]).toMatchObject({
      skill_id: "s_alpha",
      upstream_host: "github.com",
      upstream_repo: "owner/alpha",
      upstream_path: "skills/alpha",
    });
    // A bundle nobody imported carries none of the upstream fields (absent, not null).
    expect(body.skills[2]?.skill_id).toBe("s_beta");
    expect(body.skills[2]).not.toHaveProperty("upstream_host");
    await db.q(`DELETE FROM web.bundle_upstream WHERE bundle_id = 's_alpha'`);
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
