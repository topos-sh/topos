import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { mcpRevisionId } from "../helpers/mcp-ids";
import {
  assignBundleRow,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";

/**
 * WHAT A MACHINE RECEIVES FOR A CONNECTED SERVER, against a REAL scratch Postgres.
 *
 * The lane used to answer one question for every kind — here is a pointer, go fetch the bytes.
 * A server has no bytes to fetch, so it is answered in full: the document rides the delivery and
 * the revision id is the handle a device reports back. The suite holds the three resolutions
 * apart (a pin behind the current, a server's current, a server with nothing current), and proves
 * the two lists never overlap.
 */

let db: ScratchDb;
let ws = "";

const MEMBER = "u_mem";

async function lane() {
  return await import("@/lib/db/queries.lane.server");
}

const member = () => ({ userId: MEMBER, workspaceId: ws });
const session = () =>
  ({
    userId: MEMBER,
    workspaceId: ws,
    sessionId: "cs_one",
    role: "member",
    display: "Mo",
    sessionStatus: "active",
  }) as unknown as Parameters<Awaited<ReturnType<typeof lane>>["laneSkillsIndex"]>[0];

function document(name: string, version: string): Record<string, unknown> {
  return {
    name,
    description: "A server for the suite.",
    version,
    remotes: [{ type: "streamable-http", url: "https://mcp.example.com/mcp" }],
  };
}

/** A server row: public when `workspaceId` is null, otherwise that workspace's own. */
async function seedServer(id: string, name: string, workspaceId: string | null) {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ($1, $2, $3, $3, 'none', 'active')`,
    [id, workspaceId, name],
  );
}

/** One revision, and (when current) the pointer move that makes it the one people receive. A
 *  revision that was ever on offer carries the promotion stamp — every one seeded here has been. */
async function seedRevision(
  serverId: string,
  id: string,
  seq: number,
  version: string,
  opts: { current?: boolean } = {},
) {
  const name = (
    await db.q<{ name: string }>(`SELECT name FROM web.mcp_server WHERE id = $1`, [serverId])
  )[0]?.name as string;
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, upstream_version, document, transport, url, published_at, published_by)
     VALUES ($1, $2, $3, $4, $5::jsonb, 'streamable-http', 'https://mcp.example.com/mcp',
             now(), 'Staff')`,
    [id, serverId, seq, version, JSON.stringify(document(name, version))],
  );
  if (opts.current === true) {
    await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [serverId, id]);
  }
}

