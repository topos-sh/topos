import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * ADDRESSING A VERSION BY WHAT THE PRODUCT ITSELF PRINTS.
 *
 * Every id a person can copy is the 12-hex SHORT form — the link text on a version page, the
 * History rows, `topos log`, every CLI receipt — but the version routes matched only the full
 * 64-hex id, so `/…/versions/da9fa16db64b` 404'd while the same page under the long id opened.
 * The rule is git's: a full id, or a unique prefix of at least eight hex characters; an ambiguous
 * prefix is the uniform 404 rather than a coin flip between two releases.
 *
 * Driven through the REAL loaders against a scratch Postgres (the `plane.version` mirror is what
 * a prefix resolves against) with the app's custody transport re-pointed at an in-process stub
 * vault, so a resolved prefix is proven by the version the page actually renders.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let vault: StubVault;
let wsId = "";

const ORIGIN = "http://x";
const BUNDLE = "s_versioned";
const NAME = "r2-smoke";

/** Two versions sharing the first eight characters, and one that shares nothing. */
const TWIN_A = `da9fa16d${"1".repeat(56)}`;
const TWIN_B = `da9fa16d${"2".repeat(56)}`;
const LONE = `beef0042${"c".repeat(56)}`;

type Loader = (args: {
  request: Request;
  params: Record<string, string | undefined>;
  context: unknown;
}) => Promise<unknown>;

/** Run a loader, handing back either its data or the Response a guard threw. */
async function load(
  loader: Loader,
  params: Record<string, string | undefined>,
  path: string,
): Promise<{ data?: Record<string, unknown>; status?: number }> {
  try {
    const data = (await loader({
      request: new Request(`${ORIGIN}${path}`),
      params,
      context: {},
    })) as Record<string, unknown>;
    return { data };
  } catch (thrown) {
    // A guard throws either a Response or the framework's `data(null, { status })` wrapper —
    // both are the uniform miss, read here through one shape.
    if (thrown instanceof Response) {
      return { status: thrown.status };
    }
    if (typeof thrown === "object" && thrown !== null && "init" in thrown) {
      const status = (thrown as { init?: ResponseInit }).init?.status;
      if (typeof status === "number") {
        return { status };
      }
    }
    throw thrown;
  }
}

async function versionPage(
  typed: string,
): Promise<{ data?: Record<string, unknown>; status?: number }> {
  const { loader } = await import("@/routes/version-files");
  return await load(
    loader as unknown as Loader,
    { skill: NAME, versionId: typed },
    `/skills/${NAME}/versions/${typed}`,
  );
}

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_versionprefix", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: vault.url,
  });
  wsId = await bootWorkspace();
  await seedUser(db, "u_mem", "Member", "member@example.com");
  await seatUser(db, wsId, "u_mem", "member");
  session = { user: { id: "u_mem", name: "Member", email: "member@example.com" } };

  await seedBundle(db, wsId, BUNDLE, NAME, { versionId: LONE });
  for (const versionId of [TWIN_A, TWIN_B]) {
    await db.q(
      `INSERT INTO plane.version (workspace_id, bundle_id, version_id, commit_id, author_display)
       VALUES ($1, $2, $3, $3, 'seed')`,
      [wsId, BUNDLE, versionId],
    );
  }
  for (const versionId of [LONE, TWIN_A, TWIN_B]) {
    vault.seed(wsId, BUNDLE, versionId, [{ path: "SKILL.md", content: `# ${versionId}\n` }]);
  }
  vault.point(wsId, BUNDLE, LONE, 1);
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

describe("the version page addresses a version the way git addresses an object", () => {
  it("opens on the 12-hex SHORT id the page itself prints", async () => {
    const { data, status } = await versionPage(LONE.slice(0, 12));
    expect(status).toBeUndefined();
    expect(data?.versionId).toBe(LONE);
    // The page really read THAT version's bytes, not merely echoed the prefix back.
    const files = data?.versionFiles as { version: { version_id: string } | null };
    expect(files.version?.version_id).toBe(LONE);
  });

  it("still opens on the full 64-hex id", async () => {
    const { data, status } = await versionPage(LONE);
    expect(status).toBeUndefined();
    expect(data?.versionId).toBe(LONE);
  });

  it("answers the uniform 404 for a prefix two versions share", async () => {
    // `da9fa16d` names TWIN_A and TWIN_B equally — picking one would send a reader to a release
    // they did not ask for.
    expect(await versionPage("da9fa16d")).toEqual({ status: 404 });
    // One character further still lands on both.
    expect(await versionPage(TWIN_A.slice(0, 8))).toEqual({ status: 404 });
    // …and the character that tells them apart resolves.
    const { data } = await versionPage(TWIN_A.slice(0, 9));
    expect(data?.versionId).toBe(TWIN_A);
  });

  it("answers the uniform 404 for a prefix nothing matches and for one too short to be an id", async () => {
    expect(await versionPage(`0123abcd${"f".repeat(4)}`)).toEqual({ status: 404 });
    expect(await versionPage("da9fa16")).toEqual({ status: 404 });
    expect(await versionPage("not-hex-at-all")).toEqual({ status: 404 });
  });

  it("carries the same rule into the file view under it", async () => {
    const { loader } = await import("@/routes/file-view");
    const short = LONE.slice(0, 12);
    const { data, status } = await load(
      loader as unknown as Loader,
      { skill: NAME, versionId: short, "*": "SKILL.md" },
      `/skills/${NAME}/versions/${short}/files/SKILL.md`,
    );
    expect(status).toBeUndefined();
    expect(data?.versionId).toBe(LONE);

    expect(
      await load(
        loader as unknown as Loader,
        { skill: NAME, versionId: "da9fa16d", "*": "SKILL.md" },
        `/skills/${NAME}/versions/da9fa16d/files/SKILL.md`,
      ),
    ).toEqual({ status: 404 });
  });
});
