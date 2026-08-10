import { Buffer } from "node:buffer";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  asSession,
  bootWorkspace,
  createScratchDb,
  recordSeededIdentities,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * PUBLISHING A BRAND-NEW BUNDLE HAPPENS ONCE — proven at all three doors.
 *
 * A bundle can enter a workspace from an agent's `topos publish`, from add-from-GitHub, or from
 * add-an-MCP-server. They carry different bytes and each records something of its own, but the
 * BUNDLE they produce must be the same object: same catalog row, same birth kind, same placement
 * rule, same `skill_registered` audit. Three doors that agree today and drift tomorrow is exactly
 * the failure this suite is here to catch, so it compares the rows rather than the code.
 *
 * It also pins the two things the kind RECORD decides rather than the call site — where a new
 * bundle reaches by default, and whether its bytes face a gate — and the refusal a name conflict
 * produces, which must read identically wherever a person meets it.
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
  await seedSession(db, "cs_auth", wsId, MEMBER.id);
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
    actor: asSession(wsId, MEMBER.id, "cs_auth", "owner"),
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

/** DOOR THREE — add-an-MCP-server (the paste arm). */
async function publishThroughMcpPage(fields: {
  document: string;
  name: string;
  channel?: string;
}): Promise<{ status: number; body: Record<string, unknown>; location: string | null }> {
  const { action } = await import("@/routes/mcp-new");
  const form = new FormData();
  form.set("intent", "publish");
  form.set("document", fields.document);
  form.set("name", fields.name);
  form.set("channel", fields.channel ?? "");
  try {
    const result = (await action({
      request: new Request(`${ORIGIN}/mcp/new`, {
        method: "POST",
        headers: { origin: ORIGIN },
        body: form,
      }),
      params: {},
      context: {},
    } as never)) as { data?: unknown; init?: ResponseInit };
    return {
      status: result.init?.status ?? 200,
      body: (result.data ?? {}) as Record<string, unknown>,
      location: null,
    };
  } catch (thrown) {
    if (thrown instanceof Response) {
      return { status: thrown.status, body: {}, location: thrown.headers.get("location") };
    }
    throw thrown;
  }
}

describe("the three genesis doors produce the same bundle", () => {
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

  it("an MCP SERVER born on the lane and one born on its page are the same object", async () => {
    const laneDoc = JSON.stringify(SERVER("io.github.acme/lane"));
    const envelope = await publishThroughLane({
      bundleId: "s_lane_mcp",
      displayName: "lane-server",
      kind: "mcp",
      files: [
        {
          path: "server.json",
          mode: "100644",
          content_base64: Buffer.from(laneDoc).toString("base64"),
        },
      ],
    });
    expect(envelope.ok).toBe(true);

    const page = await publishThroughMcpPage({
      document: JSON.stringify(SERVER("io.github.acme/page")),
      name: "page-server",
    });
    expect(page.status).toBe(302);

    const [fromPageRow] = await db.q<{ id: string }>(
      `SELECT id FROM web.bundle WHERE name = $1 AND workspace_id = $2`,
      ["page-server", wsId],
    );
    const fromLane = await bundleShapeOf("s_lane_mcp");
    const fromPage = await bundleShapeOf(fromPageRow?.id ?? "");

    expect(fromLane.kind).toBe("mcp");
    // The two doors agree on everything the SHARED sequence decides — but NOT on where a new
    // bundle reaches, because that is the door's ruling, not the kind's.
    expect(sequenceShapeOf({ ...fromLane, channels: [] })).toEqual(
      sequenceShapeOf({ ...fromPage, channels: [] }),
    );

    // THE SESSION LANE KEEPS THE WIRE'S SEMANTICS. An agent's `topos publish` with no `--to`
    // reaches the workspace's default channel — for every kind — which is what puts the bundle
    // on the team's machines. An MCP server is not an exception: a server nobody receives is not
    // a server the team can call.
    expect(fromLane.channels).toEqual(["everyone"]);
    expect((envelope.receipt as { details?: unknown } | undefined)?.details).toBeUndefined();

    // THE WEB CREATION PAGE RESTS ON "NO CHANNEL". Importing a server on a form is a different
    // act from an agent publishing one: the field rests on no channel, and nothing arriving
    // without one may be read as consent to reach the whole workspace.
    expect(fromPage.channels).toEqual([]);

    // Both recorded the registry name they serve, which is what makes it unavailable to anyone
    // else — one door cannot skip the claim.
    const claims = await db.q<{ bundle_id: string; identity: string }>(
      `SELECT bundle_id, identity FROM web.bundle_identity WHERE workspace_id = $1 ORDER BY identity`,
      [wsId],
    );
    expect(claims).toEqual([
      { bundle_id: "s_lane_mcp", identity: "io.github.acme/lane" },
      { bundle_id: fromPageRow?.id, identity: "io.github.acme/page" },
    ]);
  });
});

