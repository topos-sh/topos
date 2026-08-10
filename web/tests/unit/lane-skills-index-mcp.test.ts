import { afterAll, beforeAll, describe, expect, it } from "vitest";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seedBundle,
  versionIdFor,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * THE ADDITIVE `mcp_server_name` FIELD on the session-lane catalog listing. It once carried a
 * registry-shaped `add`'s workspace-first join; that door is gone and no client reads the field
 * today, so this pins the SHAPE the lane still serves. Against a real scratch Postgres and
 * the stub vault, because the embedded name comes from the CURRENT version's document (the same
 * source the registry lane serves), never from a second index:
 *
 *  · an active `kind: 'mcp'` bundle whose document the vault serves carries its embedded name;
 *  · a skill bundle NEVER carries the field, whatever its catalog name looks like;
 *  · an mcp bundle whose document cannot be read stays undecorated (best-effort, additive) —
 *    the listing itself still answers;
 *  · an archived mcp bundle stays undecorated too (it delivers nothing and holds no name).
 */

let db: ScratchDb;
let vault: StubVault;
let wsId = "";

const WEATHER = {
  name: "io.github.acme/weather",
  description: "Current conditions for a named place.",
  version: "1.0.0",
  remotes: [{ type: "streamable-http", url: "https://weather.acme.example/mcp" }],
};

async function seedServer(
  bundleId: string,
  name: string,
  opts: { document?: unknown; status?: "active" | "archived" } = {},
): Promise<void> {
  await seedBundle(db, wsId, bundleId, name, { kind: "mcp", status: opts.status ?? "active" });
  if (opts.document !== undefined) {
    vault.seed(wsId, bundleId, versionIdFor(bundleId), [
      { path: "server.json", content: JSON.stringify(opts.document, null, 2) },
    ]);
  }
}

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_lane_mcp", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: vault.url,
  });
  wsId = await bootWorkspace();
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

describe("the catalog listing's embedded MCP server names", () => {
  it("decorates exactly the readable, active mcp entries — and nothing else", async () => {
    await seedServer("s_weather", "weather", { document: WEATHER });
    await seedServer("s_ghost", "ghost"); // no document in the vault
    await seedServer("s_old", "old-weather", {
      document: { ...WEATHER, name: "io.github.acme/old" },
      status: "archived",
    });
    // A skill bundle never carries the field, whatever its current version stores.
    await seedBundle(db, wsId, "s_deploy", "deploy");

    const { laneSkillsIndex } = await import("@/lib/db/queries.lane.server");
    const { withMcpServerNames } = await import("@/lib/mcp/catalog.server");
    const actor = asSession(wsId, "u_reader", "cs_reader", "member");
    const skills = await withMcpServerNames(wsId, await laneSkillsIndex(actor));

    const byId = new Map(skills.map((s) => [s.skill_id, s]));
    expect(byId.get("s_weather")?.mcp_server_name).toBe("io.github.acme/weather");
    expect(byId.get("s_weather")?.kind).toBe("mcp");
    // Best-effort: the unreadable document leaves its entry standing, undecorated.
    expect(byId.get("s_ghost")).toBeDefined();
    expect(byId.get("s_ghost")?.mcp_server_name).toBeUndefined();
    // Archived delivers nothing and holds no name on this listing.
    expect(byId.get("s_old")?.mcp_server_name).toBeUndefined();
    // The kind guard: a skill never carries the field.
    expect(byId.get("s_deploy")?.mcp_server_name).toBeUndefined();
    // The decoration is ADDITIVE — it drops nothing and reorders nothing.
    expect(skills.map((s) => s.skill_id)).toEqual(
      (await laneSkillsIndex(actor)).map((s) => s.skill_id),
    );
  });
});
