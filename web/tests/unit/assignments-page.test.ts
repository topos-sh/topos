import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  assignBundleRow,
  assignChannelRow,
  bootWorkspace,
  createScratchDb,
  declineRow,
  placeBundle,
  placeInDefault,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedChannel,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * THE ASSIGNMENTS PAGE — the loader's grouping and the actions' self-scoped writes, against a
 * REAL scratch Postgres. What is proven here is the page's whole reason to exist: a person can
 * see not just WHAT they have but WHY, so every group is asserted with its attribution, and a
 * declined row is asserted to stay in its group (dimmed, switch intact) rather than vanish.
 *
 * The session is faked at the auth entry; seats, channels, assignments and declines are real
 * rows, so the guards and the feed predicate resolve exactly as production does.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

const ORIGIN = "http://x";

let db: ScratchDb;
let ws = "";

const ME = { id: "u_me", name: "Robin Member", email: "robin@example.com" };
const CURATOR = { id: "u_curator", name: "Casey Curator", email: "casey@example.com" };

/** In the baseline channel, and turned off by the reader — the dimmed-but-present case. */
const BASELINE_SKILL = { id: "s_base", name: "release-guide" };
/** In a named channel the reader carries themselves. */
const GUILD_SKILL = { id: "s_guild", name: "guild-guide" };
/** Aimed at the reader by name, by someone else. */
const AIMED_SKILL = { id: "s_aimed", name: "aimed-guide" };
/** Aimed at everyone as a bundle (the curator arm on a skill page). */
const WIDE_SKILL = { id: "s_wide", name: "wide-guide" };
/** The reader's own pick. */
const PICKED_SKILL = { id: "s_picked", name: "picked-guide" };
/** In the catalog and nothing else — the library's add case. */
const SHELF_SKILL = { id: "s_shelf", name: "shelf-guide" };

const GUILD_CHANNEL = "c_guild";

function signedInAs(user: { id: string; name: string; email: string }): void {
  session = { user };
}

async function callLoader(): Promise<{
  view: Awaited<ReturnType<typeof import("@/routes/profile").loader>>["view"];
  reaching: number;
  library: Awaited<ReturnType<typeof import("@/routes/profile").loader>>["library"];
}> {
  const { loader } = await import("@/routes/profile");
  return (await loader({
    request: new Request(`${ORIGIN}/profile`, { headers: { accept: "text/html" } }),
    params: {},
    context: {},
  } as unknown as Parameters<typeof loader>[0])) as never;
}

async function callAction(fields: Record<string, string>): Promise<{
  status: number;
  data: { intent: string; status: string };
}> {
  const { action } = await import("@/routes/profile");
  const body = new URLSearchParams(fields);
  const result = (await action({
    request: new Request(`${ORIGIN}/profile`, {
      method: "POST",
      headers: { origin: ORIGIN, "content-type": "application/x-www-form-urlencoded" },
      body,
    }),
    params: {},
    context: {},
  } as unknown as Parameters<typeof action>[0])) as {
    data: { intent: string; status: string };
    init?: { status?: number } | null;
  };
  return { status: result.init?.status ?? 200, data: result.data };
}

