import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { mcpRevisionId } from "../helpers/mcp-ids";
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
 * THE WORKSPACE'S MCP REGISTRY LANE — the official read API served over the servers this
 * workspace runs. The suite runs the REAL loader against a real scratch Postgres, so what it
 * serves is what the catalog rows say.
 *
 * What it pins: the envelope shape an agent parses, the percent-encoded name resolution (the
 * whole reason this is a splat rather than a param), that a PIN is what the lane answers with
 * (this lane says what the team runs, not what the catalog offers), and — most important — that
 * a caller with no seat gets the SAME uniform 404 an unknown workspace would get, over both
 * doors.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

let db: ScratchDb;
let wsId = "";

const ORIGIN = "http://x";

function document(name: string, version: string, url: string): Record<string, unknown> {
  return {
    name,
    description: `Everything about ${name}.`,
    version,
    remotes: [{ type: "streamable-http", url }],
  };
}

const WEATHER = document("io.github.acme/weather", "1.4.0", "https://weather.acme.example/mcp");
const WEATHER_OLD = document("io.github.acme/weather", "1.0.0", "https://weather.acme.example/mcp");
const TIDES = document("io.github.acme/tides", "0.2.0", "https://tides.acme.example/mcp");
/** A PACKAGE-ONLY server: no address at all — a document a machine installs rather than dials. */
const FILES = {
  name: "io.github.acme/files",
  description: "Files the agent is pointed at, over stdio.",
  version: "2.1.0",
  packages: [
    {
      registryType: "npm",
      identifier: "@acme/mcp-files",
      version: "2.1.0",
      runtimeHint: "npx",
      transport: { type: "stdio" },
      environmentVariables: [{ name: "ACME_API_TOKEN", isSecret: true, isRequired: true }],
    },
  ],
};

async function get(path: string, headers: Record<string, string> = {}): Promise<Response> {
  const { loader } = await import("@/routes/mcp-registry");
  try {
    return await loader({
      request: new Request(`${ORIGIN}${path}`, { headers }),
      params: {},
      context: {},
    } as unknown as Parameters<typeof loader>[0]);
  } catch (thrown) {
    if (thrown instanceof Response) {
      return thrown;
    }
    throw thrown;
  }
}

async function seedServer(
  id: string,
  registryName: string,
  opts: { workspaceId?: string | null; authMode?: string | null; authNote?: string | null } = {},
) {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, registry_name, display_name, auth_mode, auth_note, status)
     VALUES ($1, $2, $3, $3, $4, $5, 'active')`,
    [
      id,
      opts.workspaceId ?? null,
      registryName,
      opts.authMode === undefined ? "none" : opts.authMode,
      opts.authNote ?? null,
    ],
  );
}

async function seedRevision(
  serverId: string,
  id: string,
  seq: number,
  doc: Record<string, unknown>,
  opts: { current?: boolean; status?: "published" | "revoked" } = {},
) {
  const status = opts.status ?? "published";
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, status, upstream_version, document, transport, url, source,
        published_at, published_by, revoked_at)
     VALUES ($1, $2, $3, $4, $5, $6::jsonb, 'streamable-http', 'https://x.example/mcp', 'seed',
             now(), 'Staff', CASE WHEN $4 = 'revoked' THEN now() END)`,
    [id, serverId, seq, status, String(doc.version), JSON.stringify(doc)],
  );
  if (opts.current === true) {
    await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [serverId, id]);
  }
}

