import { Buffer } from "node:buffer";
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
/** A SECOND person, on the very same machine — the laptop that changed hands. */
const SESSION_B = "sn_log_lane_b";
const BUNDLE = "s_loglane";
const MINE = `d_${"a".repeat(32)}`;
const STRANGER = `d_${"b".repeat(32)}`;
const V1 = "1".repeat(64);
const V2 = "2".repeat(64);
const V2_AT = 1_700_086_400_000;
const V1_AT = 1_700_000_000_000;
const PERSON = "Robert <robert@topos.sh>";
const PERSON_B = "Mia <mia@topos.sh>";

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

/**
 * One publish over the session lane, signed the way a real client signs it: with its MACHINE id,
 * by a named person. Answers the version the vault minted, or null when the op was refused.
 */
async function publishAs(args: {
  author: string;
  bundleId: string;
  userId: string;
  sessionId: string;
  expected?: number;
}): Promise<string | null> {
  const { publishFlow } = await import("@/lib/api/publish-flow.server");
  const before = vault.published.length;
  const raw = JSON.stringify({ skill_id: args.bundleId, author: args.author, n: before });
  const response = await publishFlow({
    actor: asSession(wsId, args.userId, args.sessionId, "owner"),
    raw,
    opId: crypto.randomUUID(),
    skillId: args.bundleId,
    expected: args.expected ?? 0,
    candidate: {
      files: [
        { path: "SKILL.md", mode: "100644", content_base64: Buffer.from(raw).toString("base64") },
      ],
      parents: [],
      author: args.author,
      message: "genesis",
    },
    displayName: "Logbook",
    channel: null,
    kind: null,
    command: "publish",
    forceProposal: false,
  });
  const envelope = (await response.json()) as {
    ok?: boolean;
    receipt?: { outcome?: string };
    data?: { record?: { version_id?: string } };
  };
  // The version the POINTER now names — an op that did not land names none.
  return envelope.ok === true && envelope.receipt?.outcome === "OK"
    ? (envelope.data?.record?.version_id ?? null)
    : null;
}

