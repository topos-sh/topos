import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { mcpRevisionId } from "../helpers/mcp-ids";
import { bootWorkspace, createScratchDb, type ScratchDb } from "./helpers/scratch-db";

/**
 * THE PUBLIC CATALOG FEED — what this install offers other installs, and everything it refuses to
 * offer them.
 *
 * The suite runs the REAL loader against a real scratch Postgres. Three properties matter more
 * than the shape: the feed is OFF until a deployment says otherwise, it carries GLOBAL PUBLISHED
 * rows and nothing else (a private server, a candidate and a pulled-back revision are all absent),
 * and the curation it adds rides in our own `_meta` namespace rather than in the official
 * registry's, which is the one field a reader uses to tell a subregistry from the real thing.
 */

let db: ScratchDb;
let wsId = "";

const ORIGIN = "http://x";

function document(name: string, version: string): Record<string, unknown> {
  return {
    name,
    description: `Everything about ${name}.`,
    version,
    remotes: [{ type: "streamable-http", url: "https://acme.example/mcp" }],
  };
}

/** The honest shape of a server we never stamped a version on — no `version` key at all. */
function versionlessDocument(name: string): Record<string, unknown> {
  return {
    name,
    description: `Everything about ${name}.`,
    remotes: [{ type: "streamable-http", url: "https://acme.example/mcp" }],
  };
}

async function get(path: string): Promise<Response> {
  const { loader } = await import("@/routes/mcp-catalog-feed");
  try {
    return await loader({
      request: new Request(`${ORIGIN}${path}`),
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
  opts: { workspaceId?: string | null; status?: string; authMode?: string | null } = {},
) {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, registry_name, display_name, auth_mode, status)
     VALUES ($1, $2, $3, $3, $4, $5)`,
    [
      id,
      opts.workspaceId ?? null,
      registryName,
      opts.authMode === undefined ? "none" : opts.authMode,
      opts.status ?? "active",
    ],
  );
}

async function seedRevision(
  serverId: string,
  id: string,
  seq: number,
  doc: Record<string, unknown>,
  opts: { current?: boolean; status?: "published" | "revoked" | "candidate" } = {},
) {
  const status = opts.status ?? "published";
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, status, upstream_version, document, transport, url, source,
        published_at, published_by, revoked_at)
     VALUES ($1, $2, $3, $4, $5, $6::jsonb, 'streamable-http', 'https://acme.example/mcp', 'seed',
             CASE WHEN $4 IN ('published', 'revoked') THEN now() END,
             CASE WHEN $4 IN ('published', 'revoked') THEN 'Staff' END,
             CASE WHEN $4 = 'revoked' THEN now() END)`,
    [
      id,
      serverId,
      seq,
      status,
      // A document that names no version stores a NULL column — never the string "undefined".
      doc.version === undefined ? null : String(doc.version),
      JSON.stringify(doc),
    ],
  );
  if (opts.current === true) {
    await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [serverId, id]);
  }
}

beforeAll(async () => {
  db = await createScratchDb("web_mcp_feed", { TOPOS_MCP_CATALOG_FEED: "on" });
  wsId = await bootWorkspace();

  // A published global server with a history behind it.
  await seedServer("mcps_a_weather", "io.github.acme/weather", { authMode: "oauth" });
  await seedRevision(
    "mcps_a_weather",
    mcpRevisionId("weather_1"),
    1,
    document("io.github.acme/weather", "1.0.0"),
  );
  await seedRevision(
    "mcps_a_weather",
    mcpRevisionId("weather_2"),
    2,
    document("io.github.acme/weather", "2.0.0"),
    { current: true },
  );
  // …whose second revision was pulled back after publication.
  await seedRevision(
    "mcps_a_weather",
    mcpRevisionId("weather_x"),
    3,
    document("io.github.acme/weather", "1.5.0"),
    { status: "revoked" },
  );

  // A second published global server, so the list has something to page over.
  await seedServer("mcps_b_tides", "io.github.acme/tides");
  await seedRevision(
    "mcps_b_tides",
    mcpRevisionId("tides_1"),
    1,
    document("io.github.acme/tides", "0.1.0"),
    {
      current: true,
    },
  );

  // Everything the feed must refuse to carry.
  await seedServer("mcps_c_candidate", "io.github.acme/candidate", { status: "candidate" });
  await seedRevision(
    "mcps_c_candidate",
    mcpRevisionId("candidate_1"),
    1,
    document("io.github.acme/candidate", "1.0.0"),
    { status: "candidate" },
  );
  await seedServer("mcps_d_private", "io.github.acme/private", { workspaceId: wsId });
  await seedRevision(
    "mcps_d_private",
    mcpRevisionId("private_1"),
    1,
    document("io.github.acme/private", "1.0.0"),
    { current: true },
  );

  // A SELF-MAINTAINED global server: staff maintain it, the official registry has no such name, so
  // its `registry_name` is null. It is a first-class member of the feed, addressed by the name
  // inside its own document.
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, registry_name, display_name, auth_mode, status)
     VALUES ('mcps_e_self', NULL, NULL, 'Internal Notes', 'manual', 'active')`,
  );
  await db.q(
    `UPDATE web.mcp_server SET auth_note = 'Ask ops for a token.' WHERE id = 'mcps_e_self'`,
  );
  await seedRevision(
    "mcps_e_self",
    mcpRevisionId("self_1"),
    1,
    document("sh.topos/internal-notes", "0.9.0"),
  );
  await seedRevision(
    "mcps_e_self",
    mcpRevisionId("self_2"),
    2,
    document("sh.topos/internal-notes", "1.0.0"),
    { current: true },
  );

  // A VERSION-LESS self-maintained server: it names no version at all. The feed must serve it
  // without inventing one — no `version` key on the document, `latest` resolving its single current.
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, registry_name, display_name, auth_mode, status)
     VALUES ('mcps_f_unversioned', NULL, NULL, 'Unversioned', 'none', 'active')`,
  );
  await seedRevision(
    "mcps_f_unversioned",
    mcpRevisionId("unversioned_1"),
    1,
    versionlessDocument("sh.topos/unversioned"),
    { current: true },
  );
}, 60000);

