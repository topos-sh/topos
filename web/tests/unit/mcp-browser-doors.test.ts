import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  recordSeededIdentities,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * THE TWO BROWSER DOORS THAT MOVE A POINTER: the review page's Approve and the history page's
 * roll-back. They land the same fact the session lane's `/reviews` and `/reverts` land — the
 * version they point at becomes `current` — so for an mcp bundle they make the SAME claim on the
 * registry name that version embeds, and must refuse it on the same terms.
 *
 * Without the check the sequence is: propose a rename onto a free name · someone else publishes
 * and claims it · approve in the BROWSER — and the workspace ends up with two active mcp bundles
 * embedding one name, which makes `…/servers/{name}` a coin flip. The lane doors are pinned in
 * mcp-publish-gate.test.ts; these are their browser twins, driven through the REAL route actions
 * with a reviewer's cookie session, a real scratch Postgres, and the app's one custody transport
 * re-pointed at an in-process stub vault — so a refusal that reached NO custody route is proven
 * by an empty recorder.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let vault: StubVault;
let wsId = "";

const ORIGIN = "http://x";

const WEATHER = {
  name: "io.github.acme/weather",
  description: "Current conditions for a named place.",
  version: "1.0.0",
  remotes: [{ type: "streamable-http", url: "https://weather.acme.example/mcp" }],
};

/** What an action answered: `data()` hands back a wrapper the framework serializes later, and a
 * guard throws a Response — both read through one shape so a test asserts the OUTCOME. */
interface ActionResult {
  status: number;
  body: Record<string, unknown>;
}

type RouteAction = (args: {
  request: Request;
  params: Record<string, string>;
  context: unknown;
}) => unknown;

async function drive(
  action: RouteAction,
  params: Record<string, string>,
  fields: Record<string, string>,
): Promise<ActionResult> {
  const form = new FormData();
  for (const [name, value] of Object.entries(fields)) {
    form.set(name, value);
  }
  let result: unknown;
  try {
    result = await action({
      request: new Request(`${ORIGIN}/mcp/x`, {
        method: "POST",
        headers: { origin: ORIGIN },
        body: form,
      }),
      params,
      context: {},
    });
  } catch (thrown) {
    result = thrown;
  }
  if (result instanceof Response) {
    let body: Record<string, unknown> = {};
    try {
      body = (await result.json()) as Record<string, unknown>;
    } catch {
      body = {};
    }
    return { status: result.status, body };
  }
  if (typeof result === "object" && result !== null && "data" in result) {
    const wrapper = result as { data: Record<string, unknown>; init?: ResponseInit };
    return { status: wrapper.init?.status ?? 200, body: wrapper.data };
  }
  throw result;
}

/** The review page's Approve — `params` decide the mount: `server` is the MCP base, `skill` the
 * skill base, exactly as the two route mounts hand them over. */
async function approveOn(
  params: Record<string, string>,
  versionId: string,
  expected = "1",
): Promise<ActionResult> {
  const { action } = await import("@/routes/proposal-review");
  return await drive(
    action as unknown as RouteAction,
    { ...params, versionId },
    {
      intent: "approve",
      version_id: versionId,
      expected_generation: expected,
    },
  );
}

/** The history page's per-row roll-back. */
async function rollBackOn(
  params: Record<string, string>,
  good: string,
  expected = "1",
): Promise<ActionResult> {
  const { action } = await import("@/routes/skill-history");
  return await drive(action as unknown as RouteAction, params, {
    intent: "revert",
    good_version_id: good,
    expected_generation: expected,
  });
}

const approve = (serverName: string, versionId: string, expected = "1") =>
  approveOn({ server: serverName }, versionId, expected);

const rollBack = (serverName: string, good: string, expected = "1") =>
  rollBackOn({ server: serverName }, good, expected);

/** An mcp bundle already published here: the catalog row, its custody rows, the document the
 * vault serves for its `current`, and the pointer state a pre-move read answers. */
async function seedPublishedServer(
  bundleId: string,
  name: string,
  document: unknown,
): Promise<void> {
  const versionId = versionIdFor(bundleId);
  await seedBundle(db, wsId, bundleId, name, { kind: "mcp", versionId });
  vault.seed(wsId, bundleId, versionId, [
    { path: "server.json", content: JSON.stringify(document, null, 2) },
  ]);
  vault.point(wsId, bundleId, versionId, 1);
  // A seeded server stands as if published, so the name it serves is on record — which is what
  // a later approve or roll-back is checked against.
  await recordSeededIdentities();
}

