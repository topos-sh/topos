import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { laneHeaders } from "./helpers/lane";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedSession,
  seedUser,
} from "./helpers/scratch-db";

/**
 * `POST /api/v1/workspaces/{ws}/mcp-servers` — the lane a machine shares a server through.
 *
 * The point of the route is that the CLIENT decides nothing: it sends a spelling, and every ruling
 * — is this a name the catalog offers, may this caller write a server down, is the document
 * shareable, what is the bundle called — is made here. So the suite drives the route the way the
 * wire does (a `Request`, a bearer, a JSON body) and asserts the answers a machine branches on:
 * the connect, the second connect that answers instead of refusing, the owner ruling on a name the
 * catalog does not offer, and the gate's own refusal on a document that may not be shared.
 *
 * The upstream fetch is the one seam a unit suite must not cross — the real one dials the internet
 * — so `loadServerDocument` is mocked and everything downstream of it runs for real.
 */

const ORIGIN = "https://topos.test";

let db: ScratchDb;
let wsId = "";

/** What the mocked upstream answers with. Set per test; a `null` value throws the fetch's own error. */
let upstream: { text: string } | Error = new Error("no upstream set");

/** Whether the upstream-fetch belt lets this call through — the seam a test flips to prove the
 * belt's own answer without hammering a real limiter into a fixed shape. */
let upstreamAllowed = true;

vi.mock("@/lib/rate-limit.server", async (importOriginal) => {
  const real = await importOriginal<typeof import("@/lib/rate-limit.server")>();
  return { ...real, allowUpstreamFetch: () => upstreamAllowed };
});

vi.mock("@/lib/mcp/fetch.server", async (importOriginal) => {
  const real = await importOriginal<typeof import("@/lib/mcp/fetch.server")>();
  return {
    ...real,
    loadServerDocument: async () => {
      if (upstream instanceof Error) {
        throw upstream;
      }
      return { ...upstream, url: "https://acme.example/server.json" };
    },
  };
});

type RouteHandler = (a: {
  request: Request;
  params: Record<string, string | undefined>;
}) => Promise<Response> | Response;

/** Drive the route the way the lane does — a guard's thrown Response is an answer, not a fault. */
async function post(
  bearer: string,
  body: unknown,
  ws: string = wsId,
): Promise<{ status: number; body: Record<string, unknown> }> {
  const { action } = await import("@/routes/api.v1.mcp-servers");
  const request = new Request(`${ORIGIN}/api/v1/workspaces/${ws}/mcp-servers`, {
    method: "POST",
    headers: laneHeaders({
      authorization: `Bearer ${bearer}`,
      "content-type": "application/json",
    }),
    body: JSON.stringify(body),
  });
  let response: Response;
  try {
    response = await (action as RouteHandler)({ request, params: { ws } });
  } catch (thrown) {
    if (thrown instanceof Response) {
      response = thrown;
    } else {
      throw thrown;
    }
  }
  return { status: response.status, body: (await response.json()) as Record<string, unknown> };
}

function errorCodeOf(body: Record<string, unknown>): string | undefined {
  return (body.error as { code?: string } | undefined)?.code;
}

function dataOf(body: Record<string, unknown>): Record<string, unknown> {
  return body.data as Record<string, unknown>;
}

/** A minimal document the gate accepts. */
function serverDocument(name: string, version = "1.0.0"): Record<string, unknown> {
  return {
    name,
    description: "A server for the suite.",
    version,
    remotes: [{ type: "streamable-http", url: "https://mcp.example.com/mcp" }],
  };
}

/** A GLOBAL catalog server with one published revision — what a member may connect. */
async function seedCatalogServer(id: string, registryName: string): Promise<void> {
  await db.q(
    `INSERT INTO web.mcp_server (id, registry_name, display_name, auth_mode, status)
     VALUES ($1, $2, $2, 'none', 'active')`,
    [id, registryName],
  );
  const { addMcpRevisionInTx } = await import("@/lib/db/queries.mcp-catalog.server");
  const { getDb } = await import("@/lib/db/index.server");
  const written = await getDb().transaction((tx) =>
    addMcpRevisionInTx(tx, id, {
      document: serverDocument(registryName),
      source: "seed",
      publish: true,
      attribution: "Suite",
    }),
  );
  expect(written.refusal).toBeNull();
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_lane_add", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
  await seedUser(db, "u_owner", "Owner", "owner@example.com");
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seatUser(db, wsId, "u_owner", "owner");
  await seatUser(db, wsId, "u_mem", "member");
  await seedSession(db, "sn_owner", wsId, "u_owner");
  await seedSession(db, "sn_mem", wsId, "u_mem");
  await seedCatalogServer("mcps_offered", "io.github.acme/offered");
}, 90000);

afterAll(async () => {
  await db.drop();
});

describe("connecting a server the catalog already offers", () => {
  it("any member may, and the answer names the bundle the workspace now shares", async () => {
    const answer = await post("sn_mem", { registry_name: "io.github.acme/offered" });
    expect(answer.status).toBe(200);
    expect(answer.body.ok).toBe(true);
    const data = dataOf(answer.body);
    expect(data.name).toBe("offered");
    expect(data.created).toBe(false);
    expect(typeof data.skill_id).toBe("string");
  });

  it("asking again ANSWERS with the standing bundle rather than refusing", async () => {
    const first = await post("sn_mem", { registry_name: "io.github.acme/offered" });
    const again = await post("sn_owner", { registry_name: "io.github.acme/offered" });
    expect(again.status).toBe(200);
    expect(again.body.ok).toBe(true);
    expect(dataOf(again.body).skill_id).toBe(dataOf(first.body).skill_id);
    expect(dataOf(again.body).created).toBe(false);
  });

  it("the connection reaches NOBODY until a channel is chosen", async () => {
    const bundleId = dataOf(
      (await post("sn_mem", { registry_name: "io.github.acme/offered" })).body,
    ).skill_id as string;
    const placements = await db.q<{ n: string }>(
      `SELECT COUNT(*) AS n FROM web.channel_bundle WHERE bundle_id = $1`,
      [bundleId],
    );
    expect(Number(placements[0]?.n)).toBe(0);
  });
});