async function connect(bundleId: string, serverId: string, pinned: string | null = null) {
  await db.q(
    `INSERT INTO web.bundle_mcp (bundle_id, workspace_id, server_id, pinned_revision_id)
     VALUES ($1, $2, $3, $4)`,
    [bundleId, wsId, serverId, pinned],
  );
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_registry");
  wsId = await bootWorkspace();
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seedUser(db, "u_out", "Stranger", "out@example.com");
  await seatUser(db, wsId, "u_mem", "member");
  await seedSession(db, "cs_mem", wsId, "u_mem");

  // A catalog server the workspace follows at its current revision.
  await seedServer("mcps_weather", "io.github.acme/weather", { authMode: "oauth" });
  await seedRevision("mcps_weather", mcpRevisionId("weather_1"), 1, WEATHER_OLD);
  await seedRevision("mcps_weather", mcpRevisionId("weather_2"), 2, WEATHER, { current: true });
  await seedBundle(db, wsId, "s_weather", "weather", { kind: "mcp", withPointer: false });
  await connect("s_weather", "mcps_weather");

  // A catalog server the workspace PINNED to an older revision — what the team runs is the pin.
  await seedServer("mcps_tides", "io.github.acme/tides", {
    authMode: "manual",
    authNote: "Mint a token in the tides console first.",
  });
  await seedRevision("mcps_tides", mcpRevisionId("tides_1"), 1, TIDES);
  await seedRevision(
    "mcps_tides",
    mcpRevisionId("tides_2"),
    2,
    document("io.github.acme/tides", "0.3.0", "https://tides.acme.example/mcp"),
    { current: true },
  );
  await seedBundle(db, wsId, "s_tides", "tides", { kind: "mcp", withPointer: false });
  await connect("s_tides", "mcps_tides", mcpRevisionId("tides_1"));

  // The workspace's OWN server, private and connected — nobody else's lane may show it.
  await seedServer("mcps_files", "io.github.acme/files", { workspaceId: wsId, authMode: null });
  await seedRevision("mcps_files", mcpRevisionId("files_1"), 1, FILES, { current: true });
  await seedBundle(db, wsId, "s_files", "files", { kind: "mcp", withPointer: false });
  await connect("s_files", "mcps_files");

  // Noise the lane must ignore: a plain skill, a catalog server nobody here connected, and a
  // connection whose bundle was archived.
  await seedBundle(db, wsId, "s_skill", "runbook", { kind: "skill" });
  await seedServer("mcps_other", "io.github.acme/unconnected");
  await seedRevision(
    "mcps_other",
    mcpRevisionId("other_1"),
    1,
    document("io.github.acme/unconnected", "1.0.0", "https://other.acme.example/mcp"),
    { current: true },
  );
  await seedServer("mcps_gone", "io.github.acme/gone");
  await seedRevision(
    "mcps_gone",
    mcpRevisionId("gone_1"),
    1,
    document("io.github.acme/gone", "1.0.0", "https://gone.acme.example/mcp"),
    { current: true },
  );
  await seedBundle(db, wsId, "s_gone", "gone", {
    kind: "mcp",
    withPointer: false,
    status: "archived",
  });
  await connect("s_gone", "mcps_gone");
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the door", () => {
  it("an anonymous request is the uniform 404 — no list, no count, no hint", async () => {
    session = null;
    const res = await get("/registry/v0.1/servers");
    expect(res.status).toBe(404);
    const body = (await res.json()) as { error: { code: string } };
    expect(body.error.code).toBe("NOT_FOUND");
  });

  it("a signed-in NON-member gets the byte-identical answer", async () => {
    session = { user: { id: "u_out", name: "Stranger", email: "out@example.com" } };
    const res = await get("/registry/v0.1/servers");
    expect(res.status).toBe(404);
    expect(await res.text()).toBe(await (await get("/registry/v0.1/servers")).text());
  });

  it("an unknown bearer is the same 404 (never a 401 that confirms the path)", async () => {
    session = null;
    const res = await get("/registry/v0.1/servers", { authorization: "Bearer nope" });
    expect(res.status).toBe(404);
  });

  it("a live session bearer reads the catalog", async () => {
    session = null;
    const res = await get("/registry/v0.1/servers", { authorization: "Bearer cs_mem" });
    expect(res.status).toBe(200);
    const body = (await res.json()) as { metadata: { count: number } };
    expect(body.metadata.count).toBe(3);
  });

  it("a non-GET method on the path is the same uniform miss", async () => {
    const { action } = await import("@/routes/mcp-registry");
    expect(action().status).toBe(404);
  });
});

