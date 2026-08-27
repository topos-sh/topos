import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { mcpRevisionId } from "../helpers/mcp-ids";
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

/**
 * THE BY-REVISION READ — `GET /api/v1/workspaces/{ws}/mcp-servers/{bundle}/revisions/{revision}`,
 * the lane a committed `topos.lock` converges an MCP entry through. The suite runs the REAL loader
 * against a real scratch Postgres.
 *
 * What it pins: both doors of a read lane (a person's session bearer and a machine token), that
 * the answer is BYTE-IDENTICAL to what the catalog index served while that revision was current
 * (the whole promise of the lane — an old lock installs the document a teammate received then),
 * and that every miss is the ONE uniform 404: no credential, no seat, an unconnected bundle, a
 * revision belonging to another server, and a proposal nobody was ever delivered.
 */

let db: ScratchDb;
let wsId = "";
let tokenSecret = "";

const ORIGIN = "http://x";
const WEATHER_1 = mcpRevisionId("rev_lane_weather_1");
const WEATHER_2 = mcpRevisionId("rev_lane_weather_2");
const WEATHER_PROPOSAL = mcpRevisionId("rev_lane_weather_3");
const TIDES_1 = mcpRevisionId("rev_lane_tides_1");
const LOOSE_1 = mcpRevisionId("rev_lane_loose_1");
const REQUIRED_1 = mcpRevisionId("rev_lane_required_1");

function document(name: string, version: string, url: string): Record<string, unknown> {
  return {
    name,
    description: `Everything about ${name}.`,
    version,
    remotes: [{ type: "streamable-http", url }],
  };
}

const WEATHER_OLD = document("io.github.acme/weather", "1.0.0", "https://weather.acme.example/mcp");
const WEATHER_NEW = document("io.github.acme/weather", "1.4.0", "https://weather.acme.example/mcp");
const WEATHER_NEXT = document(
  "io.github.acme/weather",
  "2.0.0",
  "https://weather.acme.example/v2/mcp",
);
const TIDES = document("io.github.acme/tides", "0.2.0", "https://tides.acme.example/mcp");
const LOOSE = document("io.github.acme/loose", "1.0.0", "https://loose.acme.example/mcp");
const REQUIRED = document("io.github.acme/required", "1.0.0", "https://req.acme.example/mcp");

async function get(
  bundleId: string,
  revisionId: string,
  headers: Record<string, string> = {},
): Promise<Response> {
  const { loader } = await import("@/routes/api.v1.mcp-revision");
  const path = `${ORIGIN}/api/v1/workspaces/${wsId}/mcp-servers/${bundleId}/revisions/${revisionId}`;
  try {
    return await loader({
      request: new Request(path, { headers: laneHeaders(headers) }),
      params: { ws: wsId, skill: bundleId, revisionId },
      context: {},
    } as unknown as Parameters<typeof loader>[0]);
  } catch (thrown) {
    if (thrown instanceof Response) {
      return thrown;
    }
    throw thrown;
  }
}

async function seedServer(id: string, name: string, authMode: string | null): Promise<void> {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, name, display_name, auth_mode, status)
     VALUES ($1, NULL, $2, $2, $3, 'active')`,
    [id, name, authMode],
  );
}

/** A revision that HAS been on offer carries the promotion stamp; a proposal carries none. */
async function seedRevision(
  serverId: string,
  id: string,
  seq: number,
  doc: Record<string, unknown>,
  opts: { current?: boolean; promoted?: boolean } = {},
): Promise<void> {
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, upstream_version, document, transport, url, published_at, published_by)
     VALUES ($1, $2, $3, $4, $5::jsonb, 'streamable-http', 'https://x.example/mcp',
             CASE WHEN $6 THEN now() END, CASE WHEN $6 THEN 'Staff' END)`,
    [id, serverId, seq, String(doc.version), JSON.stringify(doc), opts.promoted !== false],
  );
  if (opts.current === true) {
    await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [serverId, id]);
  }
}

