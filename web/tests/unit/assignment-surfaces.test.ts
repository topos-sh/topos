import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  asMember,
  assignBundleRow,
  bootWorkspace,
  createScratchDb,
  placeBundle,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedChannel,
  seedSession,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * The other three assignment surfaces, against a REAL scratch Postgres:
 *
 *  · the VISIBILITY page — the disclosure of what a workspace reads from a member's machines,
 *    whose loader must serve the reader's OWN sessions and nobody else's (the page's whole
 *    claim rests on the list being exactly the fields the prose names);
 *  · the per-bundle DETAIL reads — which channels carry a skill, and which of the viewer's own
 *    sessions hold it, at which version;
 *  · the CURATOR arms on the channel and skill pages — owner-only, and refused with the same
 *    uniform 404 every other owner-only act answers with.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

const ORIGIN = "http://x";

let db: ScratchDb;
let ws = "";

const OWNER = { id: "u_owner", name: "Olive Owner", email: "olive@example.com" };
const MEMBER = { id: "u_member", name: "Mo Member", email: "mo@example.com" };

const SKILL = { id: "s_visible", name: "release-guide" };
const OTHER = { id: "s_other", name: "handbook" };
const CHANNEL = "c_backend";

const MY_SESSION = "sn_mine";
const MY_OTHER_SESSION = "sn_mine_two";
const THEIR_SESSION = "sn_theirs";

function signedInAs(user: { id: string; name: string; email: string }): void {
  session = { user };
}

/** Normalize a route outcome — a return, a thrown Response, or a thrown data() — to status. */
async function statusOf(run: () => Promise<unknown>): Promise<number> {
  try {
    const returned = (await run()) as { init?: { status?: number } | null } | Response;
    if (returned instanceof Response) {
      return returned.status;
    }
    return returned?.init?.status ?? 200;
  } catch (thrown) {
    if (thrown instanceof Response) {
      return thrown.status;
    }
    const dataThrow = thrown as { init?: { status?: number } | null };
    return dataThrow.init?.status ?? 0;
  }
}

function post(path: string, fields: Record<string, string>): Request {
  return new Request(`${ORIGIN}${path}`, {
    method: "POST",
    headers: { origin: ORIGIN, "content-type": "application/x-www-form-urlencoded" },
    body: new URLSearchParams(fields),
  });
}

beforeAll(async () => {
  db = await createScratchDb("web_assign_surfaces", { TOPOS_WEB_RATELIMIT: "off" });
  ws = await bootWorkspace();
  await seedUser(db, OWNER.id, OWNER.name, OWNER.email);
  await seedUser(db, MEMBER.id, MEMBER.name, MEMBER.email);
  await seatUser(db, ws, OWNER.id, "owner");
  await seatUser(db, ws, MEMBER.id, "member");
  await seedBundle(db, ws, SKILL.id, SKILL.name);
  await seedBundle(db, ws, OTHER.id, OTHER.name, { withPointer: false });
  await seedChannel(db, ws, CHANNEL, "backend");
  await placeBundle(db, ws, CHANNEL, SKILL.id);

  // Two machines of the member's own, and one of somebody else's — the visibility page must
  // never widen past the reader.
  await seedSession(db, MY_SESSION, ws, MEMBER.id, "active", "laptop");
  await seedSession(db, MY_OTHER_SESSION, ws, MEMBER.id, "active", "desktop");
  await seedSession(db, THEIR_SESSION, ws, OWNER.id, "active", "owner-box");
  await db.q(
    `INSERT INTO web.session_bundle_state (session_id, bundle_id, applied_version_id)
     VALUES ($1, $2, $3), ($4, $2, $5), ($6, $2, $3)`,
    [
      MY_SESSION,
      SKILL.id,
      versionIdFor(SKILL.id),
      MY_OTHER_SESSION,
      "an-older-version",
      THEIR_SESSION,
    ],
  );
  await db.q(`UPDATE web.cli_session SET last_seen_at = now() WHERE id = $1`, [MY_SESSION]);
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the visibility page", () => {
  it("lists the reader's OWN machines and the four fields the page promises", async () => {
    signedInAs(MEMBER);
    const { loader } = await import("@/routes/visibility");
    const result = (await loader({
      request: new Request(`${ORIGIN}/visibility`, { headers: { accept: "text/html" } }),
      params: {},
      context: {},
    } as unknown as Parameters<typeof loader>[0])) as {
      sessions: {
        sessionId: string;
        displayName: string;
        lastSeenAtMs: number | null;
        skills: { name: string; appliedVersionId: string }[];
      }[];
    };
    expect(result.sessions.map((s) => s.sessionId).sort()).toEqual(
      [MY_SESSION, MY_OTHER_SESSION].sort(),
    );
    const laptop = result.sessions.find((s) => s.sessionId === MY_SESSION);
    expect(laptop?.displayName).toBe("laptop");
    expect(typeof laptop?.lastSeenAtMs).toBe("number");
    expect(laptop?.skills).toEqual([
      { name: SKILL.name, appliedVersionId: versionIdFor(SKILL.id) },
    ]);
    // A machine that has never synced says so honestly rather than being hidden.
    expect(result.sessions.find((s) => s.sessionId === MY_OTHER_SESSION)?.lastSeenAtMs).toBeNull();
  });

  it("never shows another person's machine, whatever their role", async () => {
    signedInAs(OWNER);
    const { loader } = await import("@/routes/visibility");
    const result = (await loader({
      request: new Request(`${ORIGIN}/visibility`, { headers: { accept: "text/html" } }),
      params: {},
      context: {},
    } as unknown as Parameters<typeof loader>[0])) as { sessions: { sessionId: string }[] };
    // The owner sees the whole workspace on the Sessions page; this page is about THEM.
    expect(result.sessions.map((s) => s.sessionId)).toEqual([THEIR_SESSION]);
  });
});

