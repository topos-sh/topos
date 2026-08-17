import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { canonicalServerJson } from "@/lib/mcp/fetch.server";
import { validateCandidateFiles } from "@/lib/mcp/validate.server";
import {
  asMember,
  asOwner,
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * THE MCP CATALOG, against a REAL scratch Postgres — the rows, the keys that refuse, and the one
 * fenced write every act above them is built from.
 *
 * Two halves, deliberately not merged. The DATABASE half drives raw SQL at the constraints so a
 * violation is proved to come from the database rather than from an app-tier check somebody could
 * quietly remove: the partial uniques (a global name is one server's; a private name is nobody
 * else's business; a version is upstream's promise and only upstream's), and the composite keys
 * that keep a pointer or a pin inside its own server's history. The QUERY half drives the real
 * functions and asserts the invariants no key can express: a pointer that only ever names a
 * published revision, one connection per server per workspace, and two writers who both think
 * they are first ending up as revision 1 and revision 2 rather than as one lost write.
 *
 * The scratch database carries the migrations whole, so the seeded catalog is here too — and the
 * suite holds it to the same publish gate a member's own paste answers to.
 */

let db: ScratchDb;
let wsId = "";
let otherWsId = "";

async function catalog() {
  return await import("@/lib/db/queries.mcp-catalog.server");
}

/** A minimal document the gate accepts: one name, one https remote, one version. */
function serverDocument(
  name: string,
  version: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    name,
    description: "A server for the suite.",
    version,
    remotes: [{ type: "streamable-http", url: "https://mcp.example.com/mcp" }],
    ...extra,
  };
}

/** A global catalog row with no revisions yet — the shape a sweep's pull-in leaves behind. */
async function seedGlobalServer(
  id: string,
  registryName: string,
  extra: { status?: string; authMode?: string | null; authNote?: string | null } = {},
): Promise<string> {
  await db.q(
    `INSERT INTO web.mcp_server (id, registry_name, display_name, auth_mode, auth_note, status)
     VALUES ($1, $2, $3, $4, $5, $6)`,
    [
      id,
      registryName,
      registryName,
      extra.authMode === undefined ? "none" : extra.authMode,
      extra.authNote ?? null,
      extra.status ?? "active",
    ],
  );
  return id;
}

/** One revision write, in its own transaction — what every caller of the fenced write does. */
async function addRevision(
  serverId: string,
  write: {
    document: Record<string, unknown>;
    source: "registry" | "staff" | "owner" | "seed";
    publish: boolean;
    attribution?: string;
  },
) {
  const { addMcpRevisionInTx } = await catalog();
  const { getDb } = await import("@/lib/db/index.server");
  return await getDb().transaction((tx) =>
    addMcpRevisionInTx(tx, serverId, { ...write, attribution: write.attribution ?? "Staff" }),
  );
}

async function currentRevisionOf(serverId: string): Promise<string | null> {
  const rows = await db.q<{ current_revision_id: string | null }>(
    `SELECT current_revision_id FROM web.mcp_server WHERE id = $1`,
    [serverId],
  );
  return rows[0]?.current_revision_id ?? null;
}

async function revisionStatus(revisionId: string): Promise<string | undefined> {
  const rows = await db.q<{ status: string }>(
    `SELECT status FROM web.mcp_server_revision WHERE id = $1`,
    [revisionId],
  );
  return rows[0]?.status;
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_catalog");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seatUser(db, wsId, "u_mem", "member");
  // A SECOND workspace, so "private servers do not collide across workspaces" is a claim about
  // two real tenants rather than about one row twice.
  otherWsId = "w_other";
  await db.q(
    `INSERT INTO web.workspace (id, name, display_name, claimed_at) VALUES ($1, 'other', 'Other', now())`,
    [otherWsId],
  );
  await seatUser(db, otherWsId, "u_owner", "owner");
}, 90000);

afterAll(async () => {
  await db.drop();
});

