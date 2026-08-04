import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";

/**
 * TWO SECTIONS, ONE CATALOG — the kind split, against a real scratch Postgres.
 *
 * A skill and an MCP server share every mechanic underneath (one catalog table, one version
 * history, one delivery predicate) and are addressed as different things above: the rail lists
 * them in separate sections and each is reachable ONLY under its own base. Three claims are
 * proven here, because each is a place the split could silently rot back into a mixed list:
 *
 *  · the chrome loader hands the rail two lists, and an `mcp` bundle is in exactly one of them;
 *  · the bundle face answers at `mcp/<name>` for a server and `skills/<name>` for a skill;
 *  · a MEMBER who addresses one through the other's base is REDIRECTED to the canonical path —
 *    not 404'd. The 404 belongs to the anonymous probe (asserted too), which is what keeps the
 *    face existence-blind while a member, who can see both sections anyway, keeps its links.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let wsId = "";

const ORIGIN = "http://x";
const MEMBER = { id: "u_mem", name: "Member", email: "mem@example.com" };

beforeAll(async () => {
  db = await createScratchDb("web_bundle_sections");
  wsId = await bootWorkspace();
  await seedUser(db, MEMBER.id, MEMBER.name, MEMBER.email);
  await seatUser(db, wsId, MEMBER.id, "member");
  // Nothing published on either (`withPointer: false`): the catalog row IS the identity surface,
  // and a pointer-less bundle keeps this suite off the vault entirely.
  await seedBundle(db, wsId, "b_skill", "pr-describe", { withPointer: false });
  await seedBundle(db, wsId, "b_mcp", "weather", { kind: "mcp", withPointer: false });
  session = { user: MEMBER };
}, 60000);

afterAll(async () => {
  await db.drop();
});

/** The bundle face, addressed through one base or the other. Returns the thrown Response for a
 *  redirect/404 and the loader data for a render. */
async function face(
  params: { skill?: string; server?: string },
  path: string,
): Promise<{ status: number; location: string | null; data: Record<string, unknown> | null }> {
  const { loader } = await import("@/routes/skill-current");
  try {
    const data = (await loader({
      request: new Request(`${ORIGIN}${path}`),
      params,
      context: {},
    } as unknown as Parameters<typeof loader>[0])) as Record<string, unknown>;
    return { status: 200, location: null, data };
  } catch (thrown) {
    if (thrown instanceof Response) {
      return { status: thrown.status, location: thrown.headers.get("location"), data: null };
    }
    // The house 404 throws the framework's data wrapper, not a Response.
    if (typeof thrown === "object" && thrown !== null && "init" in thrown) {
      const init = (thrown as { init?: ResponseInit }).init;
      return { status: init?.status ?? 500, location: null, data: null };
    }
    throw thrown;
  }
}

describe("the rail's two sections", () => {
  it("splits the catalog by kind — a server is in servers and NOT in skills", async () => {
    const { loadChrome } = await import("@/lib/shell/chrome.server");
    const { asUser } = await import("./helpers/scratch-db");
    const chrome = await loadChrome(new Request(`${ORIGIN}/`), asUser(MEMBER.id, MEMBER.name));
    const workspace = chrome.workspace;
    expect(workspace).not.toBeNull();
    expect(workspace?.skills.map((s) => s.name)).toEqual(["pr-describe"]);
    expect(workspace?.servers.map((s) => s.name)).toEqual(["weather"]);
  });
});

describe("the bundle face's two bases", () => {
  it("answers a server under mcp/ and a skill under skills/", async () => {
    const server = await face({ server: "weather" }, "/mcp/weather");
    expect(server.status).toBe(200);
    expect(server.data?.skill).toBe("weather");
    expect(server.data?.kind).toBe("mcp");

    const skill = await face({ skill: "pr-describe" }, "/skills/pr-describe");
    expect(skill.status).toBe(200);
    expect(skill.data?.kind).toBe("skill");
  });

  it("sends a member off the wrong base to the canonical path, both ways", async () => {
    const asSkill = await face({ skill: "weather" }, "/skills/weather");
    expect(asSkill.status).toBe(302);
    expect(asSkill.location).toBe("/mcp/weather");

    const asServer = await face({ server: "pr-describe" }, "/mcp/pr-describe");
    expect(asServer.status).toBe(302);
    expect(asServer.location).toBe("/skills/pr-describe");
  });

  it("keeps the anonymous 404 on both bases — the redirect is a member's convenience", async () => {
    const held = session;
    session = null;
    try {
      expect((await face({ server: "weather" }, "/mcp/weather")).status).toBe(404);
      expect((await face({ skill: "weather" }, "/skills/weather")).status).toBe(404);
    } finally {
      session = held;
    }
  });
});