describe("the per-bundle detail reads", () => {
  it("names every channel that carries the bundle, and none that do not", async () => {
    const { channelsCarrying } = await import("@/lib/db/queries.channels.server");
    const carried = await channelsCarrying(asMember(ws, MEMBER.id), SKILL.id);
    expect(carried.map((c) => c.name)).toEqual(["backend"]);
    expect(await channelsCarrying(asMember(ws, MEMBER.id), OTHER.id)).toEqual([]);
  });

  it("reports the reader's own machines with the version each holds and whether it is current", async () => {
    const { yourSessionsApplying } = await import("@/lib/db/queries.sessions.server");
    const applied = await yourSessionsApplying(asMember(ws, MEMBER.id), SKILL.id);
    expect(applied.map((s) => s.displayName).sort()).toEqual(["desktop", "laptop"]);
    expect(applied.find((s) => s.displayName === "laptop")?.current).toBe(true);
    // The other machine reports an older id, so it reads behind — the same comparison the
    // Sessions page makes, asked one bundle at a time.
    expect(applied.find((s) => s.displayName === "desktop")?.current).toBe(false);
    // Somebody else's machine holds it too; it is not this reader's business.
    expect(applied.map((s) => s.sessionId)).not.toContain(THEIR_SESSION);
  });

  it("answers whether the workspace-wide assignment exists on a target", async () => {
    const { assignedToEveryone } = await import("@/lib/db/queries.feed.server");
    const actor = asMember(ws, MEMBER.id);
    expect(await assignedToEveryone(actor, { bundleId: SKILL.id })).toBe(false);
    await assignBundleRow(db, ws, SKILL.id, null, OWNER.id);
    expect(await assignedToEveryone(actor, { bundleId: SKILL.id })).toBe(true);
    await db.q(`DELETE FROM web.assignment WHERE bundle_id = $1 AND user_id IS NULL`, [SKILL.id]);
  });
});