describe("the list", () => {
  beforeAll(() => {
    session = { user: { id: "u_mem", name: "Member", email: "mem@example.com" } };
  });

  it("serves what the workspace runs, in the read API's envelope", async () => {
    const res = await get("/registry/v0.1/servers");
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as {
      servers: { server: Record<string, unknown>; _meta: Record<string, unknown> }[];
      metadata: { count: number; nextCursor?: string };
    };
    expect(body.metadata.count).toBe(3);
    expect(body.metadata.nextCursor).toBeUndefined();
    // The document is served VERBATIM — an agent parses what the publisher wrote.
    const weather = body.servers.find((s) => s.server.name === "io.github.acme/weather");
    expect(weather?.server).toEqual(WEATHER);
    // …and a PACKAGE-ONLY document is as servable as one with an address.
    expect(body.servers.find((s) => s.server.name === "io.github.acme/files")?.server).toEqual(
      FILES,
    );
    const meta = weather?._meta as { "sh.topos/catalog": Record<string, unknown> };
    expect(meta["sh.topos/catalog"]).toMatchObject({
      status: "published",
      isLatest: true,
      auth: "oauth",
    });
    expect(Number.isNaN(Date.parse(String(meta["sh.topos/catalog"].publishedAt)))).toBe(false);
  });

  it("a PIN is what the lane answers with, and it is not the catalog's latest", async () => {
    const res = await get("/registry/v0.1/servers");
    const body = (await res.json()) as {
      servers: { server: { name: string; version: string }; _meta: Record<string, unknown> }[];
    };
    const tides = body.servers.find((s) => s.server.name === "io.github.acme/tides");
    expect(tides?.server.version).toBe("0.2.0");
    const meta = tides?._meta as { "sh.topos/catalog": Record<string, unknown> };
    expect(meta["sh.topos/catalog"]).toMatchObject({
      isLatest: false,
      auth: "manual",
      authNote: "Mint a token in the tides console first.",
    });
  });

  it("never writes into the official registry's namespace", async () => {
    const res = await get("/registry/v0.1/servers");
    const body = (await res.json()) as { servers: { _meta: Record<string, unknown> }[] };
    for (const entry of body.servers) {
      expect(Object.keys(entry._meta)).toEqual(["sh.topos/catalog"]);
    }
  });

  it("a tier nobody established is stated as nothing, never as 'none'", async () => {
    const res = await get("/registry/v0.1/servers");
    const body = (await res.json()) as {
      servers: { server: { name: string }; _meta: Record<string, unknown> }[];
    };
    const files = body.servers.find((s) => s.server.name === "io.github.acme/files");
    const meta = (files?._meta as { "sh.topos/catalog": Record<string, unknown> })[
      "sh.topos/catalog"
    ];
    expect("auth" in meta).toBe(false);
  });

  it("skills, unconnected catalog rows and archived bundles are absent", async () => {
    const res = await get("/registry/v0.1/servers");
    const body = (await res.json()) as { servers: { server: { name: string } }[] };
    const names = body.servers.map((s) => s.server.name);
    expect(names).not.toContain("runbook");
    expect(names).not.toContain("io.github.acme/unconnected");
    expect(names).not.toContain("io.github.acme/gone");
  });

  it("pages on a cursor the caller passes back", async () => {
    const first = await get("/registry/v0.1/servers?limit=2");
    const firstBody = (await first.json()) as {
      servers: { server: { name: string } }[];
      metadata: { nextCursor?: string };
    };
    expect(firstBody.servers).toHaveLength(2);
    expect(firstBody.metadata.nextCursor).toBeDefined();
    const second = await get(
      `/registry/v0.1/servers?limit=2&cursor=${encodeURIComponent(String(firstBody.metadata.nextCursor))}`,
    );
    const secondBody = (await second.json()) as {
      servers: { server: { name: string } }[];
      metadata: { nextCursor?: string };
    };
    expect(secondBody.servers).toHaveLength(1);
    expect(secondBody.metadata.nextCursor).toBeUndefined();
    const seen = [...firstBody.servers, ...secondBody.servers].map((s) => s.server.name);
    expect(new Set(seen).size).toBe(3);
  });
});