beforeAll(async () => {
  db = await createScratchDb("web_assignments", { TOPOS_WEB_RATELIMIT: "off" });
  ws = await bootWorkspace();
  await seedUser(db, ME.id, ME.name, ME.email);
  await seedUser(db, CURATOR.id, CURATOR.name, CURATOR.email);
  await seatUser(db, ws, ME.id, "member");
  await seatUser(db, ws, CURATOR.id, "owner");

  for (const skill of [
    BASELINE_SKILL,
    GUILD_SKILL,
    AIMED_SKILL,
    WIDE_SKILL,
    PICKED_SKILL,
    SHELF_SKILL,
  ]) {
    await seedBundle(db, ws, skill.id, skill.name);
  }

  await placeInDefault(db, ws, BASELINE_SKILL.id);
  await seedChannel(db, ws, GUILD_CHANNEL, "guild");
  await placeBundle(db, ws, GUILD_CHANNEL, GUILD_SKILL.id);
  // The reader carries the guild set themselves; the curator aims two bundles, one at the
  // reader by name and one at the whole workspace.
  await assignChannelRow(db, ws, GUILD_CHANNEL, ME.id, ME.id);
  await assignBundleRow(db, ws, AIMED_SKILL.id, ME.id, CURATOR.id);
  await assignBundleRow(db, ws, WIDE_SKILL.id, null, CURATOR.id);
  await assignBundleRow(db, ws, PICKED_SKILL.id, ME.id, ME.id);
  // …and the reader has switched the baseline's one skill off.
  await declineRow(db, ws, ME.id, BASELINE_SKILL.id);
  signedInAs(ME);
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the Mine view groups by what puts a skill there", () => {
  it("puts the default channel's skills under the baseline, attributed to everyone", async () => {
    const { view } = await callLoader();
    expect(view.baseline?.name).toBe("everyone");
    expect(view.baseline?.isDefault).toBe(true);
    expect(view.baseline?.attribution).toEqual({ by: "everyone" });
    expect(view.baseline?.bundles.map((b) => b.name)).toEqual([BASELINE_SKILL.name]);
  });

  it("keeps a declined skill IN its group, flagged — off for you, still in the library", async () => {
    const { view } = await callLoader();
    const row = view.baseline?.bundles.find((b) => b.name === BASELINE_SKILL.name);
    expect(row?.declined).toBe(true);
    // …and the same row is absent from the count of what actually reaches the agents.
    const { reaching } = await callLoader();
    expect(reaching).toBe(4);
  });

  it("lists each carried channel with its own attribution and today's contents", async () => {
    const { view } = await callLoader();
    expect(view.channels).toHaveLength(1);
    expect(view.channels[0]?.name).toBe("guild");
    // The reader placed the row themselves, so the page offers them the un-add.
    expect(view.channels[0]?.attribution).toEqual({ by: "you" });
    expect(view.channels[0]?.bundles.map((b) => b.name)).toEqual([GUILD_SKILL.name]);
  });

  it("names the person behind a curator's assignment, and says everyone for a wide one", async () => {
    const { view } = await callLoader();
    const byName = view.assigned.find((b) => b.name === AIMED_SKILL.name);
    expect(byName?.attribution).toEqual({ by: "person", display: CURATOR.name });
    const wide = view.assigned.find((b) => b.name === WIDE_SKILL.name);
    expect(wide?.attribution).toEqual({ by: "everyone" });
  });

  it("separates the reader's own picks from everything aimed at them", async () => {
    const { view } = await callLoader();
    expect(view.picked.map((b) => b.name)).toEqual([PICKED_SKILL.name]);
    expect(view.assigned.map((b) => b.name)).not.toContain(PICKED_SKILL.name);
  });

  it("carries the workspace's CURRENT version on a row, never a per-person pin", async () => {
    const { view } = await callLoader();
    expect(view.picked.map((b) => b.versionId)).toEqual([versionIdFor(PICKED_SKILL.id)]);
    expect(view.baseline?.bundles.map((b) => b.versionId)).toEqual([
      versionIdFor(BASELINE_SKILL.id),
    ]);
  });
});

describe("the Library view shows a state or an act, never both", () => {
  it("marks what the feed already carries, what is turned off, and what can be added", async () => {
    const { library } = await callLoader();
    const state = (name: string) => library.find((row) => row.name === name)?.state;
    expect(state(GUILD_SKILL.name)).toBe("mine");
    expect(state(WIDE_SKILL.name)).toBe("mine");
    expect(state(BASELINE_SKILL.name)).toBe("off");
    expect(state(SHELF_SKILL.name)).toBe("addable");
  });
});

