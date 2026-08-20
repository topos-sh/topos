import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { validateServerJson } from "@/lib/mcp/validate.server";
import {
  asMember,
  asOwner,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * THE MCP CATALOG, against a REAL scratch Postgres — the rows, the keys that refuse, and the two
 * writes every act above them is built from (append a revision, promote one to current).
 *
 * The scratch database carries the migrations whole, so the seeded catalog is here too — the 50
 * public servers migration 0018 stood up, reshaped by 0026. The catalog SYNC (the file → rows
 * reconcile) has its own suite; this one holds the rows, the partial uniques, the connection, the
 * server face, and the staff promote/dismiss decisions.
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

/** The same, with NO `version` — the honest shape of a self-maintained server. */
function versionlessDocument(
  name: string,
  extra: Record<string, unknown> = {},
): Record<string, unknown> {
  return {
    name,
    description: "A server for the suite.",
    remotes: [{ type: "streamable-http", url: "https://mcp.example.com/mcp" }],
    ...extra,
  };
}

/** A public catalog row with no revisions yet. */
async function seedPublicServer(
  id: string,
  name: string,
  extra: { status?: string; authMode?: string | null; authNote?: string | null } = {},
): Promise<string> {
  await db.q(
    `INSERT INTO web.mcp_server (id, name, display_name, auth_mode, auth_note, status)
     VALUES ($1, $2, $3, $4, $5, $6)`,
    [
      id,
      name,
      name,
      extra.authMode === undefined ? "none" : extra.authMode,
      extra.authNote ?? null,
      extra.status ?? "active",
    ],
  );
  return id;
}

/** Append a revision, non-current — the sole write on a proposal. */
async function addRevision(serverId: string, document: Record<string, unknown>) {
  const { addMcpRevisionInTx } = await catalog();
  const { getDb } = await import("@/lib/db/index.server");
  return await getDb().transaction((tx) => addMcpRevisionInTx(tx, serverId, { document }));
}

/** Append a revision AND promote it to current — a seed helper. */
async function addAndPromote(
  serverId: string,
  document: Record<string, unknown>,
  by = "Staff",
): Promise<string> {
  const { addMcpRevisionInTx, lockServerInTx, promoteMcpRevisionInTx } = await catalog();
  const { getDb } = await import("@/lib/db/index.server");
  return await getDb().transaction(async (tx) => {
    const added = await addMcpRevisionInTx(tx, serverId, { document });
    if (added.refusal !== null) {
      throw new Error(added.refusal.message);
    }
    const server = await lockServerInTx(tx, serverId);
    if (server === undefined) {
      throw new Error("server vanished");
    }
    const refusal = await promoteMcpRevisionInTx(tx, server, added.revisionId, by);
    if (refusal !== null) {
      throw new Error(refusal.message);
    }
    return added.revisionId;
  });
}

async function currentRevisionOf(serverId: string): Promise<string | null> {
  const rows = await db.q<{ current_revision_id: string | null }>(
    `SELECT current_revision_id FROM web.mcp_server WHERE id = $1`,
    [serverId],
  );
  return rows[0]?.current_revision_id ?? null;
}

async function manuallyCuratedOf(serverId: string): Promise<boolean> {
  const rows = await db.q<{ manually_curated: boolean }>(
    `SELECT manually_curated FROM web.mcp_server WHERE id = $1`,
    [serverId],
  );
  return rows[0]?.manually_curated === true;
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_catalog");
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seatUser(db, wsId, "u_mem", "member");
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
  it("every seeded server stands active, public, and untouched by staff, with a current revision", async () => {
    const rows = await db.q<{ total: number; with_current: number; curated: number }>(
      `SELECT count(*) AS total,
              count(*) FILTER (WHERE current_revision_id IS NOT NULL) AS with_current,
              count(*) FILTER (WHERE manually_curated) AS curated
       FROM web.mcp_server WHERE workspace_id IS NULL AND status = 'active'`,
    );
    expect(Number(rows[0]?.total)).toBe(50);
    expect(Number(rows[0]?.with_current)).toBe(50);
    expect(Number(rows[0]?.curated)).toBe(0);
  });

  it("every seeded document passes the gate a pasted one answers to, and names its own row", async () => {
    const rows = await db.q<{ name: string; document: Record<string, unknown> }>(
      `SELECT s.name, r.document
       FROM web.mcp_server s JOIN web.mcp_server_revision r ON r.id = s.current_revision_id
       WHERE s.workspace_id IS NULL`,
    );
    const failures: string[] = [];
    for (const row of rows) {
      const validated = validateServerJson(`${JSON.stringify(row.document)}\n`, {
        requireVersion: false,
      });
      if (!validated.ok) {
        failures.push(`${row.name}: ${validated.code} ${validated.message}`);
      } else if (validated.summary.name !== row.name) {
        failures.push(`${row.name}: document says ${validated.summary.name}`);
      }
    }
    expect(failures).toEqual([]);
  });

  it("stamps no fabricated version: every seeded revision names none", async () => {
    const rows = await db.q<{ versioned: number }>(
      `SELECT count(*) FILTER (WHERE upstream_version IS NOT NULL) AS versioned
       FROM web.mcp_server_revision r JOIN web.mcp_server s ON s.id = r.server_id
       WHERE s.workspace_id IS NULL`,
    );
    expect(Number(rows[0]?.versioned)).toBe(0);
  });

  it("carries the editorial half: a manual row says what the person has to do", async () => {
    const rows = await db.q<{ auth_mode: string; auth_note: string | null; icon: string | null }>(
      `SELECT auth_mode, auth_note, icon FROM web.mcp_server WHERE name = 'com.github/mcp'`,
    );
    expect(rows[0]?.auth_mode).toBe("manual");
    expect((rows[0]?.auth_note ?? "").length).toBeGreaterThan(0);
  });

  it("no seeded row is left without a stated sign-in tier", async () => {
    const rows = await db.q(
      `SELECT name FROM web.mcp_server WHERE workspace_id IS NULL AND auth_mode IS NULL`,
    );
    expect(rows).toEqual([]);
  });
});

