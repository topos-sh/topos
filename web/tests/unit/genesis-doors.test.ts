import { Buffer } from "node:buffer";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * PUBLISHING A BRAND-NEW BUNDLE HAPPENS ONCE — proven at both doors.
 *
 * A bundle of FILES can enter a workspace from an agent's `topos publish` or from
 * add-from-GitHub. They carry different bytes and each records something of its own, but the
 * BUNDLE they produce must be the same object: same catalog row, same birth kind, same placement
 * rule, same `skill_registered` audit. Two doors that agree today and drift tomorrow is exactly
 * the failure this suite is here to catch, so it compares the rows rather than the code.
 *
 * It also pins what the kind RECORD decides rather than the call site: where a new bundle reaches
 * by default, and — for a kind whose bundles are catalog rows rather than files — that bytes are
 * refused outright, in the same words at every door.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

/** add-from-GitHub's network arm, answered from here — no test touches the network. */
const upstreamTree = vi.hoisted(() => ({
  files: [] as { path: string; bytes: Buffer; executable: boolean }[],
  commit: "a".repeat(40),
  license: "MIT",
}));

vi.mock("@/lib/db/upstream.server", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/db/upstream.server")>();
  return {
    ...actual,
    fetchUpstreamTree: async () => ({ ...upstreamTree }),
    governedCopiesOf: async () => [],
    armUpstreamChecker: () => undefined,
  };
});

let db: ScratchDb;
let vault: StubVault;
let wsId = "";

const ORIGIN = "http://x";
const MEMBER = { id: "u_auth", name: "Author", email: "author@example.com" };

const SERVER = (name: string) => ({
  name,
  description: "A server.",
  version: "1.0.0",
  remotes: [{ type: "streamable-http", url: "https://acme.example/mcp" }],
});

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_genesis_doors", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: vault.url,
  });
  wsId = await bootWorkspace();
  await seedUser(db, MEMBER.id, MEMBER.name, MEMBER.email);
  await seatUser(db, wsId, MEMBER.id, "owner");
  await seedSession(db, "sn_auth", wsId, MEMBER.id);
  session = { user: MEMBER };
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

/** The catalog row + its placement + its birth audit — the object every door must agree on. */
async function bundleShapeOf(bundleId: string) {
  const [row] = await db.q<{
    kind: string;
    name: string;
    display_name: string | null;
    status: string;
    protection: string | null;
    created_by: string | null;
  }>(
    `SELECT kind, name, display_name, status, protection, created_by
     FROM web.bundle WHERE id = $1`,
    [bundleId],
  );
  const channels = await db.q<{ name: string }>(
    `SELECT c.name FROM web.channel_bundle cb JOIN web.channel c ON c.id = cb.channel_id
     WHERE cb.bundle_id = $1 ORDER BY c.name`,
    [bundleId],
  );
  const audits = await db.q<{ kind: string; outcome: string; details: unknown }>(
    `SELECT kind, outcome, details FROM web.audit_event
     WHERE subject = $1 AND kind = 'skill_registered'`,
    [bundleId],
  );
  return {
    kind: row?.kind,
    displayName: row?.display_name,
    status: row?.status,
    protection: row?.protection,
    createdBy: row?.created_by,
    channels: channels.map((c) => c.name),
    audit: audits.map((a) => ({ kind: a.kind, outcome: a.outcome, details: a.details })),
  };
}

/**
 * The same shape with the two things that are SUPPOSED to differ blanked: the display name and
 * the minted catalog name each door was told. Everything left is what the shared sequence
 * decides, and that is what has to match.
 */
function sequenceShapeOf(shape: Awaited<ReturnType<typeof bundleShapeOf>>) {
  return {
    ...shape,
    displayName: null,
    audit: shape.audit.map((a) => ({ ...a, details: { name: "<minted>" } })),
  };
}

/** DOOR ONE — the session lane's `topos publish` of a bundle the workspace has never seen. */
async function publishThroughLane(args: {
  bundleId: string;
  displayName: string;
  kind?: string;
  files: { path: string; mode: string; content_base64: string }[];
}): Promise<Record<string, unknown>> {
  const { publishFlow } = await import("@/lib/api/publish-flow.server");
  const raw = JSON.stringify({ skill_id: args.bundleId });
  const res = await publishFlow({
    actor: asSession(wsId, MEMBER.id, "sn_auth", "owner"),
    raw,
    opId: crypto.randomUUID(),
    skillId: args.bundleId,
    expected: 0,
    candidate: { files: args.files, parents: [], author: MEMBER.name, message: "genesis" },
    displayName: args.displayName,
    channel: null,
    kind: args.kind ?? null,
    command: "publish",
    forceProposal: false,
  });
  return (await res.json()) as Record<string, unknown>;
}