describe("the seeded catalog", () => {
  it("every seeded server stands active with a published current revision", async () => {
    const rows = await db.q<{ servers: string; pointed: string; published: string }>(
      `SELECT count(*) AS servers,
              count(s.current_revision_id) AS pointed,
              count(*) FILTER (WHERE r.status = 'published') AS published
       FROM web.mcp_server s
       LEFT JOIN web.mcp_server_revision r ON r.id = s.current_revision_id
       WHERE s.workspace_id IS NULL AND s.status = 'active'`,
    );
    const row = rows[0];
    expect(Number(row?.servers)).toBeGreaterThan(0);
    expect(row?.pointed).toBe(row?.servers);
    expect(row?.published).toBe(row?.servers);
  });

  it("every seeded document passes the publish gate a pasted one answers to", async () => {
    const rows = await db.q<{ registry_name: string; document: Record<string, unknown> }>(
      `SELECT s.registry_name, r.document
       FROM web.mcp_server s JOIN web.mcp_server_revision r ON r.id = s.current_revision_id
       WHERE s.workspace_id IS NULL`,
    );
    const failures: string[] = [];
    for (const row of rows) {
      const validated = validateCandidateFiles([
        {
          path: "server.json",
          bytes: new TextEncoder().encode(canonicalServerJson(row.document)),
        },
      ]);
      if (!validated.ok) {
        failures.push(`${row.registry_name}: ${validated.code} ${validated.message}`);
      } else if (validated.summary.name !== row.registry_name) {
        failures.push(`${row.registry_name}: document says ${validated.summary.name}`);
      }
    }
    expect(failures).toEqual([]);
  });

  it("carries the editorial half: a manual row says what the person has to do", async () => {
    const rows = await db.q<{ auth_mode: string; auth_note: string | null; icon: string | null }>(
      `SELECT auth_mode, auth_note, icon FROM web.mcp_server WHERE registry_name = 'com.github/mcp'`,
    );
    expect(rows[0]?.auth_mode).toBe("manual");
    expect(rows[0]?.auth_note ?? "").not.toBe("");
    expect(rows[0]?.icon).toBe("github");
  });

  it("no seeded row is left without a stated sign-in tier", async () => {
    const rows = await db.q(
      `SELECT registry_name FROM web.mcp_server WHERE workspace_id IS NULL AND auth_mode IS NULL`,
    );
    expect(rows).toEqual([]);
  });
});

describe("the global namespace is one server's", () => {
  it("a second global row claiming a name the catalog holds is refused by the index", async () => {
    await expect(
      db.q(
        `INSERT INTO web.mcp_server (id, registry_name, display_name, auth_mode)
         VALUES ('mcps_dupe', 'com.github/mcp', 'Impostor', 'none')`,
      ),
    ).rejects.toThrow(/mcp_server_global_registry_name/);
  });

  it("private rows collide with nobody — not the catalog, not each other", async () => {
    await db.q(
      `INSERT INTO web.mcp_server (id, workspace_id, registry_name, display_name, auth_mode)
       VALUES ('mcps_priv_a', $1, 'com.github/mcp', 'Ours', 'none')`,
      [wsId],
    );
    await db.q(
      `INSERT INTO web.mcp_server (id, workspace_id, registry_name, display_name, auth_mode)
       VALUES ('mcps_priv_b', $1, 'com.github/mcp', 'Theirs', 'none')`,
      [otherWsId],
    );
    const rows = await db.q(
      `SELECT id FROM web.mcp_server WHERE registry_name = 'com.github/mcp' ORDER BY id`,
    );
    expect(rows).toHaveLength(3);
  });
});

