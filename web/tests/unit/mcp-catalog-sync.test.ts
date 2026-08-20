import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { bootWorkspace, createScratchDb, type ScratchDb } from "./helpers/scratch-db";

/**
 * THE CATALOG SYNC — the committed file reconciled into the public `mcp_server` rows.
 *
 * The properties that matter: it CREATES public servers from the file and advances their current
 * to each new file version while nobody has touched them (auto-advance); once a server is manually
 * curated a file version lands as a NON-CURRENT proposal instead; it dedupes by content so a
 * settled catalog no-ops; and a document a staff member dismissed is never re-proposed. The suite
 * drives the real `syncMcpCatalog` against a real scratch Postgres with an INJECTED entry set, so
 * it never depends on the committed file's contents.
 */

const SCHEMA = "https://static.modelcontextprotocol.io/schemas/2025-12-11/server.schema.json";

type Json = Record<string, unknown>;

function entry(
  name: string,
  opts: { description?: string; authTier?: string; authNote?: string; title?: string } = {},
): Json {
  const catalog: Json = {
    authTier: opts.authTier ?? "none",
    curatedBy: "Topos",
    lastVerified: "2026-08-19",
  };
  if (opts.authNote !== undefined) {
    catalog.authNote = opts.authNote;
  }
  return {
    $schema: SCHEMA,
    name,
    title: opts.title ?? name,
    description: opts.description ?? "A server for the sync suite.",
    remotes: [{ type: "streamable-http", url: "https://mcp.example.com/mcp" }],
    _meta: { "sh.topos/catalog": catalog },
  };
}

let db: ScratchDb;

async function sync(entries: Json[]) {
  const { syncMcpCatalog } = await import("@/lib/db/mcp-catalog-sync.server");
  return await syncMcpCatalog(entries);
}

async function serverRow(name: string) {
  const rows = await db.q<{
    id: string;
    current_revision_id: string | null;
    manually_curated: boolean;
    display_name: string;
    description: string | null;
  }>(
    `SELECT id, current_revision_id, manually_curated, display_name, description
      FROM web.mcp_server WHERE workspace_id IS NULL AND name = $1`,
    [name],
  );
  return rows[0];
}

async function revisionCount(name: string): Promise<number> {
  const rows = await db.q<{ n: number }>(
    `SELECT count(*) AS n FROM web.mcp_server_revision r
     JOIN web.mcp_server s ON s.id = r.server_id
     WHERE s.workspace_id IS NULL AND s.name = $1`,
    [name],
  );
  return Number(rows[0]?.n ?? 0);
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_sync");
  await bootWorkspace();
}, 90000);

afterAll(async () => {
  await db.drop();
});

describe("a brand-new server", () => {
  it("is created and its first revision promoted to current", async () => {
    const report = await sync([entry("com.sync/new")]);
    expect(report.created).toBe(1);
    expect(report.promoted).toBe(1);
    const row = await serverRow("com.sync/new");
    expect(row?.current_revision_id).not.toBeNull();
    expect(row?.manually_curated).toBe(false);
    expect(await revisionCount("com.sync/new")).toBe(1);
  });
});

describe("idempotency", () => {
  it("re-running the same file no-ops — no new revision, current unchanged", async () => {
    await sync([entry("com.sync/idem")]);
    const before = await serverRow("com.sync/idem");
    const report = await sync([entry("com.sync/idem")]);
    expect(report.created).toBe(0);
    expect(report.promoted).toBe(0);
    expect(report.unchanged).toBe(1);
    const after = await serverRow("com.sync/idem");
    expect(after?.current_revision_id).toBe(before?.current_revision_id);
    expect(await revisionCount("com.sync/idem")).toBe(1);
  });
});

describe("auto-advance while untouched by staff", () => {
  it("a changed file version appends a revision and moves current to it", async () => {
    await sync([entry("com.sync/track")]);
    const first = await serverRow("com.sync/track");
    const report = await sync([entry("com.sync/track", { description: "Now it says more." })]);
    expect(report.promoted).toBe(1);
    const second = await serverRow("com.sync/track");
    expect(second?.current_revision_id).not.toBe(first?.current_revision_id);
    expect(await revisionCount("com.sync/track")).toBe(2);
  });

  it("refreshes the editorial half from the file", async () => {
    await sync([entry("com.sync/editorial", { title: "Old Name" })]);
    await sync([entry("com.sync/editorial", { title: "New Name", description: "New words." })]);
    const row = await serverRow("com.sync/editorial");
    expect(row?.display_name).toBe("New Name");
    expect(row?.description).toBe("New words.");
  });
});

describe("manually curated: the file proposes, it never advances", () => {
  it("a file version lands as a non-current proposal once a staff member has curated", async () => {
    await sync([entry("com.sync/curated")]);
    const server = await serverRow("com.sync/curated");
    // Simulate a staff edit/promote having curated this server.
    await db.q(`UPDATE web.mcp_server SET manually_curated = true WHERE id = $1`, [server?.id]);

    const report = await sync([entry("com.sync/curated", { description: "A newer take." })]);
    expect(report.proposed).toBe(1);
    expect(report.promoted).toBe(0);
    const after = await serverRow("com.sync/curated");
    // Current did NOT move; the new revision is a proposal behind the pointer.
    expect(after?.current_revision_id).toBe(server?.current_revision_id);
    expect(await revisionCount("com.sync/curated")).toBe(2);
  });

  it("does not overwrite the editorial half of a curated server", async () => {
    await sync([entry("com.sync/curated2", { title: "Kept" })]);
    const server = await serverRow("com.sync/curated2");
    await db.q(`UPDATE web.mcp_server SET manually_curated = true WHERE id = $1`, [server?.id]);
    await sync([entry("com.sync/curated2", { title: "Overwritten?", description: "changed" })]);
    const after = await serverRow("com.sync/curated2");
    expect(after?.display_name).toBe("Kept");
  });
});

describe("a dismissed document never re-appears", () => {
  it("the file re-offering a dismissed document is skipped, not re-proposed", async () => {
    await sync([entry("com.sync/dismissed")]);
    const server = await serverRow("com.sync/dismissed");
    await db.q(`UPDATE web.mcp_server SET manually_curated = true WHERE id = $1`, [server?.id]);
    // A file version proposes; a staff member dismisses it.
    await sync([entry("com.sync/dismissed", { description: "A take staff rejected." })]);
    const proposalRows = await db.q<{ id: string }>(
      `SELECT r.id FROM web.mcp_server_revision r
       WHERE r.server_id = $1 AND r.id <> $2`,
      [server?.id, server?.current_revision_id],
    );
    const proposalId = proposalRows[0]?.id;
    const { dismissMcpRevision } = await import("@/lib/db/queries.mcp-catalog.server");
    expect(
      (await dismissMcpRevision({ display: "Staff" }, proposalId as string)).refusal,
    ).toBeNull();

    const before = await revisionCount("com.sync/dismissed");
    const report = await sync([
      entry("com.sync/dismissed", { description: "A take staff rejected." }),
    ]);
    expect(report.dismissedSkipped).toBe(1);
    expect(report.proposed).toBe(0);
    expect(await revisionCount("com.sync/dismissed")).toBe(before);
  });
});

describe("the file only adds", () => {
  it("a name dropped from a later sync is left standing", async () => {
    await sync([entry("com.sync/lingers")]);
    // A subsequent sync that no longer names it must not delete it.
    await sync([entry("com.sync/other")]);
    const row = await serverRow("com.sync/lingers");
    expect(row?.current_revision_id).not.toBeNull();
  });
});
