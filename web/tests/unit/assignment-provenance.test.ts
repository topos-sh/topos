import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asOwner,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedChannel,
  seedUser,
} from "./helpers/scratch-db";

/**
 * Assignment PROVENANCE, against a REAL scratch Postgres: a self-pick and a curator assignment
 * of the same target are TWO rows (`self` participates in the partial uniques), each unassign
 * arm deletes only its own provenance, and delivery unions both — so an owner's withdraw can
 * never eat a person's own pick, and a person's unpick can never eat a curator's aim.
 */

let db: ScratchDb;
let ws = "";

const OWNER = "u_owner";
const MEMBER = "u_member";
const SKILL = { id: "s_prov", name: "prov-guide" };
const CHANNEL = "c_prov";

async function feed() {
  return import("@/lib/db/queries.feed.server");
}
async function lane() {
  return import("@/lib/db/queries.lane.server");
}

const member = () => ({ userId: MEMBER, workspaceId: ws });

async function assignmentRows(match: { bundleId?: string; channelId?: string }) {
  const col = match.bundleId === undefined ? "channel_id" : "bundle_id";
  const val = match.bundleId ?? match.channelId;
  return await db.q<{ self: boolean; created_by: string }>(
    `SELECT self, created_by FROM web.assignment
     WHERE workspace_id = $1 AND user_id = $2 AND ${col} = $3
     ORDER BY self`,
    [ws, MEMBER, val],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_assign_provenance");
  ws = await bootWorkspace();
  await seedUser(db, OWNER, "Olive Owner", "olive@example.com");
  await seedUser(db, MEMBER, "Mo Member", "mo@example.com");
  await seatUser(db, ws, OWNER, "owner");
  await seatUser(db, ws, MEMBER, "member");
  await seedBundle(db, ws, SKILL.id, SKILL.name);
  await seedChannel(db, ws, CHANNEL, "prov-channel");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("a self-pick and a curator assignment coexist", () => {
  it("keeps one row per provenance, and delivery serves the bundle once", async () => {
    const f = await feed();
    expect(await f.addToMine(member(), SKILL.id)).toBe("added");
    expect(await f.assignBundle(asOwner(ws, OWNER, "Olive"), SKILL.id, { userId: MEMBER })).toBe(
      "assigned",
    );
    const rows = await assignmentRows({ bundleId: SKILL.id });
    expect(rows.map((r) => r.self)).toEqual([false, true]);
    expect(rows.find((r) => !r.self)?.created_by).toBe(OWNER);
    expect(rows.find((r) => r.self)?.created_by).toBe(MEMBER);

    const l = await lane();
    const body = await l.deliveryFor(member());
    expect(body.skills.filter((s) => s.skill_id === SKILL.id)).toHaveLength(1);
  });

  it("owner unassign deletes ONLY the curator row — the person's own pick stands", async () => {
    const f = await feed();
    expect(
      await f.unassign(asOwner(ws, OWNER, "Olive"), { bundleId: SKILL.id }, { userId: MEMBER }),
    ).toBe("unassigned");
    const rows = await assignmentRows({ bundleId: SKILL.id });
    // What survives is the person's OWN row; delivery still carries it.
    expect(rows.map((r) => r.self)).toEqual([true]);
    expect(rows[0]?.created_by).toBe(MEMBER);
    const l = await lane();
    const body = await l.deliveryFor(member());
    expect(body.skills.some((s) => s.skill_id === SKILL.id)).toBe(true);
    // A second withdraw has nothing of ITS provenance left — the pick is not the owner's.
    expect(
      await f.unassign(asOwner(ws, OWNER, "Olive"), { bundleId: SKILL.id }, { userId: MEMBER }),
    ).toBe("not_assigned");
  });

  it("unpick deletes ONLY the self row — a curator's aim survives it", async () => {
    const f = await feed();
    // Re-aim it (the pick from the previous test still stands).
    expect(await f.assignBundle(asOwner(ws, OWNER, "Olive"), SKILL.id, { userId: MEMBER })).toBe(
      "assigned",
    );
    expect(await f.unpickBundle(member(), SKILL.id)).toBe("unpicked");
    const rows = await assignmentRows({ bundleId: SKILL.id });
    expect(rows.map((r) => r.self)).toEqual([false]);
    // Nothing self-owned left to unpick; the curator's aim keeps delivering.
    expect(await f.unpickBundle(member(), SKILL.id)).toBe("not_picked");
    const l = await lane();
    const body = await l.deliveryFor(member());
    expect(body.skills.some((s) => s.skill_id === SKILL.id)).toBe(true);
  });
});

describe("channel provenance", () => {
  it("self channel unassign never deletes the curator's row", async () => {
    const f = await feed();
    expect(await f.assignChannel(asOwner(ws, OWNER, "Olive"), CHANNEL, { userId: MEMBER })).toBe(
      "assigned",
    );
    // The self-carry coexists with the curator's aim rather than colliding into it.
    expect(await f.assignChannelToSelf(member(), CHANNEL)).toBe("assigned");
    expect((await assignmentRows({ channelId: CHANNEL })).map((r) => r.self)).toEqual([
      false,
      true,
    ]);
    expect(await f.unassignChannelFromSelf(member(), CHANNEL)).toBe("unassigned");
    expect((await assignmentRows({ channelId: CHANNEL })).map((r) => r.self)).toEqual([false]);
    // Nothing of the person's own left — the curator's row is not theirs to drop.
    expect(await f.unassignChannelFromSelf(member(), CHANNEL)).toBe("not_assigned");
  });
});