describe("a version is upstream's promise, and only upstream's", () => {
  it("a second registry-sourced revision of one version is refused, with its reason", async () => {
    const serverId = await seedGlobalServer("mcps_ver", "com.example/versions");
    const first = await addRevision(serverId, {
      document: serverDocument("com.example/versions", "1.2.3"),
      source: "registry",
      publish: false,
    });
    expect(first.refusal).toBeNull();
    const second = await addRevision(serverId, {
      document: serverDocument("com.example/versions", "1.2.3", { description: "Reworded." }),
      source: "registry",
      publish: false,
    });
    expect(second.refusal?.code).toBe("MCP_VERSION_HELD");
  });

  it("and the index says the same thing to a writer that skipped the check", async () => {
    await expect(
      db.q(
        `INSERT INTO web.mcp_server_revision
           (id, server_id, seq, status, upstream_version, document, source)
         VALUES ('mcpr_raw', 'mcps_ver', 99, 'candidate', '1.2.3', '{}'::jsonb, 'registry')`,
      ),
    ).rejects.toThrow(/mcp_server_revision_upstream_version/);
  });

  it("an owner's edits of one version are ordinary revisions, one after another", async () => {
    const { createPrivateMcpServer, editPrivateMcpServer } = await catalog();
    const owner = asOwner(wsId, "u_owner", "Owner");
    const created = await createPrivateMcpServer(
      owner,
      { displayName: "Ours", authMode: "none" },
      serverDocument("com.example/ours", "4.0.0"),
    );
    expect(created.refusal).toBeNull();
    if (created.refusal !== null) {
      return;
    }
    const edited = await editPrivateMcpServer(
      owner,
      created.serverId,
      { displayName: "Ours", authMode: "none" },
      serverDocument("com.example/ours", "4.0.0", { description: "Same version, new words." }),
    );
    if (edited.refusal !== null) {
      throw new Error(edited.refusal.message);
    }
    const rows = await db.q<{ seq: number; upstream_version: string }>(
      `SELECT seq, upstream_version FROM web.mcp_server_revision WHERE server_id = $1 ORDER BY seq`,
      [created.serverId],
    );
    expect(rows.map((r) => Number(r.seq))).toEqual([1, 2]);
    expect(new Set(rows.map((r) => r.upstream_version))).toEqual(new Set(["4.0.0"]));
    // The pointer followed the edit: what the workspace receives is the newest save.
    expect(await currentRevisionOf(created.serverId)).toBe(edited.revisionId);
  });
});

describe("the pointer names a published revision of its own server, or nothing", () => {
  it("a revision that was not published leaves the pointer where it was", async () => {
    const serverId = await seedGlobalServer("mcps_ptr", "com.example/pointer");
    const candidate = await addRevision(serverId, {
      document: serverDocument("com.example/pointer", "1.0.0"),
      source: "registry",
      publish: false,
    });
    if (candidate.refusal !== null) {
      throw new Error(candidate.refusal.message);
    }
    expect(await currentRevisionOf(serverId)).toBeNull();
    expect(await revisionStatus(candidate.revisionId)).toBe("candidate");
  });

  it("accepting one publishes it, points at it, and takes the server off candidacy", async () => {
    const serverId = await seedGlobalServer("mcps_accept", "com.example/accept", {
      status: "candidate",
    });
    const added = await addRevision(serverId, {
      document: serverDocument("com.example/accept", "1.0.0"),
      source: "registry",
      publish: false,
    });
    if (added.refusal !== null) {
      throw new Error(added.refusal.message);
    }
    const { acceptMcpRevision } = await catalog();
    const accepted = await acceptMcpRevision({ display: "Staff" }, added.revisionId);
    expect(accepted.refusal).toBeNull();
    expect(await currentRevisionOf(serverId)).toBe(added.revisionId);
    expect(await revisionStatus(added.revisionId)).toBe("published");
    const rows = await db.q<{ status: string }>(`SELECT status FROM web.mcp_server WHERE id = $1`, [
      serverId,
    ]);
    expect(rows[0]?.status).toBe("active");
  });

  it("rejecting one records the decision and moves nothing", async () => {
    const serverId = await seedGlobalServer("mcps_reject", "com.example/reject");
    const added = await addRevision(serverId, {
      document: serverDocument("com.example/reject", "1.0.0"),
      source: "registry",
      publish: false,
    });
    if (added.refusal !== null) {
      throw new Error(added.refusal.message);
    }
    const { rejectMcpRevision } = await catalog();
    const rejected = await rejectMcpRevision({ display: "Staff" }, added.revisionId, "not ours");
    expect(rejected.refusal).toBeNull();
    expect(await revisionStatus(added.revisionId)).toBe("rejected");
    expect(await currentRevisionOf(serverId)).toBeNull();
    const rows = await db.q<{ decided_by: string | null }>(
      `SELECT decided_by FROM web.mcp_server_revision WHERE id = $1`,
      [added.revisionId],
    );
    expect(rows[0]?.decided_by).toBe("Staff");
  });

  it("a pointer at another server's revision is refused by the composite key", async () => {
    const strangerId = await seedGlobalServer("mcps_stranger", "com.example/stranger");
    const added = await addRevision(strangerId, {
      document: serverDocument("com.example/stranger", "1.0.0"),
      source: "staff",
      publish: true,
    });
    if (added.refusal !== null) {
      throw new Error(added.refusal.message);
    }
    await expect(
      db.q(`UPDATE web.mcp_server SET current_revision_id = $1 WHERE id = 'mcps_ptr'`, [
        added.revisionId,
      ]),
    ).rejects.toThrow(/mcp_server_current_revision_fk/);
  });

  it("revoking the current one falls back to the newest still published, then to nothing", async () => {
    const serverId = await seedGlobalServer("mcps_revoke", "com.example/revoke");
    const first = await addRevision(serverId, {
      document: serverDocument("com.example/revoke", "1.0.0"),
      source: "staff",
      publish: true,
    });
    const second = await addRevision(serverId, {
      document: serverDocument("com.example/revoke", "2.0.0"),
      source: "staff",
      publish: true,
    });
    if (first.refusal !== null || second.refusal !== null) {
      throw new Error("the seed revisions did not land");
    }
    expect(await currentRevisionOf(serverId)).toBe(second.revisionId);
    const { revokeMcpRevision } = await catalog();
    expect((await revokeMcpRevision({ display: "Staff" }, second.revisionId)).refusal).toBeNull();
    expect(await currentRevisionOf(serverId)).toBe(first.revisionId);
    expect((await revokeMcpRevision({ display: "Staff" }, first.revisionId)).refusal).toBeNull();
    expect(await currentRevisionOf(serverId)).toBeNull();
    // A revocation keeps the fact that it was once published — that is what it pulls back from.
    const rows = await db.q<{ published_at: string | null; revoked_at: string | null }>(
      `SELECT published_at, revoked_at FROM web.mcp_server_revision WHERE id = $1`,
      [first.revisionId],
    );
    expect(rows[0]?.published_at).not.toBeNull();
    expect(rows[0]?.revoked_at).not.toBeNull();
  });

  it("no pointer in the database names anything but a published revision", async () => {
    const rows = await db.q(
      `SELECT s.id FROM web.mcp_server s
       JOIN web.mcp_server_revision r ON r.id = s.current_revision_id
       WHERE r.status <> 'published'`,
    );
    expect(rows).toEqual([]);
  });
});