describe("the curator arm on a skill page", () => {
  it("assigns to everyone and withdraws it, each act audited", async () => {
    signedInAs(OWNER);
    const { action } = await import("@/routes/skill-current");
    const assign = (await action({
      request: post(`/skills/${SKILL.name}`, {
        intent: "assign-everyone",
        skill_id: SKILL.id,
      }),
      params: { skill: SKILL.name },
      context: {},
    } as unknown as Parameters<typeof action>[0])) as { data: { status: string } };
    expect(assign.data.status).toBe("assigned");
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE bundle_id = $1 AND user_id IS NULL`, [
        SKILL.id,
      ]),
    ).toHaveLength(1);

    const withdraw = (await action({
      request: post(`/skills/${SKILL.name}`, {
        intent: "unassign-everyone",
        skill_id: SKILL.id,
      }),
      params: { skill: SKILL.name },
      context: {},
    } as unknown as Parameters<typeof action>[0])) as { data: { status: string } };
    expect(withdraw.data.status).toBe("unassigned");
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE bundle_id = $1 AND user_id IS NULL`, [
        SKILL.id,
      ]),
    ).toHaveLength(0);
    const audited = await db.q<{ kind: string }>(
      `SELECT kind FROM web.audit_event WHERE subject = $1 ORDER BY id`,
      [SKILL.id],
    );
    expect(audited.map((r) => r.kind)).toEqual(["assigned", "unassigned"]);
  });

  it("refuses a plain member with the uniform 404 — never a 403, never a typed refusal", async () => {
    signedInAs(MEMBER);
    const { action } = await import("@/routes/skill-current");
    const status = await statusOf(() =>
      action({
        request: post(`/skills/${SKILL.name}`, {
          intent: "assign-everyone",
          skill_id: SKILL.id,
        }),
        params: { skill: SKILL.name },
        context: {},
      } as unknown as Parameters<typeof action>[0]),
    );
    expect(status).toBe(404);
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE bundle_id = $1 AND user_id IS NULL`, [
        SKILL.id,
      ]),
    ).toHaveLength(0);
  });
});

describe("the curator arm on a channel page", () => {
  it("assigns the set to everyone and withdraws it", async () => {
    signedInAs(OWNER);
    const { action } = await import("@/routes/channel-detail");
    const assign = (await action({
      request: post("/channels/backend", {
        intent: "assign-everyone",
        channel_id: CHANNEL,
      }),
      params: { channel: "backend" },
      context: {},
    } as unknown as Parameters<typeof action>[0])) as { data: { form: string; error: string } };
    expect(assign.data).toEqual({ form: "everyone", error: "" });
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE channel_id = $1 AND user_id IS NULL`, [
        CHANNEL,
      ]),
    ).toHaveLength(1);

    // …and the channel page now reads it back as the arm's own state.
    const { channelDetail } = await import("@/lib/db/queries.channels.server");
    const detail = await channelDetail(asMember(ws, MEMBER.id), "backend");
    expect(detail?.everyoneAssigned).toBe(true);
    // Assigned to everyone means the audience IS the roster, and every member is included.
    expect(detail?.viewerIncluded).toBe(true);

    const withdraw = (await action({
      request: post("/channels/backend", {
        intent: "unassign-everyone",
        channel_id: CHANNEL,
      }),
      params: { channel: "backend" },
      context: {},
    } as unknown as Parameters<typeof action>[0])) as { data: { form: string; error: string } };
    expect(withdraw.data).toEqual({ form: "everyone", error: "" });
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE channel_id = $1 AND user_id IS NULL`, [
        CHANNEL,
      ]),
    ).toHaveLength(0);
  });

  it("refuses to unassign the BASELINE, however the intent arrives", async () => {
    // The default channel's everyone-assignment is what membership gives. No page offers to
    // withdraw it — but a hidden form field is not a rule, so the crafted intent goes straight at
    // the route and the data layer refuses it.
    const rows = await db.q<{ id: string; name: string }>(
      `SELECT id, name FROM web.channel WHERE workspace_id = $1 AND is_default`,
      [ws],
    );
    const row = rows[0];
    if (row === undefined) {
      throw new Error("the workspace has no default channel");
    }
    const before = await db.q(
      `SELECT 1 FROM web.assignment WHERE channel_id = $1 AND user_id IS NULL`,
      [row.id],
    );
    expect(before).toHaveLength(1);

    signedInAs(OWNER);
    const { action } = await import("@/routes/channel-detail");
    const refused = (await action({
      request: post(`/channels/${row.name}`, {
        intent: "unassign-everyone",
        channel_id: row.id,
      }),
      params: { channel: row.name },
      context: {},
    } as unknown as Parameters<typeof action>[0])) as {
      data: { form: string; error: string };
    };
    expect(refused.data.form).toBe("everyone");
    expect(refused.data.error).toMatch(/baseline/i);
    // The witness: the row every member's feed rests on is still there.
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE channel_id = $1 AND user_id IS NULL`, [
        row.id,
      ]),
    ).toHaveLength(1);
  });

  it("refuses a plain member with the uniform 404", async () => {
    signedInAs(MEMBER);
    const { action } = await import("@/routes/channel-detail");
    const status = await statusOf(() =>
      action({
        request: post("/channels/backend", {
          intent: "assign-everyone",
          channel_id: CHANNEL,
        }),
        params: { channel: "backend" },
        context: {},
      } as unknown as Parameters<typeof action>[0]),
    );
    expect(status).toBe(404);
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE channel_id = $1 AND user_id IS NULL`, [
        CHANNEL,
      ]),
    ).toHaveLength(0);
  });

  it("reads the baseline back as assigned to everyone — the row the workspace is born with", async () => {
    const { channelDetail } = await import("@/lib/db/queries.channels.server");
    const detail = await channelDetail(asMember(ws, MEMBER.id), "everyone");
    expect(detail?.isDefault).toBe(true);
    expect(detail?.everyoneAssigned).toBe(true);
  });
});
