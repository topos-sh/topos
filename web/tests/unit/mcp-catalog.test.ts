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
    const proposal = await addRevision(serverId, serverDocument("com.example/promote", "3.0.0"));
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
