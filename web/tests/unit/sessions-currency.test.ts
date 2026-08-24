import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { mcpRevisionId } from "../helpers/mcp-ids";
import {
  asMember,
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";

/**
 * WHAT "current" MEANS ON THE SESSIONS PAGE, for both kinds of bundle.
 *
 * A file bundle is served by VERSION — the vault's content-addressed commit — so a machine
 * reports a 64-hex id and the page compares it against the workspace's current pointer. A
 * connected MCP server is served BY REVISION (`mcpr_…`, the catalog's own mint), so a machine
 * reports a revision id and the page must compare it against the RESOLVED revision.
 *
 * Comparing a reported revision against a pointer does not merely fail to match — it INVERTS the
 * page, because an MCP bundle written before the catalog existed still carries a pointer nobody
 * is served from: a machine that just updated reads "behind", while a machine so stale it still
 * reports that abandoned pointer reads "current". The page's whole promise ("watch until every
 * non-stale session reads current") is only true when both sides name the same thing, so every
 * case below is pinned against a bundle that carries BOTH a legacy pointer and a live revision.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

const ORIGIN = "http://x";

let db: ScratchDb;
let ws = "";

const OWNER = { id: "u_owner", name: "Olive Owner", email: "olive@example.com" };

/** The connected server's bundle — kind 'mcp', and it carries the legacy pointer too. */
const SERVER = { id: "s_srv", name: "fleet" };
/** An ordinary file bundle, so the same page is proven for both kinds in one read. */
const SKILL = { id: "s_doc", name: "release-guide" };

const CURRENT_REVISION = mcpRevisionId("fleet_current");
const OLD_REVISION = mcpRevisionId("fleet_old");

/** The machine that just ran `topos update` — it reports what the workspace serves. */
const UPDATED = "cs_updated";
/** The stale machine that still reports the pointer the catalog superseded. */
const STALE = "cs_stale";

async function report(
  sessionId: string,
  applied: { skillId: string; versionId: string }[],
): Promise<string> {
  const lane = await import("@/lib/db/queries.lane.server");
  return await lane.reportApplied(asSession(ws, OWNER.id, sessionId, "owner"), applied);
}

/** The Sessions page's own view of one machine's copy of one bundle. */
async function pageState(
  sessionId: string,
  bundleId: string,
): Promise<{ status: string; appliedVersionId: string; currentVersionId: string | null }> {
  const sessions = await import("@/lib/db/queries.sessions.server");
  const view = await sessions.workspaceSessions(asMember(ws, OWNER.id, "owner"));
  const machine = view.sessions.find((s) => s.sessionId === sessionId);
  const state = machine?.skills.find((s) => s.skillId === bundleId);
  if (state === undefined) {
    throw new Error(`no state for ${sessionId} × ${bundleId}`);
  }
  return {
    status: state.status,
    appliedVersionId: state.appliedVersionId,
    currentVersionId: state.currentVersionId,
  };
}

beforeAll(async () => {
  db = await createScratchDb("web_sessions_currency", { TOPOS_WEB_RATELIMIT: "off" });
  ws = await bootWorkspace();
  await seedUser(db, OWNER.id, OWNER.name, OWNER.email);
  await seatUser(db, ws, OWNER.id, "owner");
  await seedSession(db, UPDATED, ws, OWNER.id, "active", "laptop");
  await seedSession(db, STALE, ws, OWNER.id, "active", "old-desktop");
  // The MCP bundle keeps its plane pointer on purpose: that is the shape every server written
  // before the catalog has, and it is exactly what a pointer comparison reads by mistake.
  await seedBundle(db, ws, SERVER.id, SERVER.name, { kind: "mcp" });
  await seedBundle(db, ws, SKILL.id, SKILL.name);

  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ('mcps_fleet', NULL, 'com.example/fleet', 'Fleet', 'none', 'active')`,
  );
  for (const [id, seq, upstream] of [
    [OLD_REVISION, 1, "1.0.0"],
    [CURRENT_REVISION, 2, "1.1.0"],
  ] as const) {
    await db.q(
      `INSERT INTO web.mcp_server_revision
         (id, server_id, seq, upstream_version, document, transport, url, published_at, published_by)
       VALUES ($1, 'mcps_fleet', $2, $3, '{"name":"com.example/fleet"}'::jsonb,
               'streamable-http', 'https://fleet.example/mcp', now(), 'Staff')`,
      [id, seq, upstream],
    );
  }
  // The pointer moves only once both revisions exist — `current_revision_id` is a real FK.
  await db.q(`UPDATE web.mcp_server SET current_revision_id = $1 WHERE id = 'mcps_fleet'`, [
    CURRENT_REVISION,
  ]);
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id) VALUES ($1, $2, 'mcps_fleet')`,
    [SERVER.id, ws],
  );
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the Sessions page reads currency in the spelling each kind is served by", () => {
  it("a machine holding the RESOLVED revision of a connected server reads current", async () => {
    expect(await report(UPDATED, [{ skillId: SERVER.id, versionId: CURRENT_REVISION }])).toBe("ok");
    const state = await pageState(UPDATED, SERVER.id);
    expect(state.status).toBe("current");
    // …and the "current is …" the page would print names the revision, not the abandoned pointer.
    expect(state.currentVersionId).toBe(CURRENT_REVISION);
  });

  it("a machine still reporting the bundle's abandoned POINTER reads behind", async () => {
    // The 11-day-stale machine: it reports the vault version an MCP bundle carries from before
    // the catalog existed. That is not what this workspace serves, so it is behind.
    expect(await report(STALE, [{ skillId: SERVER.id, versionId: versionIdFor(SERVER.id) }])).toBe(
      "ok",
    );
    const state = await pageState(STALE, SERVER.id);
    expect(state.status).toBe("behind");
    expect(state.appliedVersionId).toBe(versionIdFor(SERVER.id));
    expect(state.currentVersionId).toBe(CURRENT_REVISION);
  });

  it("a machine on an EARLIER revision reads behind against the current one", async () => {
    expect(await report(STALE, [{ skillId: SERVER.id, versionId: OLD_REVISION }])).toBe("ok");
    expect((await pageState(STALE, SERVER.id)).status).toBe("behind");
  });

  it("a PINNED connection is measured against its pin, not the server's current", async () => {
    await db.q(`UPDATE web.bundle_mcp SET pinned_revision_id = $1 WHERE bundle_id = $2`, [
      OLD_REVISION,
      SERVER.id,
    ]);
    try {
      expect((await pageState(STALE, SERVER.id)).status).toBe("current");
      expect((await pageState(UPDATED, SERVER.id)).status).toBe("behind");
    } finally {
      await db.q(`UPDATE web.bundle_mcp SET pinned_revision_id = NULL WHERE bundle_id = $1`, [
        SERVER.id,
      ]);
    }
  });

  it("a FILE bundle is still measured against the vault pointer", async () => {
    await report(UPDATED, [
      { skillId: SERVER.id, versionId: CURRENT_REVISION },
      { skillId: SKILL.id, versionId: versionIdFor(SKILL.id) },
    ]);
    await report(STALE, [
      { skillId: SERVER.id, versionId: OLD_REVISION },
      { skillId: SKILL.id, versionId: "b7".repeat(32) },
    ]);
    expect((await pageState(UPDATED, SKILL.id)).status).toBe("current");
    expect((await pageState(STALE, SKILL.id)).status).toBe("behind");
  });
});

describe("the Sessions route", () => {
  it("serves the same verdict the page renders — the machine on the revision reads current", async () => {
    session = { user: OWNER };
    await report(UPDATED, [{ skillId: SERVER.id, versionId: CURRENT_REVISION }]);
    const { loader } = await import("@/routes/sessions");
    const result = (await loader({
      request: new Request(`${ORIGIN}/settings/sessions`, { headers: { accept: "text/html" } }),
      params: {},
      context: {},
    } as unknown as Parameters<typeof loader>[0])) as {
      view: {
        sessions: {
          sessionId: string;
          skills: { skillId: string; status: string; currentVersionId: string | null }[];
        }[];
      };
    };
    const rendered = result.view.sessions
      .find((s) => s.sessionId === UPDATED)
      ?.skills.find((s) => s.skillId === SERVER.id);
    expect(rendered?.status).toBe("current");
    expect(rendered?.currentVersionId).toBe(CURRENT_REVISION);
  });
});