describe("the schema a document declares", () => {
  it("an unknown one is refused rather than parsed hopefully", async () => {
    const serverId = await seedGlobalServer("mcps_schema", "com.example/schema");
    const added = await addRevision(serverId, {
      document: serverDocument("com.example/schema", "1.0.0", {
        $schema: "https://static.modelcontextprotocol.io/schemas/2099-01-01/server.schema.json",
      }),
      source: "registry",
      publish: false,
    });
    expect(added.refusal?.code).toBe("MCP_SCHEMA_UNKNOWN");
  });

  it("the one this build knows is recorded on the row; declaring none is not a refusal", async () => {
    const { KNOWN_MCP_SCHEMA_VERSIONS } = await catalog();
    const known = KNOWN_MCP_SCHEMA_VERSIONS[0] ?? "";
    const serverId = await seedGlobalServer("mcps_schema_ok", "com.example/schema-ok");
    const declared = await addRevision(serverId, {
      document: serverDocument("com.example/schema-ok", "1.0.0", { $schema: known }),
      source: "registry",
      publish: false,
    });
    const silent = await addRevision(serverId, {
      document: serverDocument("com.example/schema-ok", "2.0.0"),
      source: "registry",
      publish: false,
    });
    expect(declared.refusal).toBeNull();
    expect(silent.refusal).toBeNull();
    const rows = await db.q<{ schema_version: string | null }>(
      `SELECT schema_version FROM web.mcp_server_revision WHERE server_id = $1 ORDER BY seq`,
      [serverId],
    );
    expect(rows.map((r) => r.schema_version)).toEqual([known, null]);
  });
});

