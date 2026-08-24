import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import type { CustodyLog, CustodyLogEntry } from "@/lib/plane/wire";
import { laneHeaders } from "./helpers/lane";
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
 * `GET /api/v1/workspaces/{ws}/skills/{skill}/log` — what `topos log` reads back, and the two
 * things it could not say about a version.
 *
 * WHEN: the custody row has always recorded a version's commit time; this lane dropped it, so
 * every version entry reached the client timeless and the human log printed them under whatever
 * date the line above happened to carry.
 *
 * WHO: a version's author is part of its identity — the client derives the version id from
 * `(parents, tree, author, message)` and verifies the server landed exactly that id — so the
 * commit frame carries the MACHINE's `d_…` id and can never be rewritten into a name. The session
 * that sent the candidate does know the person, so publishing records the pairing and the read
 * resolves it. A machine this workspace has never seen publish keeps the id it signed with.
 */

const SESSION = "sn_log_lane";
const BUNDLE = "s_loglane";
const MINE = `d_${"a".repeat(32)}`;
const STRANGER = `d_${"b".repeat(32)}`;
const V1 = "1".repeat(64);
const V2 = "2".repeat(64);
const V2_AT = 1_700_086_400_000;
const V1_AT = 1_700_000_000_000;
const PERSON = "Robert <robert@topos.sh>";

let db: ScratchDb;
let vault: StubVault;
let wsId = "";
let served: CustodyLogEntry[] = [];

vi.mock("@/lib/plane/reads.server", () => ({
  custodyLog: async (): Promise<{ ok: true; data: CustodyLog }> => ({
    ok: true,
    data: { versions: served },
  }),
}));

interface LogBody {
  versions: { version_id: string; at?: number; author: string }[];
}

/** The route's loader, called the way every other lane suite calls one. */
type RouteFn = (args: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<Response>;

async function readLog(): Promise<LogBody> {
  const { loader } = await import("@/routes/api.v1.skill-log");
  const request = new Request(
    `http://localhost:3000/api/v1/workspaces/${wsId}/skills/${BUNDLE}/log`,
    { headers: laneHeaders({ authorization: `Bearer ${SESSION}` }) },
  );
  const response = await (loader as unknown as RouteFn)({
    request,
    params: { ws: wsId, skill: BUNDLE },
  });
  expect(response.status).toBe(200);
  return (await response.json()) as LogBody;
}

/** One publish over the session lane, signed the way a real client signs it: with its machine id. */
async function publishAs(author: string, bundleId: string): Promise<void> {
  const { publishFlow } = await import("@/lib/api/publish-flow.server");
  const raw = JSON.stringify({ skill_id: bundleId, author });
  await publishFlow({
    actor: asSession(wsId, "u_log", SESSION, "owner"),
    raw,
    opId: crypto.randomUUID(),
    skillId: bundleId,
    expected: 0,
    candidate: {
      files: [{ path: "SKILL.md", mode: "100644", content_base64: "IyBs" }],
      parents: [],
      author,
      message: "genesis",
    },
    displayName: "Logbook",
    channel: null,
    kind: null,
    command: "publish",
    forceProposal: false,
  });
}

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_loglane", {
    TOPOS_WEB_RATELIMIT: "off",
    PLANE_INTERNAL_URL: vault.url,
  });
  wsId = await bootWorkspace();
  await seedUser(db, "u_log", "Robert", "robert@topos.sh");
  await seatUser(db, wsId, "u_log", "owner");
  await seedSession(db, SESSION, wsId, "u_log");
  await seedBundle(db, wsId, BUNDLE, "logbook", { withPointer: false });
  served = [
    {
      version_id: V2,
      message: "logbook v2",
      author_display: MINE,
      created_at_ms: V2_AT,
    },
    {
      version_id: V1,
      message: "logbook v1",
      author_display: STRANGER,
      created_at_ms: V1_AT,
    },
  ];
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

describe("the version entries a machine reads back", () => {
  it("carries each version's commit time as `at`, the field a pull event already uses", async () => {
    const body = await readLog();
    expect(body.versions.map((v) => v.at)).toEqual([V2_AT, V1_AT]);
  });

  it("keeps the custody order — newest first, the head being `current`", async () => {
    const body = await readLog();
    expect(body.versions.map((v) => v.version_id)).toEqual([V2, V1]);
  });
});

describe("the author a version is read back with", () => {
  it("is the raw machine id until that machine has published here", async () => {
    const body = await readLog();
    expect(body.versions.map((v) => v.author)).toEqual([MINE, STRANGER]);
  });

  it("becomes the person once the machine publishes — the recorded frame untouched", async () => {
    await publishAs(MINE, "s_logpub");
    // What the vault was told is still the MACHINE id: substituting a display string would
    // derive a different version id and the client would refuse the answer.
    expect(vault.published.at(-1)?.bundle).toBe("s_logpub");
    const pairing = await db.q<{ user_id: string }>(
      `SELECT user_id FROM web.device_owner WHERE workspace_id = $1 AND device_id = $2`,
      [wsId, MINE],
    );
    expect(pairing.map((r) => r.user_id)).toEqual(["u_log"]);

    const body = await readLog();
    expect(body.versions.map((v) => v.author)).toEqual([PERSON, STRANGER]);
  });
});