describe("resolving one server by its embedded name", () => {
  beforeAll(() => {
    session = { user: { id: "u_mem", name: "Member", email: "mem@example.com" } };
  });

  const ENCODED = encodeURIComponent("io.github.acme/weather");

  it("…/versions/latest serves the bare { server, _meta } pair", async () => {
    const res = await get(`/registry/v0.1/servers/${ENCODED}/versions/latest`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as { server: Record<string, unknown> };
    expect(body.server).toEqual(WEATHER);
    expect("servers" in body).toBe(false);
  });

  it("…/versions serves the list envelope holding the one version this team runs", async () => {
    const res = await get(`/registry/v0.1/servers/${ENCODED}/versions`);
    const body = (await res.json()) as {
      servers: { server: { version: string } }[];
      metadata: { count: number };
    };
    expect(body.metadata.count).toBe(1);
    expect(body.servers[0]?.server.version).toBe("1.4.0");
  });

  it("names a version outright, and refuses one this team does not run", async () => {
    const hit = await get(`/registry/v0.1/servers/${ENCODED}/versions/1.4.0`);
    expect(hit.status).toBe(200);
    const miss = await get(`/registry/v0.1/servers/${ENCODED}/versions/1.0.0`);
    expect(miss.status).toBe(404);
  });

  it("a LITERAL slash resolves identically — the encoding is the caller's choice", async () => {
    const encoded = await get(`/registry/v0.1/servers/${ENCODED}/versions/latest`);
    const literal = await get("/registry/v0.1/servers/io.github.acme/weather/versions/latest");
    expect(literal.status).toBe(200);
    expect(await literal.text()).toBe(await encoded.text());
  });

  it("an unknown name is the registry's problem shape, not the wire envelope", async () => {
    const res = await get(
      `/registry/v0.1/servers/${encodeURIComponent("io.github.acme/nope")}/versions/latest`,
    );
    expect(res.status).toBe(404);
    const body = (await res.json()) as Record<string, unknown>;
    expect(body).toMatchObject({ title: "Not Found", status: 404 });
    expect(typeof body.detail).toBe("string");
  });

  it("a path shape the lane does not serve is the uniform 404", async () => {
    expect((await get(`/registry/v0.1/servers/${ENCODED}`)).status).toBe(404);
    expect((await get("/registry/v0.1/health")).status).toBe(404);
  });

  it("the CATALOG name is not an alias — resolution is on the embedded name alone", async () => {
    const res = await get("/registry/v0.1/servers/weather/versions/latest");
    expect(res.status).toBe(404);
  });
});

describe("path parsing", () => {
  it("finds its own prefix in both tenancy grammars", async () => {
    const { parseRegistryPath } = await import("@/lib/mcp/registry-api.server");
    const at = (path: string) => parseRegistryPath(path, "/registry/v0.1/servers");
    expect(at("/registry/v0.1/servers")).toEqual({ kind: "list" });
    expect(at("/acme/registry/v0.1/servers")).toEqual({ kind: "list" });
    expect(at("/acme/registry/v0.1/servers/")).toEqual({ kind: "list" });
    expect(at("/registry/v0.1/servers/a.b%2Fc/versions")).toEqual({
      kind: "versions",
      name: "a.b/c",
    });
    expect(at("/registry/v0.1/servers/a.b%2Fc/versions/latest")).toEqual({
      kind: "version",
      name: "a.b/c",
      version: "latest",
    });
    expect(at("/registry/v0.1/servers/a.b/c/versions/1.2.3")).toEqual({
      kind: "version",
      name: "a.b/c",
      version: "1.2.3",
    });
    // A name that CONTAINS the marker word still resolves: the rightmost one is the pivot.
    expect(at("/registry/v0.1/servers/a.b/versions/versions/latest")).toEqual({
      kind: "version",
      name: "a.b/versions",
      version: "latest",
    });
    expect(at("/somewhere/else").kind).toBe("miss");
    expect(at("/registry/v0.1/servers/versions").kind).toBe("miss");
    expect(at("/registry/v0.1/servers/a.b%2Fc/versions/1.0.0/files").kind).toBe("miss");
  });
});