describe("the public namespace is one server's", () => {
  it("a second public row claiming a name the catalog holds is refused by the index", async () => {
    await expect(
      db.q(
        `INSERT INTO web.mcp_server (id, name, display_name, auth_mode)
         VALUES ('mcps_dup', 'com.github/mcp', 'Dup', 'none')`,
      ),
    ).rejects.toThrow(/mcp_server_public_name/);
  });

  it("private rows collide with nobody — not the catalog, not each other", async () => {
    await db.q(
      `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode)
       VALUES ('mcps_priv_a', $1, 'com.github/mcp', 'Ours', 'none')`,
      [wsId],
    );
    await db.q(
      `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode)
       VALUES ('mcps_priv_b', $1, 'com.github/mcp', 'Theirs', 'none')`,
      [otherWsId],
    );
    const rows = await db.q(
      `SELECT id FROM web.mcp_server WHERE name = 'com.github/mcp' ORDER BY id`,
    );
    // The public seed plus two private rows — three servers under one name, none shadowing another.
    expect(rows.length).toBe(3);
  });
});

describe("one document per version, versionless exempt", () => {
  it("appends versionless revisions freely, and refuses a repeated version", async () => {
    const serverId = await seedPublicServer("mcps_ver", "com.example/ver");
    const first = await addRevision(serverId, versionlessDocument("com.example/ver"));
    expect(first.refusal).toBeNull();
    const second = await addRevision(
      serverId,
      versionlessDocument("com.example/ver", { description: "b" }),
    );
    expect(second.refusal).toBeNull();
    const v1 = await addRevision(serverId, serverDocument("com.example/ver", "1.0.0"));
    expect(v1.refusal).toBeNull();
    const v1again = await addRevision(
      serverId,
      serverDocument("com.example/ver", "1.0.0", { description: "x" }),
    );
    expect(v1again.refusal?.code).toBe("MCP_VERSION_HELD");
  });

  it("and the index says the same thing to a writer that skipped the check", async () => {
    const serverId = await seedPublicServer("mcps_ver2", "com.example/ver2");
    const rid = await addAndPromote(serverId, serverDocument("com.example/ver2", "2.0.0"));
    await expect(
      db.q(
        `INSERT INTO web.mcp_server_revision (id, server_id, seq, upstream_version, document)
         VALUES ('mcpr_00000000000000000000000000000099', $1, 9, '2.0.0', '{}'::jsonb)`,
        [serverId],
      ),
    ).rejects.toThrow(/mcp_server_revision_upstream_version/);
    expect(await currentRevisionOf(serverId)).toBe(rid);
  });
});