/** Every version this app has recorded an author for, as (version, person) pairs. */
async function authoredRows(): Promise<[string, string][]> {
  const rows = await db.q<{ version_id: string; user_id: string }>(
    `SELECT version_id, user_id FROM web.version_author WHERE workspace_id = $1
     ORDER BY version_id`,
    [wsId],
  );
  return rows.map((r) => [r.version_id, r.user_id]);
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
  await seedUser(db, "u_mia", "Mia", "mia@topos.sh");
  await seatUser(db, wsId, "u_mia", "owner");
  await seedSession(db, SESSION_B, wsId, "u_mia");
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
  it("is the raw machine id until this app has recorded who published it", async () => {
    const body = await readLog();
    expect(body.versions.map((v) => v.author)).toEqual([MINE, STRANGER]);
  });

  it("becomes the person who published it — the recorded frame untouched", async () => {
    const versionId = await publishAs({
      author: MINE,
      bundleId: BUNDLE,
      userId: "u_log",
      sessionId: SESSION,
    });
    expect(versionId).not.toBeNull();
    // What the vault was told is still the MACHINE id: substituting a display string would
    // derive a different version id and the client would refuse the answer.
    expect(vault.published.at(-1)?.files[0]?.content).toContain(MINE);
    expect(await authoredRows()).toContainEqual([versionId, "u_log"]);

    served = [
      { version_id: versionId as string, message: "v", author_display: MINE, created_at_ms: V2_AT },
    ];
    const body = await readLog();
    expect(body.versions.map((v) => v.author)).toEqual([PERSON]);
  });

  it("stays with the person who published it after the machine changes hands", async () => {
    // THE BUG THIS PINS: authorship keyed on the machine let a later sign-in relabel every
    // version that machine had ever published. It is keyed on the VERSION.
    const mine = await publishAs({
      author: MINE,
      bundleId: "s_handover",
      userId: "u_log",
      sessionId: SESSION,
    });
    const hers = await publishAs({
      author: MINE,
      bundleId: "s_handover",
      userId: "u_mia",
      sessionId: SESSION_B,
      expected: 1,
    });
    expect(mine).not.toBeNull();
    expect(hers).not.toBeNull();
    expect(hers).not.toBe(mine);

    const { loader } = await import("@/routes/api.v1.skill-log");
    served = [
      { version_id: hers as string, message: "hers", author_display: MINE, created_at_ms: V2_AT },
      { version_id: mine as string, message: "mine", author_display: MINE, created_at_ms: V1_AT },
    ];
    const response = await (loader as unknown as RouteFn)({
      request: new Request(
        `http://localhost:3000/api/v1/workspaces/${wsId}/skills/s_handover/log`,
        { headers: laneHeaders({ authorization: `Bearer ${SESSION}` }) },
      ),
      params: { ws: wsId, skill: "s_handover" },
    });
    const body = (await response.json()) as LogBody;
    expect(body.versions.map((v) => v.author)).toEqual([PERSON_B, PERSON]);
  });

  it("records nothing for a publish the gate refused — no vault call, no author row", async () => {
    // A kind whose bundles are not files refuses ahead of every custody call.
    await seedBundle(db, wsId, "s_denied_kind", "denied-kind", {
      kind: "mcp",
      withPointer: false,
    });
    const before = vault.published.length;
    const outcome = await publishAs({
      author: MINE,
      bundleId: "s_denied_kind",
      userId: "u_log",
      sessionId: SESSION,
    });
    expect(outcome).toBeNull();
    expect(vault.published.length).toBe(before);
    expect(
      await db.q(`SELECT 1 FROM web.version_author WHERE bundle_id = $1`, ["s_denied_kind"]),
    ).toEqual([]);
  });

  it("records nothing for a publish the VAULT refused — the write is what earns the row", async () => {
    // The op reaches custody and is refused there (a lost pointer CAS). Nothing landed, so
    // nothing is authored: the row rides the accepted write's own transaction.
    await seedBundle(db, wsId, "s_conflict", "conflicted", { withPointer: false });
    vault.conflictNextPublish(7);
    const outcome = await publishAs({
      author: MINE,
      bundleId: "s_conflict",
      userId: "u_log",
      sessionId: SESSION,
    });
    expect(outcome).toBeNull();
    expect(
      await db.q(`SELECT 1 FROM web.version_author WHERE bundle_id = $1`, ["s_conflict"]),
    ).toEqual([]);
  });
});

describe("a version written before this app recorded authors", () => {
  const OLD = "9".repeat(64);
  const OLD_DEVICE = `d_${"c".repeat(32)}`;

  /** Read the log of a bundle whose only version has no author row at all. */
  async function readOldLog(): Promise<LogBody> {
    const { loader } = await import("@/routes/api.v1.skill-log");
    served = [
      { version_id: OLD, message: "old", author_display: OLD_DEVICE, created_at_ms: V1_AT },
    ];
    const response = await (loader as unknown as RouteFn)({
      request: new Request(`http://localhost:3000/api/v1/workspaces/${wsId}/skills/s_old/log`, {
        headers: laneHeaders({ authorization: `Bearer ${SESSION}` }),
      }),
      params: { ws: wsId, skill: "s_old" },
    });
    return (await response.json()) as LogBody;
  }

  beforeAll(async () => {
    await seedBundle(db, wsId, "s_old", "oldbook", { withPointer: false });
  });

  it("shows the machine id while nothing at all names that machine", async () => {
    expect((await readOldLog()).versions.map((v) => v.author)).toEqual([OLD_DEVICE]);
  });

  it("falls back to the person when the machine has published as exactly one", async () => {
    await db.q(
      `INSERT INTO web.device_owner (workspace_id, device_id, user_id) VALUES ($1, $2, $3)`,
      [wsId, OLD_DEVICE, "u_log"],
    );
    expect((await readOldLog()).versions.map((v) => v.author)).toEqual([PERSON]);
  });

  it("goes back to the machine id once that machine has published as two people", async () => {
    // Two people, one laptop: guessing between them is worse than showing the machine.
    await db.q(
      `INSERT INTO web.device_owner (workspace_id, device_id, user_id) VALUES ($1, $2, $3)`,
      [wsId, OLD_DEVICE, "u_mia"],
    );
    expect((await readOldLog()).versions.map((v) => v.author)).toEqual([OLD_DEVICE]);
  });
});