describe("what the catalog refuses to publish", () => {
  it("a server whose sign-in nobody established", async () => {
    const serverId = await seedGlobalServer("mcps_unstated", "com.example/unstated", {
      authMode: null,
    });
    const added = await addRevision(serverId, {
      document: serverDocument("com.example/unstated", "1.0.0"),
      source: "registry",
      publish: false,
    });
    if (added.refusal !== null) {
      throw new Error(added.refusal.message);
    }
    const { acceptMcpRevision } = await catalog();
    const accepted = await acceptMcpRevision({ display: "Staff" }, added.revisionId);
    expect(accepted.refusal?.code).toBe("MCP_AUTH_MODE_REQUIRED");
    expect(await currentRevisionOf(serverId)).toBeNull();
  });

  it("a chore with no instructions — `manual` and no note", async () => {
    const serverId = await seedGlobalServer("mcps_chore", "com.example/chore", {
      authMode: "manual",
    });
    const added = await addRevision(serverId, {
      document: serverDocument("com.example/chore", "1.0.0"),
      source: "registry",
      publish: false,
    });
    if (added.refusal !== null) {
      throw new Error(added.refusal.message);
    }
    const { acceptMcpRevision } = await catalog();
    expect((await acceptMcpRevision({ display: "Staff" }, added.revisionId)).refusal?.code).toBe(
      "MCP_AUTH_NOTE_REQUIRED",
    );
    await db.q(`UPDATE web.mcp_server SET auth_note = 'Mint a token first.' WHERE id = $1`, [
      serverId,
    ]);
    expect((await acceptMcpRevision({ display: "Staff" }, added.revisionId)).refusal).toBeNull();
  });

  it("a workspace's private server — staff decide the catalog, not somebody's own rows", async () => {
    const { createPrivateMcpServer, revokeMcpRevision } = await catalog();
    const created = await createPrivateMcpServer(
      asOwner(wsId, "u_owner", "Owner"),
      { displayName: "Private", authMode: "none" },
      serverDocument("com.example/private-decide", "1.0.0"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    const answered = await revokeMcpRevision({ display: "Staff" }, created.revisionId);
    expect(answered.refusal?.code).toBe("MCP_REVISION_NOT_FOUND");
  });
});

describe("connecting a server to a workspace", () => {
  it("mints the bundle through the ordinary registration and records the connection", async () => {
    const { connectMcpServer } = await catalog();
    const serverId = await seedGlobalServer("mcps_connect", "com.example/connect");
    const added = await addRevision(serverId, {
      document: serverDocument("com.example/connect", "1.0.0"),
      source: "staff",
      publish: true,
    });
    if (added.refusal !== null) {
      throw new Error(added.refusal.message);
    }
    const connected = await connectMcpServer(asMember(wsId, "u_mem", "member", "Member"), {
      serverId,
      displayName: "Connect Me",
      to: null,
    });
    expect(connected.refusal).toBeNull();
    if (connected.refusal !== null) {
      return;
    }
    expect(connected.registration.name).toBe("connect-me");
    // The default channel takes it silently, the way every other genesis publish lands there —
    // a placement is only REPORTED when a channel was named or a curated default withheld it.
    const placed = await db.q(
      `SELECT 1 FROM web.channel_bundle cb JOIN web.channel c ON c.id = cb.channel_id
       WHERE cb.bundle_id = $1 AND c.is_default`,
      [connected.registration.bundleId],
    );
    expect(placed).toHaveLength(1);
    const rows = await db.q<{ kind: string; server_id: string; pinned_revision_id: string | null }>(
      `SELECT b.kind, m.server_id, m.pinned_revision_id
       FROM web.bundle b JOIN web.bundle_mcp m ON m.bundle_id = b.id
       WHERE b.id = $1`,
      [connected.registration.bundleId],
    );
    expect(rows[0]?.kind).toBe("mcp");
    expect(rows[0]?.server_id).toBe(serverId);
    expect(rows[0]?.pinned_revision_id).toBeNull();
  });

  it("a second connection to the same server refuses and leaves no bundle behind", async () => {
    const { connectMcpServer } = await catalog();
    const before = await db.q<{ n: string }>(
      `SELECT count(*) AS n FROM web.bundle WHERE workspace_id = $1`,
      [wsId],
    );
    const again = await connectMcpServer(asMember(wsId, "u_mem", "member", "Member"), {
      serverId: "mcps_connect",
      displayName: "Connect Me Again",
      to: null,
    });
    expect(again.refusal?.code).toBe("MCP_ALREADY_CONNECTED");
    const after = await db.q<{ n: string }>(
      `SELECT count(*) AS n FROM web.bundle WHERE workspace_id = $1`,
      [wsId],
    );
    expect(after[0]?.n).toBe(before[0]?.n);
  });

  it("another workspace's private server answers exactly like one that does not exist", async () => {
    const { connectMcpServer, createPrivateMcpServer } = await catalog();
    const theirs = await createPrivateMcpServer(
      asOwner(otherWsId, "u_owner", "Owner"),
      { displayName: "Theirs", authMode: "none" },
      serverDocument("com.example/theirs", "1.0.0"),
    );
    if (theirs.refusal !== null) {
      throw new Error(theirs.refusal.message);
    }
    const reached = await connectMcpServer(asMember(wsId, "u_mem", "member", "Member"), {
      serverId: theirs.serverId,
      displayName: "Theirs",
      to: null,
    });
    const invented = await connectMcpServer(asMember(wsId, "u_mem", "member", "Member"), {
      serverId: "mcps_no_such_row",
      displayName: "Nothing",
      to: null,
    });
    expect(reached.refusal).toEqual(invented.refusal);
    expect(reached.refusal?.code).toBe("MCP_SERVER_NOT_FOUND");
  });

  it("a candidate is not on offer, and neither is a server with nothing published", async () => {
    const { connectMcpServer } = await catalog();
    await seedGlobalServer("mcps_cand", "com.example/candidate", { status: "candidate" });
    await seedGlobalServer("mcps_empty", "com.example/empty");
    const candidate = await connectMcpServer(asMember(wsId, "u_mem", "member", "Member"), {
      serverId: "mcps_cand",
      displayName: "Candidate",
      to: null,
    });
    const empty = await connectMcpServer(asMember(wsId, "u_mem", "member", "Member"), {
      serverId: "mcps_empty",
      displayName: "Empty",
      to: null,
    });
    expect(candidate.refusal?.code).toBe("MCP_SERVER_NOT_FOUND");
    expect(empty.refusal?.code).toBe("MCP_NOTHING_PUBLISHED");
  });

  it("a pin names a published revision of that same server, or the connection refuses", async () => {
    const { connectMcpServer } = await catalog();
    const serverId = await seedGlobalServer("mcps_pinnable", "com.example/pinnable");
    const published = await addRevision(serverId, {
      document: serverDocument("com.example/pinnable", "1.0.0"),
      source: "staff",
      publish: true,
    });
    const candidate = await addRevision(serverId, {
      document: serverDocument("com.example/pinnable", "2.0.0"),
      source: "registry",
      publish: false,
    });
    if (published.refusal !== null || candidate.refusal !== null) {
      throw new Error("the seed revisions did not land");
    }
    const unpublished = await connectMcpServer(asMember(wsId, "u_mem", "member", "Member"), {
      serverId,
      displayName: "Pinned To A Candidate",
      to: null,
      pinnedRevisionId: candidate.revisionId,
    });
    expect(unpublished.refusal?.code).toBe("MCP_REVISION_NOT_FOUND");
    const pinned = await connectMcpServer(asMember(wsId, "u_mem", "member", "Member"), {
      serverId,
      displayName: "Pinned",
      to: null,
      pinnedRevisionId: published.revisionId,
    });
    expect(pinned.refusal).toBeNull();
  });

  it("a pin from another server's history is refused by the composite key", async () => {
    const rows = await db.q<{ bundle_id: string }>(
      `SELECT bundle_id FROM web.bundle_mcp WHERE server_id = 'mcps_connect' LIMIT 1`,
    );
    const foreign = await db.q<{ id: string }>(
      `SELECT id FROM web.mcp_server_revision WHERE server_id = 'mcps_pinnable' LIMIT 1`,
    );
    await expect(
      db.q(`UPDATE web.bundle_mcp SET pinned_revision_id = $1 WHERE bundle_id = $2`, [
        foreign[0]?.id,
        rows[0]?.bundle_id,
      ]),
    ).rejects.toThrow(/bundle_mcp_pinned_revision_fk/);
  });
});

/**
 * WHAT A MACHINE RECEIVES — driven through the read a real caller uses (the workspace catalog's
 * server half), because the resolution is one expression shared by every lane and asserting it
 * anywhere else would be asserting a copy of it.
 */
describe("what a machine receives", () => {
  /** The resolved row for one connected bundle, as the lanes read it. */
  async function receivedFor(workspaceId: string, bundleId: string) {
    const { laneMcpServersIndex } = await import("@/lib/db/queries.lane.server");
    const rows = await laneMcpServersIndex(asSession(workspaceId, "u_mem", "cs_mem", "member"));
    return rows.find((row) => row.skill_id === bundleId);
  }

  it("follows the catalog's current, and moves with it", async () => {
    const { connectMcpServer } = await catalog();
    const serverId = await seedGlobalServer("mcps_deliver", "com.example/deliver");
    const first = await addRevision(serverId, {
      document: serverDocument("com.example/deliver", "1.0.0"),
      source: "staff",
      publish: true,
    });
    if (first.refusal !== null) {
      throw new Error(first.refusal.message);
    }
    const member = asMember(wsId, "u_mem", "member", "Member");
    const connected = await connectMcpServer(member, {
      serverId,
      displayName: "Deliver",
      to: null,
    });
    if (connected.refusal !== null) {
      throw new Error(connected.refusal.message);
    }
    const bundleId = connected.registration.bundleId;
    const before = await receivedFor(wsId, bundleId);
    expect(before?.revision_id).toBe(first.revisionId);
    expect(before?.pinned).toBeUndefined();
    expect(before?.document.version).toBe("1.0.0");

    const second = await addRevision(serverId, {
      document: serverDocument("com.example/deliver", "2.0.0"),
      source: "staff",
      publish: true,
    });
    if (second.refusal !== null) {
      throw new Error(second.refusal.message);
    }
    const after = await receivedFor(wsId, bundleId);
    expect(after?.revision_id).toBe(second.revisionId);
    expect(after?.document.version).toBe("2.0.0");
  });

  it("a pin is kept — a revocation is reported, never quietly stepped off", async () => {
    const { connectMcpServer, revokeMcpRevision } = await catalog();
    const serverId = await seedGlobalServer("mcps_pinstay", "com.example/pinstay");
    const pinnedRevision = await addRevision(serverId, {
      document: serverDocument("com.example/pinstay", "1.0.0"),
      source: "staff",
      publish: true,
    });
    const newer = await addRevision(serverId, {
      document: serverDocument("com.example/pinstay", "2.0.0"),
      source: "staff",
      publish: true,
    });
    if (pinnedRevision.refusal !== null || newer.refusal !== null) {
      throw new Error("the seed revisions did not land");
    }
    const member = asMember(wsId, "u_mem", "member", "Member");
    const connected = await connectMcpServer(member, {
      serverId,
      displayName: "Pin Stay",
      to: null,
      pinnedRevisionId: pinnedRevision.revisionId,
    });
    if (connected.refusal !== null) {
      throw new Error(connected.refusal.message);
    }
    const held = await receivedFor(wsId, connected.registration.bundleId);
    expect(held?.revision_id).toBe(pinnedRevision.revisionId);
    expect(held?.pinned).toBe(true);
    expect(held?.revoked).toBeUndefined();

    expect(
      (await revokeMcpRevision({ display: "Staff" }, pinnedRevision.revisionId)).refusal,
    ).toBeNull();
    const pulled = await receivedFor(wsId, connected.registration.bundleId);
    expect(pulled?.revision_id).toBe(pinnedRevision.revisionId);
    expect(pulled?.revoked).toBe(true);
  });

  it("a workspace's own server delivers its own document", async () => {
    const { connectMcpServer, createPrivateMcpServer } = await catalog();
    const created = await createPrivateMcpServer(
      asOwner(wsId, "u_owner", "Owner"),
      { displayName: "Internal", authMode: "manual", authNote: "Ask ops for a token." },
      serverDocument("com.example/internal-delivery", "9.9.9"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    const member = asMember(wsId, "u_mem", "member", "Member");
    const connected = await connectMcpServer(member, {
      serverId: created.serverId,
      displayName: "Internal",
      to: null,
    });
    if (connected.refusal !== null) {
      throw new Error(connected.refusal.message);
    }
    const delivered = await receivedFor(wsId, connected.registration.bundleId);
    expect(delivered?.document.version).toBe("9.9.9");
    expect(delivered?.document.name).toBe("com.example/internal-delivery");
  });

  it("a bundle another workspace connected is not this workspace's to resolve", async () => {
    const rows = await db.q<{ bundle_id: string }>(
      `SELECT bundle_id FROM web.bundle_mcp WHERE workspace_id = $1 LIMIT 1`,
      [wsId],
    );
    expect(await receivedFor(otherWsId, rows[0]?.bundle_id ?? "")).toBeUndefined();
  });
});

describe("two writers at once", () => {
  it("serialize on the server row: revision 1 and revision 2, never one lost", async () => {
    const serverId = await seedGlobalServer("mcps_race", "com.example/race");
    const [a, b] = await Promise.all([
      addRevision(serverId, {
        document: serverDocument("com.example/race", "1.0.0"),
        source: "staff",
        publish: true,
      }),
      addRevision(serverId, {
        document: serverDocument("com.example/race", "2.0.0"),
        source: "staff",
        publish: true,
      }),
    ]);
    if (a.refusal !== null || b.refusal !== null) {
      throw new Error("a concurrent write was refused");
    }
    const seqs = await db.q<{ seq: number }>(
      `SELECT seq FROM web.mcp_server_revision WHERE server_id = $1 ORDER BY seq`,
      [serverId],
    );
    expect(seqs.map((r) => Number(r.seq))).toEqual([1, 2]);
    // Whichever committed last owns the pointer, and it is one of the two — not a third state.
    expect([a.revisionId, b.revisionId]).toContain(await currentRevisionOf(serverId));
  });

  it("and the same race for one upstream version leaves exactly one recorded", async () => {
    const serverId = await seedGlobalServer("mcps_race_version", "com.example/race-version");
    const document = serverDocument("com.example/race-version", "1.0.0");
    const [a, b] = await Promise.all([
      addRevision(serverId, { document, source: "registry", publish: false }),
      addRevision(serverId, { document, source: "registry", publish: false }),
    ]);
    const landed = [a, b].filter((outcome) => outcome.refusal === null);
    expect(landed).toHaveLength(1);
    const rows = await db.q(
      `SELECT id FROM web.mcp_server_revision WHERE server_id = $1 AND source = 'registry'`,
      [serverId],
    );
    expect(rows).toHaveLength(1);
  });
});

describe("what the plane saw when it asked", () => {
  it("is written onto the revision, and replaces the older answer", async () => {
    const { recordMcpRevisionProbe } = await catalog();
    const serverId = await seedGlobalServer("mcps_probe", "com.example/probe");
    const added = await addRevision(serverId, {
      document: serverDocument("com.example/probe", "1.0.0"),
      source: "staff",
      publish: true,
    });
    if (added.refusal !== null) {
      throw new Error(added.refusal.message);
    }
    await recordMcpRevisionProbe(added.revisionId, {
      outcome: "not_responding",
      protocolVersions: ["2025-06-18"],
    });
    await recordMcpRevisionProbe(added.revisionId, {
      outcome: "sign_in_required",
      protocolVersions: ["2025-06-18"],
      verification: { authorizationServer: "https://auth.example.com" },
    });
    const rows = await db.q<{
      probe_outcome: string;
      probed_at: string | null;
      verification: Record<string, unknown> | null;
    }>(`SELECT probe_outcome, probed_at, verification FROM web.mcp_server_revision WHERE id = $1`, [
      added.revisionId,
    ]);
    expect(rows[0]?.probe_outcome).toBe("sign_in_required");
    expect(rows[0]?.probed_at).not.toBeNull();
    expect(rows[0]?.verification).toEqual({ authorizationServer: "https://auth.example.com" });
  });

  it("a word outside the probe's vocabulary is refused by the database", async () => {
    await expect(
      db.q(
        `UPDATE web.mcp_server_revision SET probe_outcome = 'maybe', probed_at = now()
         WHERE server_id = 'mcps_probe'`,
      ),
    ).rejects.toThrow(/mcp_server_revision_probe_outcome_check/);
  });
});

describe("the list a workspace may connect from", () => {
  it("is the catalog's active servers plus this workspace's own, and nobody else's", async () => {
    const { connectableMcpServers } = await catalog();
    const rows = await connectableMcpServers(asMember(wsId, "u_mem", "member", "Member"));
    const names = rows.map((row) => row.registryName);
    expect(names).toContain("com.github/mcp");
    expect(names).toContain("com.example/internal-delivery");
    expect(names).not.toContain("com.example/theirs");
    // A candidate is not on offer here either — one answer about what exists, in both places.
    expect(names).not.toContain("com.example/candidate");
  });
});