afterAll(async () => {
  await db.drop();
});

describe("the feed", () => {
  it("serves the published global rows, newest revision each", async () => {
    const res = await get("/mcp-catalog/v0.1/servers?limit=100");
    expect(res.status).toBe(200);
    expect(res.headers.get("cache-control")).toBe("public, max-age=60");
    const body = (await res.json()) as {
      servers: { server: { name: string; version: string }; _meta: Record<string, unknown> }[];
      metadata: { count: number };
    };
    const names = body.servers.map((s) => s.server.name);
    expect(names).toContain("io.github.acme/weather");
    expect(names).toContain("io.github.acme/tides");
    // The catalog this install was born with is here too — a feed carries the whole shelf.
    expect(names).toContain("com.github/mcp");
    expect(body.metadata.count).toBe(body.servers.length);
    const weather = body.servers.find((s) => s.server.name === "io.github.acme/weather");
    expect(weather?.server.version).toBe("2.0.0");
  });

  it("carries this catalog's curation under OUR namespace, never the official one", async () => {
    const res = await get("/mcp-catalog/v0.1/servers?limit=100");
    const body = (await res.json()) as {
      servers: { server: { name: string }; _meta: Record<string, unknown> }[];
    };
    for (const entry of body.servers) {
      expect(Object.keys(entry._meta)).toEqual(["sh.topos/catalog"]);
      expect(entry._meta["io.modelcontextprotocol.registry/official"]).toBeUndefined();
    }
    const weather = body.servers.find((s) => s.server.name === "io.github.acme/weather");
    const meta = weather?._meta as { "sh.topos/catalog": Record<string, unknown> };
    expect(meta["sh.topos/catalog"]).toMatchObject({
      status: "published",
      isLatest: true,
      auth: "oauth",
    });
  });

  it("exports nothing private and nothing undecided", async () => {
    const res = await get("/mcp-catalog/v0.1/servers?limit=100");
    const body = (await res.json()) as { servers: { server: { name: string } }[] };
    const names = body.servers.map((s) => s.server.name);
    expect(names).not.toContain("io.github.acme/private");
    expect(names).not.toContain("io.github.acme/candidate");
    expect((await get("/mcp-catalog/v0.1/servers/io.github.acme/private/versions")).status).toBe(
      404,
    );
  });

  it("pages on a cursor, and says so only while there is more", async () => {
    const first = await get("/mcp-catalog/v0.1/servers?limit=1");
    const firstBody = (await first.json()) as {
      servers: { server: { name: string } }[];
      metadata: { nextCursor?: string };
    };
    expect(firstBody.servers).toHaveLength(1);
    const cursor = String(firstBody.metadata.nextCursor);
    const second = await get(
      `/mcp-catalog/v0.1/servers?limit=1&cursor=${encodeURIComponent(cursor)}`,
    );
    const secondBody = (await second.json()) as { servers: { server: { name: string } }[] };
    expect(secondBody.servers[0]?.server.name).not.toBe(firstBody.servers[0]?.server.name);
    // The whole shelf in one page carries no cursor — a client is told it is done by absence.
    const whole = (await (await get("/mcp-catalog/v0.1/servers?limit=100")).json()) as {
      metadata: { nextCursor?: string };
    };
    expect(whole.metadata.nextCursor).toBeUndefined();
  });
});