async function connect(bundleId: string, serverId: string): Promise<void> {
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id) VALUES ($1, $2, $3)`,
    [bundleId, wsId, serverId],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_revision");
  wsId = await bootWorkspace();
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seatUser(db, wsId, "u_mem", "member");
  await seedSession(db, "sn_mem", wsId, "u_mem");
  const { mintMachineToken } = await import("@/lib/db/queries.tokens.server");
  tokenSecret = (await mintMachineToken(wsId, "ci", { userId: "u_mem", display: "Member" })).secret;

  // The connected server, three revisions deep: two that were on offer, one still a proposal.
  await seedServer("mcps_weather", "io.github.acme/weather", "oauth");
  await seedRevision("mcps_weather", WEATHER_1, 1, WEATHER_OLD);
  await seedRevision("mcps_weather", WEATHER_2, 2, WEATHER_NEW, { current: true });
  await seedRevision("mcps_weather", WEATHER_PROPOSAL, 3, WEATHER_NEXT, { promoted: false });
  await seedBundle(db, wsId, "s_weather", "weather", { kind: "mcp", withPointer: false });
  await connect("s_weather", "mcps_weather");

  // A second connected server — its revision must not answer under the first one's bundle.
  await seedServer("mcps_tides", "io.github.acme/tides", "none");
  await seedRevision("mcps_tides", TIDES_1, 1, TIDES, { current: true });
  await seedBundle(db, wsId, "s_tides", "tides", { kind: "mcp", withPointer: false });
  await connect("s_tides", "mcps_tides");

  // A connection the workspace MANDATED through the gateway, on a deployment running none —
  // the withhold, which this lane must answer rather than miss.
  await seedServer("mcps_req", "io.github.acme/required", "none");
  await seedRevision("mcps_req", REQUIRED_1, 1, REQUIRED, { current: true });
  await seedBundle(db, wsId, "s_req", "required-server", { kind: "mcp", withPointer: false });
  await connect("s_req", "mcps_req");
  await db.q(`UPDATE web.bundle_mcp SET gateway_policy = 'required' WHERE bundle_id = 's_req'`);

  // A catalog server nobody here connected, and a plain skill: neither is reachable by this lane.
  await seedServer("mcps_loose", "io.github.acme/loose", "none");
  await seedRevision("mcps_loose", LOOSE_1, 1, LOOSE, { current: true });
  await seedBundle(db, wsId, "s_skill", "runbook", { kind: "skill" });
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the door", () => {
  it("a session bearer reads the revision the lock names", async () => {
    const res = await get("s_weather", WEATHER_1, { authorization: "Bearer sn_mem" });
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as {
      skill_id: string;
      name: string;
      kind: string;
      revision_id: string;
      document: Record<string, unknown>;
    };
    expect(body.skill_id).toBe("s_weather");
    expect(body.name).toBe("weather");
    expect(body.kind).toBe("mcp");
    expect(body.revision_id).toBe(WEATHER_1);
    expect(body.document).toEqual(WEATHER_OLD);
  });

  it("a machine token reads it too — the same bytes, the same door", async () => {
    const person = await get("s_weather", WEATHER_1, { authorization: "Bearer sn_mem" });
    const machine = await get("s_weather", WEATHER_1, {
      authorization: `Bearer ${tokenSecret}`,
      "x-topos-machine": "ci-runner",
    });
    expect(machine.status).toBe(200);
    expect(await machine.text()).toBe(await person.text());
  });

  it("no credential, and an unknown one, are the same uniform 404", async () => {
    const bare = await get("s_weather", WEATHER_1);
    expect(bare.status).toBe(404);
    const body = (await bare.json()) as { error: { code: string } };
    expect(body.error.code).toBe("NOT_FOUND");
    const unknown = await get("s_weather", WEATHER_1, { authorization: "Bearer nope" });
    expect(unknown.status).toBe(404);
  });

  it("a non-GET method on the path is the same uniform miss", async () => {
    const { action } = await import("@/routes/api.v1.mcp-revision");
    expect(action().status).toBe(404);
  });
});

describe("what it answers with", () => {
  const bearer = { authorization: "Bearer sn_mem" };

  it("is byte-identical to what the catalog index serves while that revision is current", async () => {
    const { laneMcpServersIndex } = await import("@/lib/db/queries.lane.server");
    // The SAME caller the route resolved from the bearer above: the catalog index routes per
    // person and per session, so a comparison against it has to be asked as that caller.
    const index = await laneMcpServersIndex(asSession(wsId, "u_mem", "sn_mem"));
    const current = index.find((row) => row.skill_id === "s_weather");
    const res = await get("s_weather", WEATHER_2, bearer);
    expect(res.status).toBe(200);
    expect(await res.json()).toEqual(JSON.parse(JSON.stringify(current)));
  });

  it("serves the OLD revision's own document, never the current one", async () => {
    const res = await get("s_weather", WEATHER_1, bearer);
    const body = (await res.json()) as { document: Record<string, unknown> };
    expect(body.document).toEqual(WEATHER_OLD);
    expect(body.document).not.toEqual(WEATHER_NEW);
  });
});

describe("a withheld connection, over the wire", () => {
  const bearer = { authorization: "Bearer sn_mem" };

  it("answers 200 with the reason and NO document key at all", async () => {
    const res = await get("s_req", REQUIRED_1, bearer);
    // Not the 404. A client reads this lane's 404 as "could not reach it" and keeps the entry it
    // already has — which on a `required` connection is the bypass the mandate exists to close.
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const raw = await res.text();
    const body = JSON.parse(raw) as Record<string, unknown>;
    expect(body.withheld).toBe("gateway_required");
    expect(body.skill_id).toBe("s_req");
    expect(body.name).toBe("required-server");
    expect(body.kind).toBe("mcp");
    expect(body.status).toBe("active");
    expect(body.revision_id).toBe(REQUIRED_1);
    expect(typeof body.updated_at).toBe("number");
    // THE BYTES: the key is absent, never `"document": null` — the shape the Rust wire type
    // deserializes as `None`, and the one this side agreed to emit.
    expect(Object.hasOwn(body, "document")).toBe(false);
    expect(raw).not.toContain('"document"');
  });

  it("says the same to a machine token", async () => {
    const res = await get("s_req", REQUIRED_1, {
      authorization: `Bearer ${tokenSecret}`,
      "x-topos-machine": "ci-runner",
    });
    expect(res.status).toBe(200);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body.withheld).toBe("gateway_required");
    expect(Object.hasOwn(body, "document")).toBe(false);
  });

  it("carries no `withheld` key on a served row", async () => {
    const res = await get("s_weather", WEATHER_1, bearer);
    const body = (await res.json()) as Record<string, unknown>;
    expect(Object.hasOwn(body, "withheld")).toBe(false);
    expect(body.document).toBeDefined();
  });
});

describe("the misses", () => {
  const bearer = { authorization: "Bearer sn_mem" };

  it("an unknown revision id", async () => {
    expect((await get("s_weather", mcpRevisionId("nobody"), bearer)).status).toBe(404);
  });

  it("a revision of ANOTHER server, under this bundle", async () => {
    expect((await get("s_weather", TIDES_1, bearer)).status).toBe(404);
  });

  it("a proposal nobody was ever delivered", async () => {
    expect((await get("s_weather", WEATHER_PROPOSAL, bearer)).status).toBe(404);
  });

  it("a catalog server this workspace does not connect", async () => {
    expect((await get("mcps_loose", LOOSE_1, bearer)).status).toBe(404);
  });

  it("a bundle of the other kind", async () => {
    expect((await get("s_skill", WEATHER_1, bearer)).status).toBe(404);
  });
});
