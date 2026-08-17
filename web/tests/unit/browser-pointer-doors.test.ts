import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
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
 * version they point at becomes `current` — and they are driven here through the REAL route
 * actions with a reviewer's cookie session, a real scratch Postgres, and the app's one custody
 * transport re-pointed at an in-process stub vault, so what reached custody is proven by the
 * recorder rather than assumed.
 *
 * The vault's own answers are the interesting half: a version it does not hold must come back as
 * ITS refusal, in its words, rather than as anything this tier invented on the way past.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let vault: StubVault;
let wsId = "";

const ORIGIN = "http://x";

const _WEATHER = {
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

/** A candidate version standing in the vault, pointed at by nothing — a proposal's bytes. */
function _seedCandidate(bundleId: string, versionId: string, document: unknown): void {
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

const _pointerVersion = async (bundleId: string): Promise<string | undefined> =>
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

describe("the browser Approve", () => {
  it("moves the pointer onto the candidate and resolves the proposal", async () => {
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
    expect(await proposalStatus("p_skill")).toBe("approved");
  });

  it("keeps the vault's own answer for a candidate it does not hold", async () => {
    const versionId = versionIdFor("s_skill_gone");
    await seedBundle(db, wsId, "s_skill_gone", "skill-gone", { versionId });
    vault.seed(wsId, "s_skill_gone", versionId, [{ path: "SKILL.md", content: "# v1" }]);
    vault.point(wsId, "s_skill_gone", versionId, 1);
    const candidate = "6f".repeat(32);
    await openProposal("p_gone", "s_skill_gone", candidate);

    const { body } = await approveOn({ skill: "skill-gone" }, candidate);
    expect(body.status).toBe("denied");
    expect(body.message).toBeUndefined();
    expect(vault.calls).toEqual([{ route: "pointer", ws: wsId, bundle: "s_skill_gone" }]);
    expect(await proposalStatus("p_gone")).toBe("open");
  });
});

describe("the browser roll-back", () => {
  it("carries a good version forward onto current", async () => {
    const versionId = versionIdFor("s_skill_hist");
    await seedBundle(db, wsId, "s_skill_hist", "skill-hist", { versionId });
    vault.seed(wsId, "s_skill_hist", versionId, [{ path: "SKILL.md", content: "# v2" }]);
    const good = "8b".repeat(32);
    vault.seed(wsId, "s_skill_hist", good, [{ path: "SKILL.md", content: "# v1" }]);

    const { body } = await rollBackOn({ skill: "skill-hist" }, good);
    expect(body.status).toBe("reverted");
    expect(vault.calls).toEqual([{ route: "revert", ws: wsId, bundle: "s_skill_hist" }]);
  });

  it("keeps the vault's own answer for a good version it does not hold", async () => {
    const versionId = versionIdFor("s_skill_shoal");
    await seedBundle(db, wsId, "s_skill_shoal", "skill-shoal", { versionId });
    vault.seed(wsId, "s_skill_shoal", versionId, [{ path: "SKILL.md", content: "# v1" }]);
    vault.point(wsId, "s_skill_shoal", versionId, 1);

    const { body } = await rollBackOn({ skill: "skill-shoal" }, "7a".repeat(32));
    expect(body.status).toBe("denied");
    expect(String(body.reason)).toContain("no version with this id");
    expect(vault.calls).toEqual([{ route: "revert", ws: wsId, bundle: "s_skill_shoal" }]);
    // The attempt is on the record, refusal and all.
    const events = await db.q<{ outcome: string }>(
      `SELECT outcome FROM web.audit_event WHERE kind = 'revert' AND subject = $1`,
      ["s_skill_shoal"],
    );
    expect(events.map((e) => e.outcome)).toEqual(["denied"]);
  });
});