/** The connection row a bundle carries — the whole of what makes a bundle an MCP server here. */
async function connect(bundleId: string, serverId: string, pinned: string | null = null) {
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id, pinned_revision_id)
     VALUES ($1, $2, $3, $4)`,
    [bundleId, ws, serverId, pinned],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_delivery");
  ws = await bootWorkspace();
  await seedUser(db, MEMBER, "Mo Member", "mo@example.com");
  await seatUser(db, ws, MEMBER, "member");

  // A skill, untouched by any of this — the control.
  await seedBundle(db, ws, "b_skill", "writing-guide");
  await assignBundleRow(db, ws, "b_skill", MEMBER);

  // The catalog's own server, followed at its current revision.
  await seedServer("mcps_glob", "com.example/global", null);
  await seedRevision("mcps_glob", mcpRevisionId("glob_1"), 1, "1.0.0");
  await seedRevision("mcps_glob", mcpRevisionId("glob_2"), 2, "2.0.0", { current: true });
  await seedBundle(db, ws, "b_glob", "global-server", { kind: "mcp", withPointer: false });
  await connect("b_glob", "mcps_glob");
  await assignBundleRow(db, ws, "b_glob", MEMBER);

  // The same catalog server, PINNED by another workspace bundle — to a revision behind the current.
  await seedServer("mcps_pin", "com.example/pinned", null);
  await seedRevision("mcps_pin", mcpRevisionId("pin_1"), 1, "1.0.0");
  await seedRevision("mcps_pin", mcpRevisionId("pin_2"), 2, "2.0.0", { current: true });
  await seedBundle(db, ws, "b_pin", "pinned-server", { kind: "mcp", withPointer: false });
  await connect("b_pin", "mcps_pin", mcpRevisionId("pin_1"));
  await assignBundleRow(db, ws, "b_pin", MEMBER);

  // A server with nothing published — a connection that resolves to nothing.
  await seedServer("mcps_empty", "com.example/empty", null);
  await seedBundle(db, ws, "b_empty", "empty-server", { kind: "mcp", withPointer: false });
  await connect("b_empty", "mcps_empty");
  await assignBundleRow(db, ws, "b_empty", MEMBER);

  // A kind-'mcp' bundle that still carries vault bytes and NO connection — the shape a backfill
  // has not reached yet.
  await seedBundle(db, ws, "b_stale", "stale-server", { kind: "mcp" });
  await assignBundleRow(db, ws, "b_stale", MEMBER);
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("delivery serves a connected server whole", () => {
  it("carries the document inline with the revision it resolved to", async () => {
    const body = await (await lane()).deliveryFor(member());
    const served = body.mcp_servers.find((row) => row.skill_id === "b_glob");
    expect(served).toBeDefined();
    expect(served?.revision_id).toBe(mcpRevisionId("glob_2"));
    expect(served?.document).toMatchObject({ name: "com.example/global", version: "2.0.0" });
    expect(served?.name).toBe("global-server");
    expect(served?.kind).toBe("mcp");
    // Absence is how the wire spells "not pinned" and "not revoked".
    expect(served && "pinned" in served).toBe(false);
    expect(served && "revoked" in served).toBe(false);
    expect(served?.updated_at).toBeGreaterThan(0);
    expect(served?.via.direct).toBe(true);
  });

  it("follows a pin to a revision behind the current, and says it is pinned", async () => {
    const body = await (await lane()).deliveryFor(member());
    const served = body.mcp_servers.find((row) => row.skill_id === "b_pin");
    expect(served?.revision_id).toBe(mcpRevisionId("pin_1"));
    expect(served?.document).toMatchObject({ version: "1.0.0" });
    expect(served?.pinned).toBe(true);
  });

  it("says nothing at all about a server with nothing published", async () => {
    const body = await (await lane()).deliveryFor(member());
    expect(body.mcp_servers.map((row) => row.skill_id)).not.toContain("b_empty");
    expect(body.skills.map((row) => row.skill_id)).not.toContain("b_empty");
  });

  it("never serves a server as a file bundle, connected or not", async () => {
    const body = await (await lane()).deliveryFor(member());
    expect(body.skills.map((row) => row.skill_id)).toEqual(["b_skill"]);
    expect(body.skills[0]?.bundle_digest).toHaveLength(64);
    // The unconnected leftover carries a live pointer and still delivers nothing: its kind says
    // where its document comes from, and no connection means no document.
    expect(body.mcp_servers.map((row) => row.skill_id)).not.toContain("b_stale");
  });

  it("is shape-complete and empty for a session still waiting on approval", async () => {
    const body = await (await lane()).emptyDeliveryFor(session());
    expect(body.mcp_servers).toEqual([]);
    expect(body.skills).toEqual([]);
  });
});

describe("the workspace catalog splits the same way", () => {
  it("keeps servers out of the skills index and serves them with their document", async () => {
    const l = await lane();
    const skills = await l.laneSkillsIndex(session());
    expect(skills.map((row) => row.skill_id)).toEqual(["b_skill"]);

    const servers = await l.laneMcpServersIndex(session());
    expect(servers.map((row) => row.skill_id)).toEqual(["b_glob", "b_pin"]);
    expect(servers.find((row) => row.skill_id === "b_pin")?.pinned).toBe(true);
    expect(servers.find((row) => row.skill_id === "b_glob")?.document).toMatchObject({
      name: "com.example/global",
    });
  });
});