/** A candidate version standing in the vault, pointed at by nothing — a proposal's bytes. */
function seedCandidate(bundleId: string, versionId: string, document: unknown): void {
  vault.seed(wsId, bundleId, versionId, [
    { path: "server.json", content: JSON.stringify(document, null, 2) },
  ]);
}

async function openProposal(
  id: string,
  bundleId: string,
  candidateVersionId: string,
): Promise<void> {
  await db.q(
    `INSERT INTO web.proposal (id, workspace_id, bundle_id, candidate_version_id, proposed_by, status)
     VALUES ($1, $2, $3, $4, 'u_mem', 'open')`,
    [id, wsId, bundleId, candidateVersionId],
  );
}

const proposalStatus = async (id: string): Promise<string | undefined> =>
  (await db.q<{ status: string }>(`SELECT status FROM web.proposal WHERE id = $1`, [id]))[0]
    ?.status;

const pointerVersion = async (bundleId: string): Promise<string | undefined> =>
  (
    await db.q<{ version_id: string }>(
      `SELECT version_id FROM plane.current_pointer WHERE workspace_id = $1 AND bundle_id = $2`,
      [wsId, bundleId],
    )
  )[0]?.version_id;

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_mcp_browser", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: vault.url,
  });
  wsId = await bootWorkspace();
  await seedUser(db, "u_rev", "Reviewer", "reviewer@example.com");
  await seatUser(db, wsId, "u_rev", "reviewer");
  // The proposer is somebody else, so four-eyes never enters into it.
  await seedUser(db, "u_mem", "Member", "member@example.com");
  await seatUser(db, wsId, "u_mem", "member");
  session = { user: { id: "u_rev", name: "Reviewer", email: "reviewer@example.com" } };
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

beforeEach(() => {
  vault.calls.length = 0;
  vault.published.length = 0;
});

describe("the browser Approve claims the candidate's embedded name", () => {
  it("refuses a candidate that renames the server onto a name another bundle holds", async () => {
    const HELD = { ...WEATHER, name: "io.github.acme/harbour" };
    const RENAMER = { ...WEATHER, name: "io.github.acme/estuary" };
    await seedPublishedServer("s_harbour", "harbour", HELD);
    await seedPublishedServer("s_renamer", "estuary", RENAMER);
    const candidate = "1a".repeat(32);
    seedCandidate("s_renamer", candidate, { ...HELD, version: "9.9.9" });
    await openProposal("p_rename", "s_renamer", candidate);

    const { body } = await approve("estuary", candidate);
    expect(body.status).toBe("denied");
    // The page says WHICH bundle holds the name and where it lives — the same words every other
    // door uses, not a bare "declined".
    expect(String(body.message)).toContain("harbour");
    expect(String(body.message)).toContain("/mcp/harbour");
    // Nothing moved: the refusal preceded the custody call, and the proposal still stands open.
    expect(vault.calls).toEqual([]);
    expect(await proposalStatus("p_rename")).toBe("open");
  });

  it("approves a candidate whose embedded name is free", async () => {
    const FREE = { ...WEATHER, name: "io.github.acme/lagoon" };
    await seedPublishedServer("s_lagoon", "lagoon", FREE);
    const candidate = "2b".repeat(32);
    seedCandidate("s_lagoon", candidate, { ...FREE, version: "1.2.0" });
    await openProposal("p_lagoon", "s_lagoon", candidate);

    const { body } = await approve("lagoon", candidate);
    expect(body.status).toBe("approved");
    expect(vault.calls).toEqual([{ route: "pointer", ws: wsId, bundle: "s_lagoon" }]);
    expect(await proposalStatus("p_lagoon")).toBe("approved");
  });

  it("keeps the vault's own answer for a candidate it does not hold", async () => {
    // The name check must not swallow it: a version the vault never held cannot become `current`,
    // so the move gets to say so — a refusal, but not the name-taken one.
    const GONE = { ...WEATHER, name: "io.github.acme/sandbar" };
    await seedPublishedServer("s_sandbar", "sandbar", GONE);
    const candidate = "6f".repeat(32);
    await openProposal("p_sandbar", "s_sandbar", candidate);

    const { body } = await approve("sandbar", candidate);
    expect(body.status).toBe("denied");
    expect(body.message).toBeUndefined();
    expect(vault.calls).toEqual([{ route: "pointer", ws: wsId, bundle: "s_sandbar" }]);
  });

  it("leaves a plain skill's approve exactly as it was", async () => {
    const versionId = versionIdFor("s_skill_prop");
    await seedBundle(db, wsId, "s_skill_prop", "skill-prop", { versionId });
    vault.seed(wsId, "s_skill_prop", versionId, [{ path: "SKILL.md", content: "# v1" }]);
    vault.point(wsId, "s_skill_prop", versionId, 1);
    const candidate = "5e".repeat(32);
    vault.seed(wsId, "s_skill_prop", candidate, [{ path: "SKILL.md", content: "# v2" }]);
    await openProposal("p_skill", "s_skill_prop", candidate);

    const { body } = await approveOn({ skill: "skill-prop" }, candidate);
    expect(body.status).toBe("approved");
    expect(vault.calls).toEqual([{ route: "pointer", ws: wsId, bundle: "s_skill_prop" }]);
  });
});

