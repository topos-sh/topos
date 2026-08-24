import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import type { CustodyLog, CustodyLogEntry } from "@/lib/plane/wire";
import { laneHeaders } from "./helpers/lane";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * `GET /api/v1/workspaces/{ws}/skills/{skill}/log` — what the CLI's `topos log` reads.
 *
 * The custody rows have always recorded WHEN each version was committed; this lane dropped the
 * field on the way out, so every version entry reached the client timeless and `topos log`
 * printed them under whatever date the line above happened to carry.
 */

const SESSION = "sn_log_lane";
const BUNDLE = "s_loglane";
const V1 = "1".repeat(64);
const V2 = "2".repeat(64);
const V2_AT = 1_700_086_400_000;
const V1_AT = 1_700_000_000_000;

let db: ScratchDb;
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

beforeAll(async () => {
  db = await createScratchDb("web_loglane", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
  await seedUser(db, "u_log", "Robert", "robert@topos.sh");
  await seatUser(db, wsId, "u_log", "owner");
  await seedSession(db, SESSION, wsId, "u_log");
  await seedBundle(db, wsId, BUNDLE, "logbook", { withPointer: false });
  served = [
    {
      version_id: V2,
      message: "logbook v2",
      author_display: "Robert <robert@topos.sh>",
      created_at_ms: V2_AT,
    },
    {
      version_id: V1,
      message: "logbook v1",
      author_display: "Robert <robert@topos.sh>",
      created_at_ms: V1_AT,
    },
  ];
}, 60000);

afterAll(async () => {
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