describe("a name conflict reads the same at every door", () => {
  const TAKEN = "io.github.acme/contested";
  /** The one wording, built from the holder — what each door below must reproduce exactly. */
  let expectedMessage = "";

  beforeAll(async () => {
    const versionId = versionIdFor("s_holder");
    await seedBundle(db, wsId, "s_holder", "holder", { kind: "mcp", versionId });
    vault.seed(wsId, "s_holder", versionId, [
      { path: "server.json", content: JSON.stringify(SERVER(TAKEN), null, 2) },
    ]);
    await recordSeededIdentities();
    expectedMessage = `${TAKEN} is already published in this workspace as holder — view /mcp/holder`;
  });

  it("the session lane's publish", async () => {
    const doc = JSON.stringify(SERVER(TAKEN));
    const envelope = await publishThroughLane({
      bundleId: "s_lane_dupe",
      displayName: "dupe",
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
    expect(error.code).toBe("MCP_NAME_TAKEN");
    expect(error.context.message).toBe(expectedMessage);
    // Refused BEFORE custody: no bytes were ingested and no catalog row was written.
    const rows = await db.q(`SELECT id FROM web.bundle WHERE id = $1`, ["s_lane_dupe"]);
    expect(rows).toEqual([]);
  });

  it("the add-an-MCP-server page", async () => {
    const page = await publishThroughMcpPage({
      document: JSON.stringify(SERVER(TAKEN)),
      name: "page-dupe",
    });
    expect(page.status).toBe(400);
    expect(page.body.code).toBe("MCP_NAME_TAKEN");
    expect(page.body.error).toBe(expectedMessage);
    // The page can link straight to the holder rather than leaving a name to hunt for.
    expect(page.body.at).toBe("mcp/holder");
  });

  it("a review approve that would rename a server onto the taken name", async () => {
    const versionId = versionIdFor("s_rev");
    const candidate = versionIdFor("s_rev_candidate");
    await seedBundle(db, wsId, "s_rev", "rev", { kind: "mcp", versionId });
    vault.seed(wsId, "s_rev", versionId, [
      { path: "server.json", content: JSON.stringify(SERVER("io.github.acme/rev"), null, 2) },
    ]);
    await recordSeededIdentities();
    vault.seed(wsId, "s_rev", candidate, [
      { path: "server.json", content: JSON.stringify(SERVER(TAKEN), null, 2) },
    ]);
    const { movePointerWithKindPrecondition } = await import("@/lib/db/queries.custody.server");
    let moved = false;
    const claimed = await movePointerWithKindPrecondition({
      kind: "mcp",
      actor: asSession(wsId, MEMBER.id, "cs_auth", "owner"),
      bundleId: "s_rev",
      versionId: candidate,
      move: async () => {
        moved = true;
        return { kind: "ok" as const };
      },
    });
    expect(claimed.refusal?.code).toBe("MCP_NAME_TAKEN");
    expect(claimed.refusal?.message).toBe(expectedMessage);
    // The precondition failed, so the pointer never moved.
    expect(moved).toBe(false);
  });
});

describe("the backfill records what an existing server already serves", () => {
  it("claims every readable document, names every unreadable one, and skips nothing silently", async () => {
    const { backfillBundleIdentities } = await import("@/lib/db/bundle-identity.server");

    // Two servers standing as if published before the claim existed: one whose document the
    // vault serves, one whose bytes it does not.
    const readable = versionIdFor("s_fill_ok");
    await seedBundle(db, wsId, "s_fill_ok", "fill-ok", { kind: "mcp", versionId: readable });
    vault.seed(wsId, "s_fill_ok", readable, [
      { path: "server.json", content: JSON.stringify(SERVER("io.github.acme/filled"), null, 2) },
    ]);
    // Seeded with a pointer but NO bytes behind it — the unreadable case.
    await seedBundle(db, wsId, "s_fill_bad", "fill-bad", {
      kind: "mcp",
      versionId: versionIdFor("s_fill_bad"),
    });
    // A plain skill: it has no identity namespace, so the backfill must not touch it.
    const skillVersion = versionIdFor("s_fill_skill");
    await seedBundle(db, wsId, "s_fill_skill", "fill-skill", { versionId: skillVersion });

    const report = await backfillBundleIdentities();
    expect(report.claimed).toBeGreaterThanOrEqual(1);
    // NEVER A SILENT SKIP: the bundle left without a claim is named, workspace and all.
    expect(report.unreadable).toContain(`${wsId}/fill-bad`);
    expect(report.contested).toEqual([]);

    const claimed = await db.q<{ identity: string }>(
      `SELECT identity FROM web.bundle_identity WHERE bundle_id = $1`,
      ["s_fill_ok"],
    );
    expect(claimed.map((r) => r.identity)).toEqual(["io.github.acme/filled"]);
    // The unreadable one holds nothing, so the name it might serve stays available — the honest
    // state, and one a later boot can still correct.
    expect(
      await db.q(`SELECT 1 FROM web.bundle_identity WHERE bundle_id = $1`, ["s_fill_bad"]),
    ).toEqual([]);
    expect(
      await db.q(`SELECT 1 FROM web.bundle_identity WHERE bundle_id = $1`, ["s_fill_skill"]),
    ).toEqual([]);
  });

  it("runs again cleanly — a settled catalog claims nothing new", async () => {
    const { backfillBundleIdentities } = await import("@/lib/db/bundle-identity.server");
    const before = await db.q<{ n: string }>(`SELECT count(*)::text AS n FROM web.bundle_identity`);
    const report = await backfillBundleIdentities();
    const after = await db.q<{ n: string }>(`SELECT count(*)::text AS n FROM web.bundle_identity`);
    expect(report.claimed).toBe(0);
    expect(after[0]?.n).toBe(before[0]?.n);
  });
});