describe("private servers: an owner's own, created and edited", () => {
  it("create promotes the first revision; edit adds a new one and moves the pointer", async () => {
    const { createPrivateMcpServer, editPrivateMcpServer } = await catalog();
    const owner = asOwner(wsId, "u_owner", "Owner");
    const created = await createPrivateMcpServer(
      owner,
      { displayName: "Ours", authMode: "none" },
      versionlessDocument("com.example/ours"),
    );
    expect(created.refusal).toBeNull();
    if (created.refusal !== null) {
      return;
    }
    expect(await currentRevisionOf(created.serverId)).toBe(created.revisionId);
    const edited = await editPrivateMcpServer(
      owner,
      created.serverId,
      { displayName: "Ours", authMode: "none" },
      versionlessDocument("com.example/ours", { description: "New words." }),
    );
    if (edited.refusal !== null) {
      throw new Error(edited.refusal.message);
    }
    const rows = await db.q<{ seq: number }>(
      `SELECT seq FROM web.mcp_server_revision WHERE server_id = $1 ORDER BY seq`,
      [created.serverId],
    );
    expect(rows.map((r) => Number(r.seq))).toEqual([1, 2]);
    expect(await currentRevisionOf(created.serverId)).toBe(edited.revisionId);
  });

  it("refuses a revision whose document renames the server", async () => {
    const { createPrivateMcpServer, editPrivateMcpServer } = await catalog();
    const owner = asOwner(wsId, "u_owner", "Owner");
    const created = await createPrivateMcpServer(
      owner,
      { displayName: "Named", authMode: "none" },
      versionlessDocument("com.example/named"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    const renamed = await editPrivateMcpServer(
      owner,
      created.serverId,
      { displayName: "Named", authMode: "none" },
      versionlessDocument("com.example/renamed"),
    );
    expect(renamed.refusal?.code).toBe("MCP_NAME_MISMATCH");
  });

  it("refuses a second private server under a name this workspace already holds", async () => {
    const { createPrivateMcpServer } = await catalog();
    const owner = asOwner(wsId, "u_owner", "Owner");
    await createPrivateMcpServer(
      owner,
      { displayName: "Once", authMode: "none" },
      versionlessDocument("com.example/once"),
    );
    const twice = await createPrivateMcpServer(
      owner,
      { displayName: "Twice", authMode: "none" },
      versionlessDocument("com.example/once"),
    );
    expect(twice.refusal?.code).toBe("MCP_NAME_TAKEN");
  });
});

describe("the connection", () => {
  it("connects a public active server, one per workspace", async () => {
    const { connectMcpServer } = await catalog();
    const member = asMember(wsId, "u_mem", "member", "Member");
    const serverId = await seedPublicServer("mcps_conn", "com.example/conn");
    await addAndPromote(serverId, versionlessDocument("com.example/conn"));
    const first = await connectMcpServer(member, {
      serverId,
      displayName: "conn",
      to: null,
    });
    expect(first.refusal).toBeNull();
    const again = await connectMcpServer(member, {
      serverId,
      displayName: "conn",
      to: null,
    });
    expect(again.refusal?.code).toBe("MCP_ALREADY_CONNECTED");
  });

  it("refuses a delisted server, and another workspace's private one, identically", async () => {
    const { connectMcpServer, createPrivateMcpServer } = await catalog();
    const member = asMember(wsId, "u_mem", "member", "Member");
    const delisted = await seedPublicServer("mcps_delisted", "com.example/delisted", {
      status: "delisted",
    });
    await addRevision(delisted, versionlessDocument("com.example/delisted"));
    const off = await connectMcpServer(member, { serverId: delisted, displayName: "d", to: null });
    expect(off.refusal?.code).toBe("MCP_SERVER_NOT_FOUND");

    const theirs = await createPrivateMcpServer(
      asOwner(otherWsId, "u_owner", "Owner"),
      { displayName: "Theirs", authMode: "none" },
      versionlessDocument("com.example/theirs"),
    );
    if (theirs.refusal !== null) {
      throw new Error(theirs.refusal.message);
    }
    const reached = await connectMcpServer(member, {
      serverId: theirs.serverId,
      displayName: "t",
      to: null,
    });
    expect(reached.refusal?.code).toBe("MCP_SERVER_NOT_FOUND");
  });

  it("pins to a specific revision, and the face reports the pin", async () => {
    const { connectMcpServer, mcpServerFace } = await catalog();
    const member = asMember(wsId, "u_mem", "member", "Member");
    const serverId = await seedPublicServer("mcps_pin", "com.example/pin");
    const pinned = await addAndPromote(serverId, versionlessDocument("com.example/pin"));
    // A newer current, so the pin is deliberately behind it.
    await addAndPromote(serverId, serverDocument("com.example/pin", "2.0.0"));
    const connected = await connectMcpServer(member, {
      serverId,
      displayName: "pinbundle",
      to: null,
      pinnedRevisionId: pinned,
    });
    if (connected.refusal !== null) {
      throw new Error(connected.refusal.message);
    }
    const face = await mcpServerFace(member, connected.registration.bundleId);
    expect(face?.pinnedRevisionId).toBe(pinned);
    expect(face?.resolved?.revisionId).toBe(pinned);
    expect(face?.resolved?.state).toBe("history");
  });

  it("refuses a pin to a proposal nobody promoted — a pin never bypasses curation", async () => {
    const { connectMcpServer } = await catalog();
    const member = asMember(wsId, "u_mem", "member", "Member");
    const serverId = await seedPublicServer("mcps_pinprop", "com.example/pinprop");
    await addAndPromote(serverId, versionlessDocument("com.example/pinprop"));
    // A non-current proposal (appended, never promoted) — no promotion stamp.
    const proposal = await addRevision(serverId, serverDocument("com.example/pinprop", "9.9.9"));
    if (proposal.refusal !== null) {
      throw new Error(proposal.refusal.message);
    }
    const refused = await connectMcpServer(member, {
      serverId,
      displayName: "pinprop",
      to: null,
      pinnedRevisionId: proposal.revisionId,
    });
    expect(refused.refusal?.code).toBe("MCP_REVISION_NOT_FOUND");
  });
});

describe("the server face reads the pointer, not a status column", () => {
  it("a private server shows its whole edit trail; the current reads as current", async () => {
    const { createPrivateMcpServer, editPrivateMcpServer, connectMcpServer, mcpServerFace } =
      await catalog();
    const owner = asOwner(wsId, "u_owner", "Owner");
    const created = await createPrivateMcpServer(
      owner,
      { displayName: "Trail", authMode: "none" },
      versionlessDocument("com.example/trail"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    await editPrivateMcpServer(
      owner,
      created.serverId,
      { displayName: "Trail", authMode: "none" },
      versionlessDocument("com.example/trail", { description: "v2" }),
    );
    const connected = await connectMcpServer(owner, {
      serverId: created.serverId,
      displayName: "trailbundle",
      to: null,
    });
    if (connected.refusal !== null) {
      throw new Error(connected.refusal.message);
    }
    const face = await mcpServerFace(owner, connected.registration.bundleId);
    expect(face?.isPrivate).toBe(true);
    expect(face?.revisions.length).toBe(2);
    const current = face?.revisions.find((r) => r.state === "current");
    expect(current?.revisionId).toBe(face?.currentRevisionId);
    expect(face?.revisions.filter((r) => r.state === "history").length).toBe(1);
  });

  it("a public server shows only what has been on offer — a proposal is not in its history", async () => {
    const { connectMcpServer, mcpServerFace } = await catalog();
    const member = asMember(wsId, "u_mem", "member", "Member");
    const serverId = await seedPublicServer("mcps_face_pub", "com.example/facepub");
    await addAndPromote(serverId, versionlessDocument("com.example/facepub"));
    // A non-current proposal (never promoted) — a public server's face must not surface it.
    await addRevision(serverId, serverDocument("com.example/facepub", "5.0.0"));
    const connected = await connectMcpServer(member, {
      serverId,
      displayName: "facepubbundle",
      to: null,
    });
    if (connected.refusal !== null) {
      throw new Error(connected.refusal.message);
    }
    const face = await mcpServerFace(member, connected.registration.bundleId);
    expect(face?.isPrivate).toBe(false);
    expect(face?.revisions.length).toBe(1);
    expect(face?.revisions[0]?.state).toBe("current");
  });
});

describe("staff promote and dismiss a proposal", () => {
  it("promoting a proposal advances current and makes the server manually curated", async () => {
    const { promoteMcpRevision } = await catalog();
    const serverId = await seedPublicServer("mcps_promote", "com.example/promote");
    const first = await addAndPromote(serverId, versionlessDocument("com.example/promote"));
    const proposal = await addRevision(
      serverId,
      serverDocument("com.example/promote", "3.0.0", { _meta: { "sh.topos/auth": "none" } }),
    );
    expect(proposal.refusal).toBeNull();
    if (proposal.refusal !== null) {
      return;
    }
    expect(await currentRevisionOf(serverId)).toBe(first);
    const promoted = await promoteMcpRevision({ display: "Staff" }, proposal.revisionId);
    expect(promoted.refusal).toBeNull();
    expect(await currentRevisionOf(serverId)).toBe(proposal.revisionId);
    expect(await manuallyCuratedOf(serverId)).toBe(true);
  });

  it("refuses to promote the version already on offer, or a dismissed one", async () => {
    const { promoteMcpRevision, dismissMcpRevision } = await catalog();
    const serverId = await seedPublicServer("mcps_promote2", "com.example/promote2");
    const current = await addAndPromote(serverId, versionlessDocument("com.example/promote2"));
    const already = await promoteMcpRevision({ display: "Staff" }, current);
    expect(already.refusal?.code).toBe("MCP_REVISION_CURRENT");
    const proposal = await addRevision(serverId, serverDocument("com.example/promote2", "4.0.0"));
    if (proposal.refusal !== null) {
      throw new Error(proposal.refusal.message);
    }
    expect(
      (await dismissMcpRevision({ display: "Staff" }, proposal.revisionId)).refusal,
    ).toBeNull();
    const dead = await promoteMcpRevision({ display: "Staff" }, proposal.revisionId);
    expect(dead.refusal?.code).toBe("MCP_REVISION_DISMISSED");
  });

  it("dismissing a proposal marks it, curates the server, and refuses to touch the current", async () => {
    const { dismissMcpRevision } = await catalog();
    const serverId = await seedPublicServer("mcps_dismiss", "com.example/dismiss");
    const current = await addAndPromote(serverId, versionlessDocument("com.example/dismiss"));
    const onOffer = await dismissMcpRevision({ display: "Staff" }, current);
    expect(onOffer.refusal?.code).toBe("MCP_REVISION_CURRENT");
    const proposal = await addRevision(serverId, serverDocument("com.example/dismiss", "6.0.0"));
    if (proposal.refusal !== null) {
      throw new Error(proposal.refusal.message);
    }
    const dismissed = await dismissMcpRevision({ display: "Staff" }, proposal.revisionId);
    expect(dismissed.refusal).toBeNull();
    const rows = await db.q<{ dismissed_at: string | null }>(
      `SELECT dismissed_at FROM web.mcp_server_revision WHERE id = $1`,
      [proposal.revisionId],
    );
    expect(rows[0]?.dismissed_at).not.toBeNull();
    expect(await manuallyCuratedOf(serverId)).toBe(true);
  });

  it("refuses to dismiss a revision that has become history", async () => {
    const { promoteMcpRevision, dismissMcpRevision } = await catalog();
    const serverId = await seedPublicServer("mcps_dismiss_hist", "com.example/dismiss-hist");
    await addAndPromote(serverId, versionlessDocument("com.example/dismiss-hist"));
    const older = await addRevision(serverId, serverDocument("com.example/dismiss-hist", "2.0.0"));
    const newer = await addRevision(
      serverId,
      serverDocument("com.example/dismiss-hist", "3.0.0", { _meta: { "sh.topos/auth": "none" } }),
    );
    if (older.refusal !== null || newer.refusal !== null) {
      throw new Error("seeding proposals failed");
    }
    // Promote the newest; the older proposal is now history, below the current.
    expect((await promoteMcpRevision({ display: "Staff" }, newer.revisionId)).refusal).toBeNull();
    const stale = await dismissMcpRevision({ display: "Staff" }, older.revisionId);
    expect(stale.refusal?.code).toBe("MCP_REVISION_NOT_PROPOSAL");
    const rows = await db.q<{ dismissed_at: string | null }>(
      `SELECT dismissed_at FROM web.mcp_server_revision WHERE id = $1`,
      [older.revisionId],
    );
    expect(rows[0]?.dismissed_at).toBeNull();
  });

  it("a private server's revision is not a staff concern — promote answers as not found", async () => {
    const { createPrivateMcpServer, promoteMcpRevision } = await catalog();
    const created = await createPrivateMcpServer(
      asOwner(wsId, "u_owner", "Owner"),
      { displayName: "Mine", authMode: "none" },
      versionlessDocument("com.example/mine"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    const denied = await promoteMcpRevision({ display: "Staff" }, created.revisionId);
    expect(denied.refusal?.code).toBe("MCP_REVISION_NOT_FOUND");
  });
});

describe("the schema a document declares", () => {
  it("an unknown one is refused rather than parsed hopefully", async () => {
    const serverId = await seedPublicServer("mcps_schema", "com.example/schema");
    const bad = await addRevision(
      serverId,
      versionlessDocument("com.example/schema", { $schema: "https://example.com/made-up.json" }),
    );
    expect(bad.refusal?.code).toBe("MCP_SCHEMA_UNKNOWN");
  });

  it("the one this build knows is recorded; declaring none is not a refusal", async () => {
    const serverId = await seedPublicServer("mcps_schema2", "com.example/schema2");
    const known = await addRevision(
      serverId,
      serverDocument("com.example/schema2", "1.0.0", {
        $schema: "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
      }),
    );
    expect(known.refusal).toBeNull();
    const none = await addRevision(serverId, versionlessDocument("com.example/schema2"));
    expect(none.refusal).toBeNull();
    const rows = await db.q<{ schema_version: string | null }>(
      `SELECT schema_version FROM web.mcp_server_revision WHERE server_id = $1 ORDER BY seq`,
      [serverId],
    );
    expect(rows[0]?.schema_version).toContain("2025-12-11");
    expect(rows[1]?.schema_version).toBeNull();
  });
});

describe("staff manage the public catalog: list, add, edit", () => {
  it("lists a public server with its current version and its newest pending proposal", async () => {
    const { listPublicMcpServers } = await catalog();
    const serverId = await seedPublicServer("mcps_list", "com.example/list");
    await addAndPromote(serverId, versionlessDocument("com.example/list"));
    // Two file versions ahead at once: the list surfaces the NEWEST as the one decision to make.
    await addRevision(serverId, serverDocument("com.example/list", "2.0.0"));
    const proposal = await addRevision(serverId, serverDocument("com.example/list", "2.1.0"));
    if (proposal.refusal !== null) {
      throw new Error(proposal.refusal.message);
    }
    const row = (await listPublicMcpServers()).find((s) => s.name === "com.example/list");
    expect(row).toBeDefined();
    expect(row?.currentVersion).toBeNull(); // the versionless current names none
    expect(row?.connections).toBe(0);
    expect(row?.proposal?.revisionId).toBe(proposal.revisionId);
    expect(row?.proposal?.upstreamVersion).toBe("2.1.0");
  });

  it("shows no proposal for a settled server", async () => {
    const { listPublicMcpServers } = await catalog();
    const serverId = await seedPublicServer("mcps_settled", "com.example/settled");
    await addAndPromote(serverId, versionlessDocument("com.example/settled"));
    const row = (await listPublicMcpServers()).find((s) => s.name === "com.example/settled");
    expect(row?.proposal).toBeNull();
  });

  it("add creates a public, manually-curated server and promotes its first revision", async () => {
    const { createPublicMcpServer } = await catalog();
    const created = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Weather", authMode: "none" },
      versionlessDocument("com.example/weather"),
    );
    expect(created.refusal).toBeNull();
    if (created.refusal !== null) {
      return;
    }
    expect(await currentRevisionOf(created.serverId)).toBe(created.revisionId);
    expect(await manuallyCuratedOf(created.serverId)).toBe(true);
    const rows = await db.q<{ workspace_id: string | null; status: string; auth_mode: string }>(
      `SELECT workspace_id, status, auth_mode FROM web.mcp_server WHERE id = $1`,
      [created.serverId],
    );
    expect(rows[0]?.workspace_id).toBeNull();
    expect(rows[0]?.status).toBe("active");
    expect(rows[0]?.auth_mode).toBe("none");
    // The verified tier is materialized into the DELIVERED document, so a machine reads it (the
    // client derives auth from this key, not the column).
    const doc = await db.q<{ document: { _meta?: Record<string, unknown> } }>(
      `SELECT document FROM web.mcp_server_revision WHERE id = $1`,
      [created.revisionId],
    );
    expect(doc[0]?.document._meta?.["sh.topos/auth"]).toBe("none");
  });

  it("add refuses a name a self-maintained (name-null) public row already serves", async () => {
    const { createPublicMcpServer } = await catalog();
    // A public row addressed by the name inside its document, carrying no `name` of its own.
    await db.q(
      `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
       VALUES ('mcps_selfmaint_x', NULL, NULL, 'Self Maintained', 'none', 'active')`,
    );
    await addAndPromote("mcps_selfmaint_x", versionlessDocument("com.example/self-maint-x"));
    const refused = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Shadow", authMode: "none" },
      versionlessDocument("com.example/self-maint-x"),
    );
    expect(refused.refusal?.code).toBe("MCP_NAME_TAKEN");
    // Nothing new was created under the name — the self-maintained row is still the only one.
    const rows = await db.q<{ n: number }>(
      `SELECT count(*)::int AS n FROM web.mcp_server ms
       JOIN web.mcp_server_revision cur ON cur.id = ms.current_revision_id
       WHERE ms.workspace_id IS NULL AND cur.document->>'name' = 'com.example/self-maint-x'`,
    );
    expect(Number(rows[0]?.n)).toBe(1);
  });

  it("stores public documents versionless, even from a versioned input", async () => {
    const { createPublicMcpServer } = await catalog();
    // A version passed in is editorial framing; the stored public document drops it (and the schema
    // claim that would require it), matching the file sync — so a tier can be corrected later.
    const created = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Versioned in", authMode: "none" },
      serverDocument("com.example/versionless-store", "9.9.9", {
        $schema: "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json",
      }),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    const rows = await db.q<{ upstream_version: string | null; document: Record<string, unknown> }>(
      `SELECT upstream_version, document FROM web.mcp_server_revision WHERE id = $1`,
      [created.revisionId],
    );
    expect(rows[0]?.upstream_version).toBeNull();
    expect(rows[0]?.document).not.toHaveProperty("version");
    expect(rows[0]?.document).not.toHaveProperty("$schema");
  });

  it("edit changing only the display name keeps the revision — no duplicate", async () => {
    const { createPublicMcpServer, editPublicMcpServer } = await catalog();
    const created = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Editable meta", authMode: "none" },
      versionlessDocument("com.example/editorial-only"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    const edited = await editPublicMcpServer(
      { display: "Ops" },
      created.serverId,
      { displayName: "Renamed", authMode: "none" },
      versionlessDocument("com.example/editorial-only"),
    );
    expect(edited.refusal).toBeNull();
    // Same revision, no second one appended — the delivered document did not change.
    expect(edited.refusal === null && edited.revisionId).toBe(created.revisionId);
    expect(await currentRevisionOf(created.serverId)).toBe(created.revisionId);
    const rows = await db.q<{ n: number }>(
      `SELECT count(*)::int AS n FROM web.mcp_server_revision WHERE server_id = $1`,
      [created.serverId],
    );
    expect(Number(rows[0]?.n)).toBe(1);
    const server = await db.q<{ display_name: string }>(
      `SELECT display_name FROM web.mcp_server WHERE id = $1`,
      [created.serverId],
    );
    expect(server[0]?.display_name).toBe("Renamed");
  });

  it("add refuses a public server with no established sign-in tier", async () => {
    const { createPublicMcpServer } = await catalog();
    const refused = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "No tier", authMode: null },
      versionlessDocument("com.example/no-tier"),
    );
    expect(refused.refusal?.code).toBe("MCP_AUTH_MODE_REQUIRED");
  });

  it("add refuses a `manual` server with no line saying what a person must do", async () => {
    const { createPublicMcpServer } = await catalog();
    const refused = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Manual", authMode: "manual", authNote: "  " },
      versionlessDocument("com.example/manual-add"),
    );
    expect(refused.refusal?.code).toBe("MCP_AUTH_NOTE_REQUIRED");
    const rows = await db.q(`SELECT id FROM web.mcp_server WHERE name = 'com.example/manual-add'`);
    expect(rows).toEqual([]); // the refusal left nothing behind
  });

  it("add refuses a name the public catalog already carries", async () => {
    const { createPublicMcpServer } = await catalog();
    const refused = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Dup", authMode: "none" },
      versionlessDocument("com.github/mcp"),
    );
    expect(refused.refusal?.code).toBe("MCP_NAME_TAKEN");
  });

  it("edit appends a new current revision and marks the server manually curated", async () => {
    const { createPublicMcpServer, editPublicMcpServer } = await catalog();
    const created = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Editable", authMode: "none" },
      versionlessDocument("com.example/editable"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    // The file never touched it, so reset the bit the add set — proving edit is what re-sets it.
    await db.q(`UPDATE web.mcp_server SET manually_curated = false WHERE id = $1`, [
      created.serverId,
    ]);
    const edited = await editPublicMcpServer(
      { display: "Ops" },
      created.serverId,
      { displayName: "Editable v2", authMode: "oauth" },
      versionlessDocument("com.example/editable", { description: "Now with oauth." }),
    );
    if (edited.refusal !== null) {
      throw new Error(edited.refusal.message);
    }
    expect(await currentRevisionOf(created.serverId)).toBe(edited.revisionId);
    expect(await manuallyCuratedOf(created.serverId)).toBe(true);
    const rows = await db.q<{ seq: number }>(
      `SELECT seq FROM web.mcp_server_revision WHERE server_id = $1 ORDER BY seq`,
      [created.serverId],
    );
    expect(rows.map((r) => Number(r.seq))).toEqual([1, 2]);
    const server = await db.q<{ display_name: string; auth_mode: string }>(
      `SELECT display_name, auth_mode FROM web.mcp_server WHERE id = $1`,
      [created.serverId],
    );
    expect(server[0]?.display_name).toBe("Editable v2");
    expect(server[0]?.auth_mode).toBe("oauth");
  });

  it("edit refuses a revision that renames the server", async () => {
    const { createPublicMcpServer, editPublicMcpServer } = await catalog();
    const created = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Fixed name", authMode: "none" },
      versionlessDocument("com.example/fixed-name"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    const renamed = await editPublicMcpServer(
      { display: "Ops" },
      created.serverId,
      { displayName: "Fixed name", authMode: "none" },
      versionlessDocument("com.example/other-name"),
    );
    expect(renamed.refusal?.code).toBe("MCP_NAME_MISMATCH");
  });

  it("edit answers `no such server` for a workspace's private one", async () => {
    const { createPrivateMcpServer, editPublicMcpServer } = await catalog();
    const mine = await createPrivateMcpServer(
      asOwner(wsId, "u_owner", "Owner"),
      { displayName: "Private", authMode: "none" },
      versionlessDocument("com.example/private-edit"),
    );
    if (mine.refusal !== null) {
      throw new Error(mine.refusal.message);
    }
    const denied = await editPublicMcpServer(
      { display: "Ops" },
      mine.serverId,
      { displayName: "Hijack", authMode: "none" },
      versionlessDocument("com.example/private-edit"),
    );
    expect(denied.refusal?.code).toBe("MCP_SERVER_NOT_FOUND");
  });

  it("edit refuses renaming a self-maintained (name-null) public server", async () => {
    const { editPublicMcpServer } = await catalog();
    await db.q(
      `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
       VALUES ('mcps_selfmaint_edit', NULL, NULL, 'Self Maint Edit', 'none', 'active')`,
    );
    await addAndPromote("mcps_selfmaint_edit", versionlessDocument("com.example/self-maint-edit"));
    // The identity is the name inside its current document; an edit may not change it, even though
    // the row carries no `name` for `addMcpRevisionInTx` to check.
    const renamed = await editPublicMcpServer(
      { display: "Ops" },
      "mcps_selfmaint_edit",
      { displayName: "Renamed", authMode: "none" },
      versionlessDocument("com.example/self-maint-renamed"),
    );
    expect(renamed.refusal?.code).toBe("MCP_NAME_MISMATCH");
  });

  it("edit refuses activating a name-null row under a name another public row holds", async () => {
    const { editPublicMcpServer } = await catalog();
    // A public row with no `name` AND no current revision yet — the edit would give it its identity.
    await db.q(
      `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
       VALUES ('mcps_adopt', NULL, NULL, 'To Adopt', 'none', 'active')`,
    );
    // A name the seeded catalog already carries.
    const taken = await editPublicMcpServer(
      { display: "Ops" },
      "mcps_adopt",
      { displayName: "Adopted", authMode: "none" },
      versionlessDocument("com.github/mcp"),
    );
    expect(taken.refusal?.code).toBe("MCP_NAME_TAKEN");
    // A free name activates it.
    const fresh = await editPublicMcpServer(
      { display: "Ops" },
      "mcps_adopt",
      { displayName: "Adopted", authMode: "none" },
      versionlessDocument("com.example/adopted-fresh"),
    );
    expect(fresh.refusal).toBeNull();
    expect(await currentRevisionOf("mcps_adopt")).not.toBeNull();
    // The embedded name is CLAIMED into the column, so the file sync (which searches by `name`)
    // finds this row rather than inserting a duplicate under the same name later.
    const named = await db.q<{ name: string | null }>(
      `SELECT name FROM web.mcp_server WHERE id = 'mcps_adopt'`,
    );
    expect(named[0]?.name).toBe("com.example/adopted-fresh");
  });

  it("edit refuses a save built on a stale row stamp, but accepts the real one", async () => {
    const { createPublicMcpServer, editPublicMcpServer, listPublicMcpServers } = await catalog();
    const created = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Stale", authMode: "none" },
      versionlessDocument("com.example/stale"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    const stamp = (await listPublicMcpServers()).find((s) => s.serverId === created.serverId)
      ?.updatedAt as number;
    // A save built on an OLDER stamp than the row carries — another operator wrote it since.
    const stale = await editPublicMcpServer(
      { display: "Ops" },
      created.serverId,
      { displayName: "Stale v2", authMode: "none" },
      versionlessDocument("com.example/stale", { description: "changed" }),
      stamp - 1000,
    );
    expect(stale.refusal?.code).toBe("MCP_REVISION_STALE");
    // The real stamp succeeds.
    const fresh = await editPublicMcpServer(
      { display: "Ops" },
      created.serverId,
      { displayName: "Stale v2", authMode: "none" },
      versionlessDocument("com.example/stale", { description: "changed" }),
      stamp,
    );
    expect(fresh.refusal).toBeNull();
  });

  it("edit refuses clearing a manual server's note, even on an editorial-only change", async () => {
    const { createPublicMcpServer, editPublicMcpServer } = await catalog();
    const created = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Manual srv", authMode: "manual", authNote: "Mint a token first." },
      versionlessDocument("com.example/manual-edit"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    // Same document + tier, but blank the note — the editorial-only fast path must still refuse.
    const refused = await editPublicMcpServer(
      { display: "Ops" },
      created.serverId,
      { displayName: "Manual srv", authMode: "manual", authNote: "  " },
      versionlessDocument("com.example/manual-edit"),
    );
    expect(refused.refusal?.code).toBe("MCP_AUTH_NOTE_REQUIRED");
    const rows = await db.q<{ auth_note: string | null }>(
      `SELECT auth_note FROM web.mcp_server WHERE id = $1`,
      [created.serverId],
    );
    expect(rows[0]?.auth_note).toBe("Mint a token first.");
  });

  it("edit relists a server a migration left delisted", async () => {
    const { createPublicMcpServer, editPublicMcpServer } = await catalog();
    const created = await createPublicMcpServer(
      { display: "Ops" },
      { displayName: "Relist by edit", authMode: "none" },
      versionlessDocument("com.example/relist-edit"),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    await db.q(`UPDATE web.mcp_server SET status = 'delisted' WHERE id = $1`, [created.serverId]);
    const edited = await editPublicMcpServer(
      { display: "Ops" },
      created.serverId,
      { displayName: "Relisted", authMode: "none" },
      versionlessDocument("com.example/relist-edit"),
    );
    expect(edited.refusal).toBeNull();
    const rows = await db.q<{ status: string }>(`SELECT status FROM web.mcp_server WHERE id = $1`, [
      created.serverId,
    ]);
    expect(rows[0]?.status).toBe("active");
  });
});

describe("staff promotion advances only forward and relists", () => {
  it("refuses to promote a revision no longer newer than the current", async () => {
    const { promoteMcpRevision } = await catalog();
    const serverId = await seedPublicServer("mcps_backward", "com.example/backward");
    await addAndPromote(serverId, versionlessDocument("com.example/backward"));
    const older = await addRevision(serverId, serverDocument("com.example/backward", "2.0.0"));
    const newer = await addRevision(
      serverId,
      serverDocument("com.example/backward", "3.0.0", { _meta: { "sh.topos/auth": "none" } }),
    );
    if (older.refusal !== null || newer.refusal !== null) {
      throw new Error("seeding proposals failed");
    }
    // Promote the newest; the older one is now history, and promoting it would roll the offer back.
    expect((await promoteMcpRevision({ display: "Staff" }, newer.revisionId)).refusal).toBeNull();
    expect(await currentRevisionOf(serverId)).toBe(newer.revisionId);
    const stale = await promoteMcpRevision({ display: "Staff" }, older.revisionId);
    expect(stale.refusal?.code).toBe("MCP_REVISION_NOT_PROPOSAL");
    expect(await currentRevisionOf(serverId)).toBe(newer.revisionId);
  });

  it("relists a delisted server when its proposal is promoted", async () => {
    const { promoteMcpRevision } = await catalog();
    const serverId = await seedPublicServer("mcps_relist", "com.example/relist", {
      status: "delisted",
    });
    await addAndPromote(serverId, versionlessDocument("com.example/relist"));
    const proposal = await addRevision(
      serverId,
      serverDocument("com.example/relist", "2.0.0", { _meta: { "sh.topos/auth": "none" } }),
    );
    if (proposal.refusal !== null) {
      throw new Error(proposal.refusal.message);
    }
    expect(
      (await promoteMcpRevision({ display: "Staff" }, proposal.revisionId)).refusal,
    ).toBeNull();
    const rows = await db.q<{ status: string }>(`SELECT status FROM web.mcp_server WHERE id = $1`, [
      serverId,
    ]);
    expect(rows[0]?.status).toBe("active");
  });

  async function seedTierProposal(name: string, proposalId: string, tier: string): Promise<string> {
    const created = await (await catalog()).createPublicMcpServer(
      { display: "Ops" },
      { displayName: name, authMode: "oauth" },
      versionlessDocument(name),
    );
    if (created.refusal !== null) {
      throw new Error(created.refusal.message);
    }
    await db.q(
      `INSERT INTO web.mcp_server_revision (id, server_id, seq, document, transport, url)
       VALUES ($1, $2, 2, $3::jsonb, 'streamable-http', 'https://mcp.example.com/mcp')`,
      [
        proposalId,
        created.serverId,
        JSON.stringify({
          name,
          description: "A server for the suite.",
          remotes: [{ type: "streamable-http", url: "https://mcp.example.com/mcp" }],
          _meta: { "sh.topos/auth": tier },
        }),
      ],
    );
    return created.serverId;
  }

  it("refuses a proposal whose delivered tier differs from the verified one", async () => {
    // The column is verified truth (oauth); a proposal delivering a different tier must not
    // overwrite it — staff reconcile a genuine change through an edit, not a blind promote.
    const proposalId = `mcpr_${"c".repeat(32)}`;
    const serverId = await seedTierProposal("com.example/tier-mismatch", proposalId, "none");
    const res = await (await catalog()).promoteMcpRevision({ display: "Staff" }, proposalId);
    expect(res.refusal?.code).toBe("MCP_AUTH_TIER_MISMATCH");
    const rows = await db.q<{ auth_mode: string; current_revision_id: string }>(
      `SELECT auth_mode, current_revision_id FROM web.mcp_server WHERE id = $1`,
      [serverId],
    );
    // Column and pointer both untouched — verified truth held.
    expect(rows[0]?.auth_mode).toBe("oauth");
    expect(rows[0]?.current_revision_id).not.toBe(proposalId);
  });

  it("promotes a proposal whose delivered tier matches the verified one", async () => {
    const proposalId = `mcpr_${"d".repeat(32)}`;
    const serverId = await seedTierProposal("com.example/tier-match", proposalId, "oauth");
    expect(
      (await (await catalog()).promoteMcpRevision({ display: "Staff" }, proposalId)).refusal,
    ).toBeNull();
    const rows = await db.q<{ auth_mode: string; current_revision_id: string }>(
      `SELECT auth_mode, current_revision_id FROM web.mcp_server WHERE id = $1`,
      [serverId],
    );
    expect(rows[0]?.current_revision_id).toBe(proposalId);
    expect(rows[0]?.auth_mode).toBe("oauth");
  });

  it("refuses a proposal whose document delivers NO tier at all", async () => {
    const { promoteMcpRevision } = await catalog();
    const serverId = await seedPublicServer("mcps_notier", "com.example/no-tier-prop");
    await addAndPromote(serverId, versionlessDocument("com.example/no-tier-prop"));
    // The document carries no `sh.topos/auth`, so a machine would read no tier — refuse.
    const proposal = await addRevision(
      serverId,
      serverDocument("com.example/no-tier-prop", "2.0.0"),
    );
    if (proposal.refusal !== null) {
      throw new Error(proposal.refusal.message);
    }
    const res = await promoteMcpRevision({ display: "Staff" }, proposal.revisionId);
    expect(res.refusal?.code).toBe("MCP_AUTH_TIER_MISMATCH");
  });
});