describe("one server's history", () => {
  const ENCODED = encodeURIComponent("io.github.acme/weather");

  it("lists every published revision, newest first, and marks the one on offer", async () => {
    const res = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as {
      servers: { server: { version: string }; _meta: Record<string, unknown> }[];
      metadata: { count: number };
    };
    expect(body.servers.map((s) => s.server.version)).toEqual(["2.0.0", "1.0.0"]);
    const latest = body.servers[0]?._meta as { "sh.topos/catalog": { isLatest: boolean } };
    const older = body.servers[1]?._meta as { "sh.topos/catalog": { isLatest: boolean } };
    expect(latest["sh.topos/catalog"].isLatest).toBe(true);
    expect(older["sh.topos/catalog"].isLatest).toBe(false);
  });

  it("never lists a revision that was pulled back", async () => {
    const res = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions`);
    const body = (await res.json()) as { servers: { server: { version: string } }[] };
    expect(body.servers.map((s) => s.server.version)).not.toContain("1.5.0");
  });

  it("`latest` is the one on offer, and a named version resolves to itself", async () => {
    const latest = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions/latest`);
    const body = (await latest.json()) as { server: { version: string } };
    expect(body.server.version).toBe("2.0.0");
    const named = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions/1.0.0`);
    expect(((await named.json()) as { server: { version: string } }).server.version).toBe("1.0.0");
    expect((await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions/9.9.9`)).status).toBe(404);
  });

  it("an unknown name is the registry's problem shape", async () => {
    const res = await get("/mcp-catalog/v0.1/servers/io.github.acme/nope/versions");
    expect(res.status).toBe(404);
    expect(await res.json()).toMatchObject({ title: "Not Found", status: 404 });
  });
});

describe("a self-maintained server (no upstream name)", () => {
  const ENCODED = encodeURIComponent("sh.topos/internal-notes");

  it("appears in the feed, addressed by the name inside its own document", async () => {
    const res = await get("/mcp-catalog/v0.1/servers?limit=100");
    const body = (await res.json()) as {
      servers: { server: { name: string; version: string }; _meta: Record<string, unknown> }[];
    };
    const self = body.servers.find((s) => s.server.name === "sh.topos/internal-notes");
    // A subregistry offering what the official registry lacks is the value-add — it is on the shelf.
    expect(self).toBeDefined();
    expect(self?.server.version).toBe("1.0.0");
    // The curation this catalog adds still rides under our namespace, upstream name or not.
    const meta = self?._meta as { "sh.topos/catalog": Record<string, unknown> };
    expect(meta["sh.topos/catalog"]).toMatchObject({ status: "published", isLatest: true });
  });

  it("resolves its /versions by that document name, newest first", async () => {
    const res = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions`);
    expect(res.status).toBe(200);
    const body = (await res.json()) as {
      servers: {
        server: { version: string };
        _meta: { "sh.topos/catalog": { isLatest: boolean } };
      }[];
    };
    expect(body.servers.map((s) => s.server.version)).toEqual(["1.0.0", "0.9.0"]);
    expect(body.servers[0]?._meta["sh.topos/catalog"].isLatest).toBe(true);
    expect(body.servers[1]?._meta["sh.topos/catalog"].isLatest).toBe(false);
  });

  it("`latest` and a named version both resolve by that document name", async () => {
    const latest = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions/latest`);
    expect(((await latest.json()) as { server: { version: string } }).server.version).toBe("1.0.0");
    const named = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions/0.9.0`);
    expect(((await named.json()) as { server: { version: string } }).server.version).toBe("0.9.0");
  });
});

describe("a version-less server (no version at all)", () => {
  const ENCODED = encodeURIComponent("sh.topos/unversioned");

  it("appears in the feed with no version key — never a fabricated number", async () => {
    const res = await get("/mcp-catalog/v0.1/servers?limit=100");
    const body = (await res.json()) as {
      servers: { server: { name: string; version?: string } }[];
    };
    const self = body.servers.find((s) => s.server.name === "sh.topos/unversioned");
    expect(self).toBeDefined();
    // The document is served verbatim, and it never carried a version — so the wire carries none.
    expect(self?.server.version).toBeUndefined();
    expect(Object.hasOwn(self?.server ?? {}, "version")).toBe(false);
  });

  it("resolves /versions and /versions/latest to its one current document, no version invented", async () => {
    const versions = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions`);
    expect(versions.status).toBe(200);
    const list = (await versions.json()) as {
      servers: {
        server: { name: string; version?: string };
        _meta: { "sh.topos/catalog": { isLatest: boolean } };
      }[];
    };
    expect(list.servers).toHaveLength(1);
    expect(list.servers[0]?.server.version).toBeUndefined();
    expect(list.servers[0]?._meta["sh.topos/catalog"].isLatest).toBe(true);

    const latest = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions/latest`);
    expect(latest.status).toBe(200);
    const latestBody = (await latest.json()) as { server: { name: string; version?: string } };
    expect(latestBody.server.name).toBe("sh.topos/unversioned");
    expect(latestBody.server.version).toBeUndefined();
  });

  it("a numbered version selector matches nothing on a version-less server — the registry's 404", async () => {
    const res = await get(`/mcp-catalog/v0.1/servers/${ENCODED}/versions/1.0.0`);
    expect(res.status).toBe(404);
    expect(await res.json()).toMatchObject({ title: "Not Found", status: 404 });
  });
});

describe("the door", () => {
  it("a non-GET method on the path is the same uniform miss", async () => {
    const { action } = await import("@/routes/mcp-catalog-feed");
    expect(action().status).toBe(404);
  });

  it("a path shape the feed does not serve is the uniform 404", async () => {
    expect((await get("/mcp-catalog/v0.1/health")).status).toBe(404);
  });
});