describe("the self-scoped acts", () => {
  it("adding from the library writes the person's OWN row — audience and author both theirs", async () => {
    signedInAs(ME);
    const reply = await callAction({ intent: "add-skill", skill_id: SHELF_SKILL.id });
    expect(reply.data.status).toBe("added");
    const rows = await db.q<{ user_id: string; created_by: string }>(
      `SELECT user_id, created_by FROM web.assignment WHERE workspace_id = $1 AND bundle_id = $2`,
      [ws, SHELF_SKILL.id],
    );
    expect(rows).toHaveLength(1);
    expect(rows[0]?.user_id).toBe(ME.id);
    expect(rows[0]?.created_by).toBe(ME.id);
    const { view } = await callLoader();
    expect(view.picked.map((b) => b.name)).toContain(SHELF_SKILL.name);
    // Take it back so the rest of the suite reads the seeded shape.
    await callAction({ intent: "unpick-skill", skill_id: SHELF_SKILL.id });
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE bundle_id = $1`, [SHELF_SKILL.id]),
    ).toHaveLength(0);
  });

  it("adding a skill you had turned off clears the decline — the newer stance wins", async () => {
    signedInAs(ME);
    await declineRow(db, ws, ME.id, SHELF_SKILL.id);
    expect(
      await db.q(`SELECT 1 FROM web.decline WHERE user_id = $1 AND bundle_id = $2`, [
        ME.id,
        SHELF_SKILL.id,
      ]),
    ).toHaveLength(1);
    await callAction({ intent: "add-skill", skill_id: SHELF_SKILL.id });
    expect(
      await db.q(`SELECT 1 FROM web.decline WHERE user_id = $1 AND bundle_id = $2`, [
        ME.id,
        SHELF_SKILL.id,
      ]),
    ).toHaveLength(0);
    await callAction({ intent: "unpick-skill", skill_id: SHELF_SKILL.id });
  });

  it("turning a skill off records the one negative row, and turning it on clears it", async () => {
    signedInAs(ME);
    const off = await callAction({ intent: "decline-skill", skill_id: GUILD_SKILL.id });
    expect(off.data.status).toBe("declined");
    let view = (await callLoader()).view;
    expect(view.channels[0]?.bundles.find((b) => b.name === GUILD_SKILL.name)?.declined).toBe(true);
    const on = await callAction({ intent: "undecline-skill", skill_id: GUILD_SKILL.id });
    expect(on.data.status).toBe("cleared");
    view = (await callLoader()).view;
    expect(view.channels[0]?.bundles.find((b) => b.name === GUILD_SKILL.name)?.declined).toBe(
      false,
    );
  });

  it("un-adding a skill deletes only the reader's OWN row — never someone else's assignment", async () => {
    signedInAs(ME);
    const reply = await callAction({ intent: "unpick-skill", skill_id: AIMED_SKILL.id });
    // The curator's row is not this person's to take back: nothing is deleted, and it is still
    // there afterwards. Declining is the affordance for "not this one".
    expect(reply.data.status).toBe("not_picked");
    expect(
      await db.q(`SELECT 1 FROM web.assignment WHERE bundle_id = $1`, [AIMED_SKILL.id]),
    ).toHaveLength(1);
  });

  it("un-adding a carried channel takes back the reader's own set assignment", async () => {
    signedInAs(ME);
    const reply = await callAction({ intent: "unpick-channel", channel_id: GUILD_CHANNEL });
    expect(reply.data.status).toBe("unassigned");
    expect((await callLoader()).view.channels).toHaveLength(0);
    await assignChannelRow(db, ws, GUILD_CHANNEL, ME.id, ME.id);
  });

  it("refuses to drop the baseline — it reaches everyone, so no one person un-assigns it", async () => {
    signedInAs(ME);
    const baselineId = (
      await db.q<{ id: string }>(
        `SELECT id FROM web.channel WHERE workspace_id = $1 AND is_default`,
        [ws],
      )
    )[0]?.id as string;
    const reply = await callAction({ intent: "unpick-channel", channel_id: baselineId });
    expect(reply.data.status).toBe("baseline");
    expect((await callLoader()).view.baseline).not.toBeNull();
  });

  it("answers an unknown intent with a 400 that only a member can reach", async () => {
    signedInAs(ME);
    const reply = await callAction({ intent: "nonsense" });
    expect(reply.status).toBe(400);
    expect(reply.data.intent).toBe("unknown");
  });
});