describe("the browser roll-back claims the name it carries forward", () => {
  it("refuses a roll-back that would carry a taken name forward", async () => {
    const HELD = { ...WEATHER, name: "io.github.acme/jetty" };
    const NOW = { ...WEATHER, name: "io.github.acme/slipway" };
    await seedPublishedServer("s_jetty", "jetty", HELD);
    await seedPublishedServer("s_slipway", "slipway", NOW);
    // The good version predates the rename: rolling back to it re-claims s_jetty's name.
    const good = "3c".repeat(32);
    seedCandidate("s_slipway", good, { ...HELD, version: "0.9.0" });

    const { body } = await rollBack("slipway", good);
    expect(body.status).toBe("denied");
    expect(String(body.reason)).toContain("jetty");
    expect(vault.calls).toEqual([]);
    expect(await pointerVersion("s_slipway")).toBe(versionIdFor("s_slipway"));
    // The attempt is on the record, refusal and all.
    const events = await db.q<{ outcome: string }>(
      `SELECT outcome FROM web.audit_event WHERE kind = 'revert' AND subject = $1`,
      ["s_slipway"],
    );
    expect(events.map((e) => e.outcome)).toEqual(["denied"]);
  });

  it("rolls back to a good version whose embedded name is this bundle's own", async () => {
    const OWN = { ...WEATHER, name: "io.github.acme/quay" };
    await seedPublishedServer("s_quay", "quay", OWN);
    const good = "4d".repeat(32);
    seedCandidate("s_quay", good, { ...OWN, version: "0.9.0" });

    const { body } = await rollBack("quay", good);
    expect(body.status).toBe("reverted");
    expect(vault.calls).toEqual([{ route: "revert", ws: wsId, bundle: "s_quay" }]);
  });

  it("keeps the vault's own answer for a good version it does not hold", async () => {
    const SHOAL = { ...WEATHER, name: "io.github.acme/shoal" };
    await seedPublishedServer("s_shoal", "shoal", SHOAL);

    const { body } = await rollBack("shoal", "7a".repeat(32));
    expect(body.status).toBe("denied");
    expect(String(body.reason)).toContain("no version with this id");
    expect(vault.calls).toEqual([{ route: "revert", ws: wsId, bundle: "s_shoal" }]);
  });

  it("leaves a plain skill's roll-back exactly as it was", async () => {
    const versionId = versionIdFor("s_skill_hist");
    await seedBundle(db, wsId, "s_skill_hist", "skill-hist", { versionId });
    vault.seed(wsId, "s_skill_hist", versionId, [{ path: "SKILL.md", content: "# v2" }]);
    const good = "8b".repeat(32);
    vault.seed(wsId, "s_skill_hist", good, [{ path: "SKILL.md", content: "# v1" }]);

    const { body } = await rollBackOn({ skill: "skill-hist" }, good);
    expect(body.status).toBe("reverted");
    expect(vault.calls).toEqual([{ route: "revert", ws: wsId, bundle: "s_skill_hist" }]);
  });
});
