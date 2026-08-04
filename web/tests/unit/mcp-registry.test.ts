import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedSession,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * THE WORKSPACE'S MCP REGISTRY LANE — the official read API served over this workspace's own
 * catalog. The suite runs the REAL loader against a real scratch Postgres and the app's one
 * custody transport re-pointed at an in-process stub vault, so the documents it serves are
 * bytes that actually came back through `vaultFetch`.
 *
 * What it pins: the envelope shape an agent parses, the percent-encoded name resolution (the
 * whole reason this is a splat rather than a param), and — most important — that a caller with
 * no seat gets the SAME uniform 404 an unknown workspace would get, over both doors.
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
  version: "1.4.0",
  remotes: [{ type: "streamable-http", url: "https://weather.acme.example/mcp" }],
};
const TIDES = {
  name: "io.github.acme/tides",
  description: "Tide tables for a coastline.",
  version: "0.2.0",
  remotes: [{ type: "streamable-http", url: "https://tides.acme.example/mcp" }],
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

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_mcp_registry", { PLANE_INTERNAL_URL: vault.url });
  wsId = await bootWorkspace();
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seedUser(db, "u_out", "Stranger", "out@example.com");
  await seatUser(db, wsId, "u_mem", "member");
  await seedSession(db, "cs_mem", wsId, "u_mem");

  for (const [id, name, document] of [
    ["s_weather", "weather", WEATHER],
    ["s_tides", "tides", TIDES],
  ] as const) {
    const versionId = versionIdFor(id);
    await seedBundle(db, wsId, id, name, { kind: "mcp", versionId });
    vault.seed(wsId, id, versionId, [
      { path: "server.json", content: JSON.stringify(document, null, 2) },
    ]);
  }
  // Noise the lane must ignore: a plain skill, and an MCP bundle with nothing published.
  await seedBundle(db, wsId, "s_skill", "runbook", { kind: "skill" });
  await seedBundle(db, wsId, "s_draft", "draft-server", { kind: "mcp", withPointer: false });
}, 60000);

afterAll(async () => {
  await vault.close();
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
    expect(body.metadata.count).toBe(2);
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

  it("serves every published MCP server in the read API's envelope", async () => {
    const res = await get("/registry/v0.1/servers");
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("no-store");
    const body = (await res.json()) as {
      servers: { server: Record<string, unknown>; _meta: Record<string, unknown> }[];
      metadata: { count: number };
    };
    expect(body.metadata.count).toBe(2);
    expect(body.servers.map((s) => s.server.name)).toEqual([
      // Catalog-name order: tides before weather.
      "io.github.acme/tides",
      "io.github.acme/weather",
    ]);
    // The document is served VERBATIM — an agent parses what the publisher wrote.
    expect(body.servers[1]?.server).toEqual(WEATHER);
    const meta = body.servers[1]?._meta as { "sh.topos/catalog": Record<string, unknown> };
    expect(meta["sh.topos/catalog"]).toMatchObject({
      bundleId: "s_weather",
      versionId: versionIdFor("s_weather"),
    });
    expect(Number.isNaN(Date.parse(String(meta["sh.topos/catalog"].updatedAt)))).toBe(false);
  });

  it("skills and unpublished MCP bundles are absent", async () => {
    const res = await get("/registry/v0.1/servers");
    const body = (await res.json()) as { servers: { server: { name: string } }[] };
    expect(body.servers.map((s) => s.server.name)).not.toContain("runbook");
    expect(body.servers).toHaveLength(2);
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

  it("…/versions serves the list envelope holding the one current version", async () => {
    const res = await get(`/registry/v0.1/servers/${ENCODED}/versions`);
    const body = (await res.json()) as {
      servers: { server: { version: string } }[];
      metadata: { count: number };
    };
    expect(body.metadata.count).toBe(1);
    expect(body.servers[0]?.server.version).toBe("1.4.0");
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
    const { parseRegistryPath } = await import("@/routes/mcp-registry");
    expect(parseRegistryPath("/registry/v0.1/servers")).toEqual({ kind: "list" });
    expect(parseRegistryPath("/acme/registry/v0.1/servers")).toEqual({ kind: "list" });
    expect(parseRegistryPath("/acme/registry/v0.1/servers/")).toEqual({ kind: "list" });
    expect(parseRegistryPath("/registry/v0.1/servers/a.b%2Fc/versions")).toEqual({
      kind: "versions",
      name: "a.b/c",
    });
    expect(parseRegistryPath("/registry/v0.1/servers/a.b%2Fc/versions/latest")).toEqual({
      kind: "latest",
      name: "a.b/c",
    });
    expect(parseRegistryPath("/somewhere/else").kind).toBe("miss");
    expect(parseRegistryPath("/registry/v0.1/servers/versions").kind).toBe("miss");
  });
});