describe("a server the catalog does NOT offer", () => {
  it("is an owner's ruling — a member is refused by name, and nothing is written", async () => {
    upstream = { text: JSON.stringify(serverDocument("io.github.acme/private-one")) };
    const answer = await post("sn_mem", { registry_name: "io.github.acme/private-one" });
    expect(answer.status).toBe(200);
    expect(answer.body.ok).toBe(false);
    expect(errorCodeOf(answer.body)).toBe("OWNER_ROLE_REQUIRED");
    const rows = await db.q<{ n: string }>(
      `SELECT COUNT(*) AS n FROM web.mcp_server WHERE registry_name = $1`,
      ["io.github.acme/private-one"],
    );
    expect(Number(rows[0]?.n)).toBe(0);
  });

  it("an OWNER writes it down and connects it in one call", async () => {
    upstream = { text: JSON.stringify(serverDocument("io.github.acme/private-two")) };
    const answer = await post("sn_owner", { registry_name: "io.github.acme/private-two" });
    expect(answer.status).toBe(200);
    expect(answer.body.ok).toBe(true);
    expect(dataOf(answer.body).created).toBe(true);
    const rows = await db.q<{ workspace_id: string | null }>(
      `SELECT workspace_id FROM web.mcp_server WHERE registry_name = $1`,
      ["io.github.acme/private-two"],
    );
    expect(rows[0]?.workspace_id).toBe(wsId);
  });

  it("a DOCUMENT LINK takes the same owner path", async () => {
    upstream = { text: JSON.stringify(serverDocument("io.github.acme/linked")) };
    const answer = await post("sn_owner", { document_url: "https://acme.example/server.json" });
    expect(answer.status).toBe(200);
    expect(dataOf(answer.body).created).toBe(true);
  });
});

describe("the document gate rules, and its words are the ones the machine renders", () => {
  it("a document carrying a credential is refused with the gate's own code", async () => {
    upstream = {
      text: JSON.stringify({
        ...serverDocument("io.github.acme/leaky"),
        _meta: { note: "ghp_0123456789abcdefghijklmnopqrstuvwxyzA" },
      }),
    };
    const answer = await post("sn_owner", { document_url: "https://acme.example/server.json" });
    expect(answer.body.ok).toBe(false);
    expect(errorCodeOf(answer.body)).toBe("MCP_SECRET_REFUSED");
    // The refusal never echoes the credential back.
    expect(JSON.stringify(answer.body)).not.toContain("ghp_0123456789");
  });

  it("a document with nothing to run is refused before anything is written", async () => {
    upstream = {
      text: JSON.stringify({
        name: "io.github.acme/empty",
        description: "Nothing to run.",
        version: "1.0.0",
      }),
    };
    const answer = await post("sn_owner", { document_url: "https://acme.example/server.json" });
    expect(answer.body.ok).toBe(false);
    expect(errorCodeOf(answer.body)).toBe("MCP_NO_STREAMABLE_REMOTE");
    const rows = await db.q<{ n: string }>(
      `SELECT COUNT(*) AS n FROM web.mcp_server WHERE registry_name = $1`,
      ["io.github.acme/empty"],
    );
    expect(Number(rows[0]?.n)).toBe(0);
  });

  it("the upstream-fetch belt answers typed too, and nothing is dialled", async () => {
    upstreamAllowed = false;
    upstream = { text: JSON.stringify(serverDocument("io.github.acme/belted")) };
    const answer = await post("sn_owner", { document_url: "https://acme.example/server.json" });
    expect(answer.status).toBe(200);
    expect(errorCodeOf(answer.body)).toBe("TOO_MANY_LOOKUPS");
    upstreamAllowed = true;
  });

  it("an upstream that cannot be read answers typed, never as a fault", async () => {
    const { McpFetchError } = await import("@/lib/mcp/fetch.server");
    upstream = new McpFetchError("that address does not resolve", "unresolved");
    const answer = await post("sn_owner", { document_url: "https://acme.example/server.json" });
    expect(answer.status).toBe(200);
    expect(errorCodeOf(answer.body)).toBe("MCP_UNREACHABLE");
  });
});

describe("the body, and the misses", () => {
  it("naming both a registry name and a url is a bad request", async () => {
    const answer = await post("sn_owner", {
      registry_name: "io.github.acme/offered",
      document_url: "https://acme.example/server.json",
    });
    expect(answer.status).toBe(400);
  });

  it("naming NEITHER is a bad request", async () => {
    expect((await post("sn_owner", {})).status).toBe(400);
  });

  it("a GET on the served path is the uniform 404, byte-identical to a miss", async () => {
    const { loader } = await import("@/routes/api.v1.mcp-servers");
    const answered = (loader as () => Response)();
    expect(answered.status).toBe(404);
    expect(((await answered.json()) as { error?: { code?: string } }).error?.code).toBe(
      "NOT_FOUND",
    );
  });

  it("an unauthenticated call is the uniform 404 — the route is no existence oracle", async () => {
    const answer = await post("no-such-credential", { registry_name: "io.github.acme/offered" });
    expect(answer.status).toBe(404);
    expect(errorCodeOf(answer.body)).toBe("NOT_FOUND");
  });
});
