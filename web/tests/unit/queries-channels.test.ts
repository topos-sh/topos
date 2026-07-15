import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asMember,
  asOwner,
  bootWorkspace,
  createScratchDb,
  placeBundle,
  placeInDefault,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The CHANNELS DAL (queries.channels.server.ts) against a REAL scratch Postgres. A channel is
 * plain rows; the DEFAULT channel's membership is IMPLICIT (every seat minus explicit
 * `channel_optout` rows), the existence ceremonies (create / rename / delete) are id-keyed
 * owner transactions refusing the default channel, and the self-service opt-out writes the
 * detach records its lapse earns — the opt-in clears them.
 */

let db: ScratchDb;
let wsId = "";
let engId = "";

async function q() {
  return import("@/lib/db/queries.channels.server");
}

beforeAll(async () => {
  db = await createScratchDb("web_channels");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_ana", "Ana", "ana@example.com");
  await seedUser(db, "u_bo", "Bo", "bo@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seatUser(db, wsId, "u_ana", "member");
  await seatUser(db, wsId, "u_bo", "member");
  // Two bundles with pointers: one rides the default channel, one a named one.
  await seedBundle(db, wsId, "s_doc", "doc-helper");
  await placeInDefault(db, wsId, "s_doc");
  await seedBundle(db, wsId, "s_tool", "tool-helper");
  const queries = await q();
  const created = await queries.createChannel(asMember(wsId, "u_ana", "member", "Ana"), "eng");
  if (created.outcome !== "created") {
    throw new Error("eng channel seed failed");
  }
  engId = created.channelId;
  await placeBundle(db, wsId, engId, "s_tool");
  await db.q(
    `INSERT INTO web.channel_member (channel_id, workspace_id, user_id, added_by)
     VALUES ($1, $2, 'u_ana', 'u_owner')`,
    [engId, wsId],
  );
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("channelsOf (the index read)", () => {
  it("lists the default first, then name order, counting implicit membership minus opt-outs", async () => {
    const queries = await q();
    const rows = await queries.channelsOf(asMember(wsId, "u_ana"));
    expect(rows.map((r) => [r.name, r.isDefault, r.skillCount, r.memberCount])).toEqual([
      ["everyone", true, 1, 3],
      ["eng", false, 1, 1],
    ]);

    // An opt-out subtracts from the DEFAULT channel's derived count only.
    await db.q(
      `INSERT INTO web.channel_optout (channel_id, workspace_id, user_id)
       SELECT id, workspace_id, 'u_bo' FROM web.channel WHERE is_default AND workspace_id = $1`,
      [wsId],
    );
    const after = await queries.channelsOf(asMember(wsId, "u_ana"));
    expect(after.map((r) => [r.name, r.memberCount])).toEqual([
      ["everyone", 2],
      ["eng", 1],
    ]);
    await db.q(`DELETE FROM web.channel_optout WHERE user_id = 'u_bo'`);
  });
});

describe("channelDetail (the one-channel read)", () => {
  it("the DEFAULT channel: no member rows, a derived count, and the viewer's opt-out stance", async () => {
    const queries = await q();
    const detail = await queries.channelDetail(asMember(wsId, "u_ana"), "everyone");
    expect(detail).toMatchObject({
      name: "everyone",
      isDefault: true,
      members: [],
      defaultMemberCount: 3,
      viewerIsMember: true,
    });
    expect(detail?.skills.map((s) => s.skillId)).toEqual(["s_doc"]);

    await db.q(
      `INSERT INTO web.channel_optout (channel_id, workspace_id, user_id)
       SELECT id, workspace_id, 'u_ana' FROM web.channel WHERE is_default AND workspace_id = $1`,
      [wsId],
    );
    const optedOut = await queries.channelDetail(asMember(wsId, "u_ana"), "everyone");
    expect(optedOut?.viewerIsMember).toBe(false);
    expect(optedOut?.defaultMemberCount).toBe(2);
    await db.q(`DELETE FROM web.channel_optout WHERE user_id = 'u_ana'`);
  });

  it("a NAMED channel: explicit member rows decide the viewer's stance", async () => {
    const queries = await q();
    const asAna = await queries.channelDetail(asMember(wsId, "u_ana"), "eng");
    expect(asAna?.viewerIsMember).toBe(true);
    expect(asAna?.members.map((m) => [m.userId, m.display])).toEqual([["u_ana", "Ana"]]);
    expect(asAna?.skills.map((s) => [s.skillId, s.name, s.status])).toEqual([
      ["s_tool", "tool-helper", "active"],
    ]);
    const asBo = await queries.channelDetail(asMember(wsId, "u_bo"), "eng");
    expect(asBo?.viewerIsMember).toBe(false);
    expect(await queries.channelDetail(asMember(wsId, "u_ana"), "nope")).toBeUndefined();
  });
});

describe("createChannel", () => {
  it("refuses a bad charset, a leading hyphen, and an over-length name", async () => {
    const queries = await q();
    expect(await queries.createChannel(asMember(wsId, "u_ana"), "Bad_Name")).toEqual({
      outcome: "bad_name",
    });
    expect(await queries.createChannel(asMember(wsId, "u_ana"), "-lead")).toEqual({
      outcome: "bad_name",
    });
    expect(await queries.createChannel(asMember(wsId, "u_ana"), "a".repeat(65))).toEqual({
      outcome: "bad_name",
    });
  });

  // (Drizzle wraps the pg error — the unique-violation probe reads the code through `.cause`.)
  it("refuses a taken name as the typed name_taken (the unique index is the race arbiter)", async () => {
    const queries = await q();
    expect(await queries.createChannel(asMember(wsId, "u_ana"), "eng")).toEqual({
      outcome: "name_taken",
    });
  });
});

describe("renameChannel (id-keyed, owner)", () => {
  it("refuses the default channel and a bad name; renames otherwise, id-keyed", async () => {
    const queries = await q();
    const owner = asOwner(wsId, "u_owner", "Owner");
    const everyone = await queries.channelKeyByName(asMember(wsId, "u_ana"), "everyone");
    expect(await queries.renameChannel(owner, everyone?.channelId ?? "", "all-hands")).toBe(
      "builtin",
    );
    expect(await queries.renameChannel(owner, engId, "Bad_Name")).toBe("bad_name");
    expect(await queries.renameChannel(owner, engId, "unknown-yet")).toBe("renamed");
    // The rename is visible under the NEW name; the old one no longer resolves.
    expect(await queries.channelKeyByName(asMember(wsId, "u_ana"), "unknown-yet")).toMatchObject({
      channelId: engId,
    });
    expect(await queries.channelKeyByName(asMember(wsId, "u_ana"), "eng")).toBeUndefined();
    expect(await queries.renameChannel(owner, "c_nope", "whatever")).toBe("unknown_channel");
    // Restore for the later cases.
    expect(await queries.renameChannel(owner, engId, "eng")).toBe("renamed");
  });

  // (Same wrapped-23505 discipline as createChannel — the probe reads through `.cause`.)
  it("refuses a taken name as the typed name_taken", async () => {
    const queries = await q();
    expect(await queries.renameChannel(asOwner(wsId, "u_owner", "Owner"), engId, "everyone")).toBe(
      "name_taken",
    );
  });
});

describe("deleteChannel (id-keyed, owner)", () => {
  it("cascades references + memberships; the audit trail keeps the channel id as subject", async () => {
    const queries = await q();
    const owner = asOwner(wsId, "u_owner", "Owner");
    const everyone = await queries.channelKeyByName(asMember(wsId, "u_ana"), "everyone");
    expect(await queries.deleteChannel(owner, everyone?.channelId ?? "")).toBe("builtin");
    expect(await queries.deleteChannel(owner, "c_nope")).toBe("unknown_channel");

    expect(await queries.deleteChannel(owner, engId)).toBe("deleted");
    expect(await db.q(`SELECT 1 FROM web.channel WHERE id = $1`, [engId])).toHaveLength(0);
    expect(
      await db.q(`SELECT 1 FROM web.channel_bundle WHERE channel_id = $1`, [engId]),
    ).toHaveLength(0);
    expect(
      await db.q(`SELECT 1 FROM web.channel_member WHERE channel_id = $1`, [engId]),
    ).toHaveLength(0);
    // History is append-only and OUTLIVES the row it names.
    const audit = await db.q<{ kind: string }>(
      `SELECT kind FROM web.audit_event WHERE subject = $1 ORDER BY id`,
      [engId],
    );
    expect(audit.map((a) => a.kind)).toContain("channel_created");
    expect(audit.map((a) => a.kind)).toContain("channel_deleted");
  });
});

describe("the default channel's self-service opt-out", () => {
  it("opting out writes the detach records its lapse earns; opting back in clears them", async () => {
    const queries = await q();
    const ana = asMember(wsId, "u_ana", "member", "Ana");
    expect(await queries.optOutDefaultChannel(ana)).toBe("left");
    // s_doc was delivered via the default channel alone → the lapse is recorded, cause-tagged.
    const detached = await db.q<{ bundle_id: string; cause: string }>(
      `SELECT bundle_id, cause FROM web.bundle_detachment WHERE user_id = 'u_ana'`,
    );
    expect(detached).toEqual([{ bundle_id: "s_doc", cause: "channel_leave" }]);
    // Idempotence: a second opt-out is the honest not_member.
    expect(await queries.optOutDefaultChannel(ana)).toBe("not_member");

    expect(await queries.optInDefaultChannel(ana)).toBe("joined");
    expect(await db.q(`SELECT 1 FROM web.channel_optout WHERE user_id = 'u_ana'`)).toHaveLength(0);
    // Re-entitled: the record heals (entitlement always wins over a stale record).
    expect(await db.q(`SELECT 1 FROM web.bundle_detachment WHERE user_id = 'u_ana'`)).toHaveLength(
      0,
    );
  });
});
