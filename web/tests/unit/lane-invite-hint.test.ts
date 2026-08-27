import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedChannel,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * The session lane's invitation FIRST-DESTINATION hint. The wire spells the bundle arm `skill`,
 * but it resolves ANY bundle in the catalog — MCP servers included — so what the destination IS
 * has to be read off the row that resolved, never inferred from which field the caller filled
 * in. Two readers depend on that: the audit row (the workspace's own record of what was sent)
 * and the invitation mail (which names the destination to a stranger, in their words).
 */

let db: ScratchDb;
let wsId = "";

/** The `hint` object the invitation's audit row recorded, or undefined when it recorded none. */
async function auditedHint(email: string): Promise<{ kind: string; name: string } | undefined> {
  const rows = await db.q<{ hint: { kind: string; name: string } | null }>(
    `SELECT details -> 'hint' AS hint FROM web.audit_event
     WHERE kind = 'invitation_created' AND subject = $1`,
    [email],
  );
  return rows[0]?.hint ?? undefined;
}

beforeAll(async () => {
  db = await createScratchDb("web_lane_invite_hint");
  wsId = await bootWorkspace();
  await seedUser(db, "u_own", "Owner", "owner@example.com");
  await seatUser(db, wsId, "u_own", "owner");
  await seedSession(db, "sn_own", wsId, "u_own");
  await seedBundle(db, wsId, "s_deploy", "deploy");
  await seedBundle(db, wsId, "s_linear", "linear", { kind: "mcp" });
  await seedChannel(db, wsId, "ch_ops", "ops");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("laneInvite — the hint says what the destination IS", () => {
  it("records and returns the bundle's OWN kind, so an mcp server is never called a skill", async () => {
    const { laneInvite } = await import("@/lib/db/queries.lane.server");
    const actor = asSession(wsId, "u_own", "sn_own", "owner");

    const mcp = await laneInvite(actor, ["ann@example.com"], { skill: "linear" });
    expect(mcp.outcome).toBe("invited");
    // The workspace's own record of what was sent.
    expect(await auditedHint("ann@example.com")).toEqual({ kind: "mcp", name: "linear" });
    // And what the route hands the mail — the same fact, so the mail says "MCP server" rather
    // than "skill" to the person reading it.
    expect(mcp.outcome === "invited" && mcp.hint).toEqual({ kind: "mcp", name: "linear" });

    // A real skill through the same field still reads as one — the fix is a lookup, not a flip.
    const skill = await laneInvite(actor, ["bo@example.com"], { skill: "deploy" });
    expect(await auditedHint("bo@example.com")).toEqual({ kind: "skill", name: "deploy" });
    expect(skill.outcome === "invited" && skill.hint).toEqual({ kind: "skill", name: "deploy" });
  });

  it("a channel hint stays a channel; no hint records none", async () => {
    const { laneInvite } = await import("@/lib/db/queries.lane.server");
    const actor = asSession(wsId, "u_own", "sn_own", "owner");

    const channel = await laneInvite(actor, ["cy@example.com"], { channel: "ops" });
    expect(channel.outcome === "invited" && channel.hint).toEqual({
      kind: "channel",
      name: "ops",
    });
    expect(await auditedHint("cy@example.com")).toEqual({ kind: "channel", name: "ops" });

    const bare = await laneInvite(actor, ["di@example.com"], {});
    expect(bare.outcome === "invited" && bare.hint).toBeUndefined();
    expect(await auditedHint("di@example.com")).toBeUndefined();
  });

  it("an unresolvable destination refuses before any invitation row is written", async () => {
    const { laneInvite } = await import("@/lib/db/queries.lane.server");
    const actor = asSession(wsId, "u_own", "sn_own", "owner");
    expect((await laneInvite(actor, ["ed@example.com"], { skill: "ghost" })).outcome).toBe(
      "unknown_skill",
    );
    expect((await laneInvite(actor, ["ed@example.com"], { channel: "ghost" })).outcome).toBe(
      "unknown_channel",
    );
    expect(await db.q(`SELECT 1 FROM web.invitation WHERE email = 'ed@example.com'`)).toHaveLength(
      0,
    );
  });
});