/** DOOR TWO — add-from-GitHub. */
async function publishThroughImport(name: string): Promise<{ status: number; location: string }> {
  const { action } = await import("@/routes/skill-import");
  const form = new FormData();
  form.set("intent", "publish");
  form.set("repo", "acme/skills");
  form.set("subdir", "deploy");
  form.set("commit", upstreamTree.commit);
  form.set("name", name);
  try {
    await action({
      request: new Request(`${ORIGIN}/skills/import`, {
        method: "POST",
        headers: { origin: ORIGIN },
        body: form,
      }),
      params: {},
      context: {},
    } as never);
  } catch (thrown) {
    if (thrown instanceof Response) {
      return { status: thrown.status, location: thrown.headers.get("location") ?? "" };
    }
    throw thrown;
  }
  throw new Error("the import did not redirect");
}

describe("both genesis doors produce the same bundle", () => {
  it("a SKILL born on the lane and one born from GitHub are the same object", async () => {
    upstreamTree.files = [{ path: "SKILL.md", bytes: Buffer.from("# deploy"), executable: false }];

    const envelope = await publishThroughLane({
      bundleId: "s_lane_skill",
      displayName: "Deploy",
      files: [
        {
          path: "SKILL.md",
          mode: "100644",
          content_base64: Buffer.from("# deploy").toString("base64"),
        },
      ],
    });
    expect(envelope.ok).toBe(true);
    const redirected = await publishThroughImport("deploy-imported");
    expect(redirected.status).toBe(302);

    const [imported] = await db.q<{ id: string }>(
      `SELECT id FROM web.bundle WHERE name = $1 AND workspace_id = $2`,
      ["deploy-imported", wsId],
    );
    const fromLane = await bundleShapeOf("s_lane_skill");
    const fromImport = await bundleShapeOf(imported?.id ?? "");

    // The display names differ (each door was told a different one); everything the SEQUENCE
    // decides is identical — including reaching the default channel, which is what this kind's
    // record says a genesis publish does when no destination is named.
    expect(fromLane.kind).toBe("skill");
    expect(sequenceShapeOf(fromLane)).toEqual(sequenceShapeOf(fromImport));
    expect(fromLane.channels).toEqual(["everyone"]);
    expect(fromLane.audit).toEqual([
      { kind: "skill_registered", outcome: "ok", details: { name: "deploy" } },
    ]);
    expect(fromImport.audit).toEqual([
      { kind: "skill_registered", outcome: "ok", details: { name: "deploy-imported" } },
    ]);
  });
});

describe("a kind whose bundles are catalog rows refuses bytes", () => {
  it("the session lane's publish is DENIED, and nothing is registered", async () => {
    const doc = JSON.stringify(SERVER("io.github.acme/lane"));
    const envelope = await publishThroughLane({
      bundleId: "s_lane_mcp",
      displayName: "lane-server",
      kind: "mcp",
      files: [
        {
          path: "server.json",
          mode: "100644",
          content_base64: Buffer.from(doc).toString("base64"),
        },
      ],
    });
    const error = envelope.error as { code: string; context: { message?: string } };
    expect(error.code).toBe("KIND_HAS_NO_FILES");
    expect(error.context.message).toBe(
      "MCP servers are catalog entries, not bundles of files — add one on the MCP servers page",
    );
    // Refused BEFORE custody: no bytes ingested, no catalog row written.
    expect(await db.q(`SELECT id FROM web.bundle WHERE id = $1`, ["s_lane_mcp"])).toEqual([]);
  });

  it("a proposal against a server already in the catalog is refused in the same words", async () => {
    await seedBundle(db, wsId, "s_connected", "connected-server", {
      kind: "mcp",
      withPointer: false,
    });
    const { publishFlow } = await import("@/lib/api/publish-flow.server");
    const raw = JSON.stringify({ skill_id: "s_connected" });
    const res = await publishFlow({
      actor: asSession(wsId, MEMBER.id, "sn_auth", "owner"),
      raw,
      opId: crypto.randomUUID(),
      skillId: "s_connected",
      expected: 0,
      candidate: {
        files: [
          {
            path: "server.json",
            mode: "100644",
            content_base64: Buffer.from(JSON.stringify(SERVER("io.github.acme/x"))).toString(
              "base64",
            ),
          },
        ],
        parents: [],
        author: MEMBER.name,
        message: "propose",
      },
      displayName: null,
      channel: null,
      command: "publish",
      forceProposal: true,
    });
    const envelope = (await res.json()) as { error?: { code: string } };
    expect(envelope.error?.code).toBe("KIND_HAS_NO_FILES");
    expect(await db.q(`SELECT id FROM web.proposal WHERE bundle_id = $1`, ["s_connected"])).toEqual(
      [],
    );
  });
});
