import { createElement } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import {
  createStaticHandler,
  createStaticRouter,
  type RouteObject,
  StaticRouterProvider,
} from "react-router";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedUser,
} from "./helpers/scratch-db";

/**
 * THE ADD-AN-MCP-SERVER PAGE — its two acts against a real scratch Postgres, with the fetch seam
 * replaced so no test touches the network.
 *
 * CONNECTING is the common one: the server is a catalog row already, so the page posts an id and
 * the workspace gets the bundle that names it. Nothing is copied, nothing is validated a second
 * time — the row was verified when it was published.
 *
 * WRITING ONE DOWN is the other, and it is an owner's: the three custom sources are one code path,
 * whatever the bytes came from the preview canonicalizes them and runs the document gate, and the
 * create arm runs it AGAIN on the bytes the form posted back — the form is a client, and a
 * client's word is not the gate. That second run is what these tests lean on hardest.
 *
 * The SSRF guard is exercised directly, with the resolver mocked: what matters is which ADDRESSES
 * it refuses, and a test that needed real DNS to say so would be testing DNS.
 */

let session: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => session } }),
}));

/** The fetch seam: the two network arms answer from here, the paste arm never calls it. */
const fetched = vi.hoisted(() => ({
  text: "",
  url: "https://example.test/server.json",
  fail: null as string | null,
  calls: [] as { kind: string; value: string }[],
}));

vi.mock("@/lib/mcp/fetch.server", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/mcp/fetch.server")>();
  return {
    ...actual,
    loadServerDocument: async (source: { kind: string; value: string }) => {
      fetched.calls.push(source);
      // The arm that never reaches the network runs for real.
      if (source.kind === "paste") {
        return await actual.loadServerDocument(source as { kind: "paste"; value: string });
      }
      if (fetched.fail !== null) {
        throw new actual.McpFetchError(fetched.fail);
      }
      return { text: fetched.text, url: fetched.url };
    },
  };
});

let db: ScratchDb;
let wsId = "";

const ORIGIN = "http://x";

const OWNER = { id: "u_own", name: "Owner", email: "own@example.com" };
const MEMBER = { id: "u_mem", name: "Member", email: "mem@example.com" };

const WEATHER = {
  name: "io.github.acme/weather",
  description: "Current conditions for a named place.",
  version: "1.4.0",
  remotes: [{ type: "streamable-http", url: "https://weather.acme.example/mcp" }],
};

/**
 * What the action answered, whichever way it answered it: `data()` returns a wrapper the
 * framework serializes later, a redirect is a thrown Response. The suite reads both through
 * one shape so a test asserts the OUTCOME rather than the return convention.
 */
interface ActionResult {
  status: number;
  body: Record<string, unknown>;
  location: string | null;
}

async function post(fields: Record<string, string>): Promise<ActionResult> {
  const { action } = await import("@/routes/mcp-new");
  const form = new FormData();
  for (const [key, value] of Object.entries(fields)) {
    form.set(key, value);
  }
  let result: unknown;
  try {
    result = await action({
      request: new Request(`${ORIGIN}/mcp/new`, {
        method: "POST",
        headers: { origin: ORIGIN },
        body: form,
      }),
      params: {},
      context: {},
    } as unknown as Parameters<typeof action>[0]);
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
    return { status: result.status, body, location: result.headers.get("location") };
  }
  if (typeof result === "object" && result !== null && "data" in result) {
    const wrapper = result as { data: Record<string, unknown>; init?: ResponseInit };
    return { status: wrapper.init?.status ?? 200, body: wrapper.data, location: null };
  }
  throw result;
}

/** A catalog server: global unless a workspace is named, published unless told otherwise. */
async function seedServer(
  id: string,
  registryName: string,
  opts: { status?: string; publish?: boolean; authMode?: string | null } = {},
): Promise<void> {
  await db.q(
    `INSERT INTO web.mcp_server (id, registry_name, display_name, description, auth_mode, status)
     VALUES ($1, $2, $2, 'A server for the suite.', $3, $4)`,
    [
      id,
      registryName,
      opts.authMode === undefined ? "none" : opts.authMode,
      opts.status ?? "active",
    ],
  );
  const revisionId = `${id}_r1`;
  const published = opts.publish !== false;
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, status, upstream_version, document, transport, url, source,
        published_at, published_by)
     VALUES ($1, $2, 1, $3, '1.0.0', $4::jsonb, 'streamable-http',
             'https://acme.example/mcp', 'seed',
             CASE WHEN $3 = 'published' THEN now() END,
             CASE WHEN $3 = 'published' THEN 'Staff' END)`,
    [
      revisionId,
      id,
      published ? "published" : "candidate",
      JSON.stringify({ ...WEATHER, name: registryName }),
    ],
  );
  if (published) {
    await db.q(`UPDATE web.mcp_server SET current_revision_id = $2 WHERE id = $1`, [
      id,
      revisionId,
    ]);
  }
}

const bundleNamed = async (name: string) =>
  (
    await db.q<{ id: string; kind: string }>(
      `SELECT id, kind FROM web.bundle WHERE workspace_id = $1 AND name = $2`,
      [wsId, name],
    )
  )[0];

beforeAll(async () => {
  db = await createScratchDb("web_mcp_new", { TOPOS_WEB_RATELIMIT: "off" });
  wsId = await bootWorkspace();
  await seedUser(db, OWNER.id, OWNER.name, OWNER.email);
  await seatUser(db, wsId, OWNER.id, "owner");
  await seedUser(db, MEMBER.id, MEMBER.name, MEMBER.email);
  await seatUser(db, wsId, MEMBER.id, "member");
  session = { user: OWNER };
}, 60000);

afterAll(async () => {
  await db.drop();
});

beforeEach(() => {
  fetched.fail = null;
  fetched.calls.length = 0;
  session = { user: OWNER };
});

describe("the preview", () => {
  it("reads a REGISTRY answer, unwrapping the { server, _meta } envelope", async () => {
    fetched.text = JSON.stringify({ server: WEATHER, _meta: { official: { status: "active" } } });
    const { body } = await post({
      intent: "preview",
      source: "registry",
      registry_name: "io.github.acme/weather",
    });
    expect(body.form).toBe("preview");
    expect((body.summary as { name: string }).name).toBe("io.github.acme/weather");
    // The envelope is gone: what would be stored is the document itself.
    expect(String(body.document)).not.toContain("_meta");
    expect(fetched.calls).toEqual([{ kind: "registry", value: "io.github.acme/weather" }]);
  });

  it("reads a bare document from a URL", async () => {
    fetched.text = JSON.stringify(WEATHER);
    const { body } = await post({
      intent: "preview",
      source: "url",
      url: "https://example.test/server.json",
    });
    expect(body.form).toBe("preview");
    expect((body.summary as { url: string }).url).toBe("https://weather.acme.example/mcp");
  });

  it("reads a PASTED document without any fetch at all", async () => {
    const { body } = await post({
      intent: "preview",
      source: "paste",
      document: JSON.stringify(WEATHER),
    });
    expect(body.form).toBe("preview");
    expect(body.origin).toBe("pasted");
  });

  it("surfaces a fetch failure as the fetcher worded it", async () => {
    fetched.fail = "that registry has no server by that name";
    const { status, body } = await post({
      intent: "preview",
      source: "registry",
      registry_name: "io.github.acme/nope",
    });
    expect(status).toBe(400);
    expect(body.error).toBe("that registry has no server by that name");
  });

  it("refuses a document the gate refuses, naming the code", async () => {
    const { status, body } = await post({
      intent: "preview",
      source: "paste",
      document: JSON.stringify({
        ...WEATHER,
        remotes: [{ type: "streamable-http", url: "http://weather.acme.example/mcp" }],
      }),
    });
    expect(status).toBe(400);
    expect(body.code).toBe("MCP_INSECURE_URL");
  });

  it("refuses an empty source without calling the fetcher", async () => {
    const { status, body } = await post({ intent: "preview", source: "url", url: "" });
    expect(status).toBe(400);
    expect(body.error).toBe("Pick a source and fill in the matching field.");
    expect(fetched.calls).toEqual([]);
  });
});

describe("connecting a catalog server", () => {
  it("gives the workspace a bundle that names the server, and reaches nobody", async () => {
    await seedServer("mcps_conn", "io.github.acme/connected");
    const { status, location } = await post({
      intent: "connect",
      server_id: "mcps_conn",
      name: "connected",
      channel: "",
    });
    // The act lands by redirect to the server's page — in the MCP section, never under /skills.
    expect(status).toBe(302);
    expect(location).toBe("/mcp/connected");

    const bundle = await bundleNamed("connected");
    expect(bundle?.kind).toBe("mcp");
    const connection = await db.q<{ server_id: string; pinned_revision_id: string | null }>(
      `SELECT server_id, pinned_revision_id FROM web.bundle_mcp WHERE bundle_id = $1`,
      [bundle?.id],
    );
    expect(connection[0]?.server_id).toBe("mcps_conn");
    // Following the catalog, not pinned to today's version.
    expect(connection[0]?.pinned_revision_id).toBe(null);
    // AND IT REACHES NOBODY. An empty destination means no channel, not the default one: adding a
    // server is not the same act as handing it to the workspace.
    const placed = await db.q<{ n: number }>(
      `SELECT count(*)::int AS n FROM web.channel_bundle WHERE bundle_id = $1`,
      [bundle?.id],
    );
    expect(Number(placed[0]?.n)).toBe(0);
    // And the act is on the record.
    const audit = await db.q<{ details: Record<string, unknown> }>(
      `SELECT details FROM web.audit_event WHERE kind = 'mcp_server_connected' AND subject = $1`,
      [bundle?.id],
    );
    expect(audit[0]?.details).toMatchObject({ serverId: "mcps_conn" });
  });

  /**
   * THE OTHER HALF OF THE SAME RULE. Not choosing is the default, but choosing still works — and
   * the default channel is chosen the way every other channel is, BY NAME, so there is no empty
   * value doing double duty and nothing that lands in `everyone` without being asked for.
   */
  it("places into the channel the form names, the default one included", async () => {
    await seedServer("mcps_named", "io.github.acme/named");
    const { status, location } = await post({
      intent: "connect",
      server_id: "mcps_named",
      name: "named-dest",
      channel: "everyone",
    });
    expect(status).toBe(302);
    expect(location).toBe("/mcp/named-dest");
    const placed = await db.q<{ n: string }>(
      `SELECT count(*)::int AS n FROM web.channel_bundle cb
       JOIN web.channel c ON c.id = cb.channel_id AND c.is_default
       JOIN web.bundle b ON b.id = cb.bundle_id
       WHERE b.name = 'named-dest'`,
    );
    expect(Number(placed[0]?.n)).toBe(1);
  });

  it("refuses a second connection to a server this workspace already runs", async () => {
    await seedServer("mcps_twice", "io.github.acme/twice");
    expect((await post({ intent: "connect", server_id: "mcps_twice", name: "twice" })).status).toBe(
      302,
    );
    const { status, body } = await post({
      intent: "connect",
      server_id: "mcps_twice",
      server: "mcps_twice",
      name: "twice-again",
    });
    expect(status).toBe(400);
    expect(body.code).toBe("MCP_ALREADY_CONNECTED");
    // The refusal goes back into the dialog that asked, carrying the row it is about.
    expect(body.form).toBe("pick");
    expect(body.server).toBe("mcps_twice");
    expect(await bundleNamed("twice-again")).toBeUndefined();
  });

  it("refuses a server the catalog does not offer, and says nothing else about it", async () => {
    await seedServer("mcps_cand", "io.github.acme/candidate", {
      status: "candidate",
      publish: false,
    });
    for (const serverId of ["mcps_cand", "mcps_absent"]) {
      const { status, body } = await post({ intent: "connect", server_id: serverId, name: "x" });
      expect(status).toBe(400);
      expect(body.code).toBe("MCP_SERVER_NOT_FOUND");
    }
  });
});

describe("writing down a server the catalog does not carry", () => {
  it("is an owner's act — a member is refused the way every owner-only act refuses", async () => {
    session = { user: MEMBER };
    const { status } = await post({
      intent: "create",
      document: JSON.stringify({ ...WEATHER, name: "io.github.acme/by-member" }),
      name: "by-member",
    });
    expect(status).toBe(404);
    expect(await bundleNamed("by-member")).toBeUndefined();
  });

  it("creates the workspace's OWN server, holding the document, and connects it", async () => {
    const document = `${JSON.stringify({ ...WEATHER, name: "io.github.acme/private" }, null, 2)}\n`;
    const { status, location } = await post({
      intent: "create",
      document,
      name: "private-one",
      channel: "",
    });
    expect(status).toBe(302);
    expect(location).toBe("/mcp/private-one");

    const bundle = await bundleNamed("private-one");
    const rows = await db.q<{
      workspace_id: string | null;
      registry_name: string;
      auth_mode: string | null;
      document: Record<string, unknown>;
    }>(
      `SELECT ms.workspace_id, ms.registry_name, ms.auth_mode, r.document
       FROM web.bundle_mcp bm
       JOIN web.mcp_server ms ON ms.id = bm.server_id
       JOIN web.mcp_server_revision r ON r.id = ms.current_revision_id
       WHERE bm.bundle_id = $1`,
      [bundle?.id],
    );
    // PRIVATE to this workspace, exported nowhere.
    expect(rows[0]?.workspace_id).toBe(wsId);
    expect(rows[0]?.registry_name).toBe("io.github.acme/private");
    expect(rows[0]?.document).toMatchObject({ name: "io.github.acme/private" });
    // NOTHING IS CLAIMED about the sign-in: nobody checked it, so the row says nothing.
    expect(rows[0]?.auth_mode).toBe(null);
  });

  it("re-runs the gate on the posted bytes — a doctored form is refused, nothing written", async () => {
    const { status, body } = await post({
      intent: "create",
      document: JSON.stringify({
        ...WEATHER,
        name: "io.github.acme/doctored",
        remotes: [
          {
            type: "streamable-http",
            url: "https://a.example/mcp",
            headers: [{ name: "X-Key", isRequired: true }],
          },
        ],
      }),
      name: "doctored",
      channel: "",
    });
    expect(status).toBe(400);
    expect(body.code).toBe("MCP_SECRET_REFUSED");
    expect(await bundleNamed("doctored")).toBeUndefined();
    expect(
      await db.q(`SELECT id FROM web.mcp_server WHERE registry_name = $1`, [
        "io.github.acme/doctored",
      ]),
    ).toEqual([]);
  });

  it("hands the staged document back with the refusal, so a retry keeps the bytes", async () => {
    // The document PARSES and passes the file gate; what refuses it is the catalog's own
    // fail-closed schema vocabulary, which is exactly the refusal a person retries after.
    const document = JSON.stringify(
      {
        ...WEATHER,
        name: "io.github.acme/staged",
        $schema: "https://static.modelcontextprotocol.io/schemas/2099-01-01/server.schema.json",
      },
      null,
      2,
    );
    const { status, body } = await post({
      intent: "create",
      document,
      name: "staged",
      origin: "https://staged.example/server.json",
      channel: "",
    });
    expect(status).toBe(400);
    expect(body.code).toBe("MCP_SCHEMA_UNKNOWN");
    const { canonicalServerJson } = await import("@/lib/mcp/fetch.server");
    const staged = body.preview as { document: string; origin: string } | undefined;
    // The bytes come back CANONICAL, which is what a retry would store — not the browser's
    // spacing, and not a second opinion about it.
    expect(staged?.document).toBe(canonicalServerJson(JSON.parse(document)));
    expect(staged?.origin).toBe("https://staged.example/server.json");
    expect(await bundleNamed("staged")).toBeUndefined();
  });

  it("hands nothing back when the posted bytes no longer preview at all", async () => {
    const { status, body } = await post({
      intent: "create",
      document: "not a document",
      name: "garbage",
    });
    expect(status).toBe(400);
    expect(body.preview).toBeUndefined();
  });

  it("refuses an empty payload rather than writing nothing down", async () => {
    const { status, body } = await post({ intent: "create", document: "", name: "empty" });
    expect(status).toBe(400);
    expect(body.error).toBe("Nothing to add — run the preview again.");
  });
});

/**
 * WHAT THE ACT DID TO THE REACH. A curated channel withholds a MEMBER's placement — the default
 * `everyone` included — so the bundle lands in the workspace and reaches nobody. The dialog
 * promises that a chosen channel's agents get the address, so the outcome has to survive the
 * redirect and be said on the page it lands on.
 *
 * Every case here NAMES its destination: with no channel chosen there is no reach to withhold and
 * nothing to disclose.
 */
describe("a withheld placement is disclosed", () => {
  async function defaultChannelMode(mode: "open" | "curated"): Promise<void> {
    await db.q(`UPDATE web.channel SET mode = $2 WHERE workspace_id = $1 AND is_default`, [
      wsId,
      mode,
    ]);
  }

  it("carries the withheld outcome and the channel through the redirect", async () => {
    await seedServer("mcps_withheld", "io.github.acme/withheld");
    await defaultChannelMode("curated");
    session = { user: MEMBER };
    try {
      const { status, location } = await post({
        intent: "connect",
        server_id: "mcps_withheld",
        name: "withheld",
        channel: "everyone",
      });
      expect(status).toBe(302);
      expect(location).toBe("/mcp/withheld?placement=curated_role_required&channel=everyone");
      // The connection itself still landed — reach is curated, existence is not.
      expect((await bundleNamed("withheld"))?.kind).toBe("mcp");
      const placed = await db.q<{ n: number }>(
        `SELECT count(*)::int AS n FROM web.channel_bundle cb
         JOIN web.bundle b ON b.id = cb.bundle_id WHERE b.name = 'withheld'`,
      );
      expect(Number(placed[0]?.n)).toBe(0);
    } finally {
      await defaultChannelMode("open");
    }
  });

  it("says nothing when the placement actually happened", async () => {
    await seedServer("mcps_placed", "io.github.acme/placed");
    const { status, location } = await post({
      intent: "connect",
      server_id: "mcps_placed",
      name: "placed-here",
      channel: "everyone",
    });
    expect(status).toBe(302);
    expect(location).toBe("/mcp/placed-here");
  });

  /**
   * A CURATED DEFAULT CHANNEL WITHHOLDS NOTHING FROM AN ACT THAT ASKED FOR NO CHANNEL. The bundle
   * lands either way, but the redirect must be plain: a note about a placement nobody requested
   * would read as a failure where there was none.
   */
  it("says nothing when no channel was chosen, curated default or not", async () => {
    await seedServer("mcps_unplaced", "io.github.acme/unplaced");
    await defaultChannelMode("curated");
    session = { user: MEMBER };
    try {
      const { status, location } = await post({
        intent: "connect",
        server_id: "mcps_unplaced",
        name: "unplaced",
        channel: "",
      });
      expect(status).toBe(302);
      expect(location).toBe("/mcp/unplaced");
    } finally {
      await defaultChannelMode("open");
    }
  });

  it("names the withheld channel on the page the redirect lands on", async () => {
    const { placementNote } = await import("@/routes/skill-current");
    expect(placementNote("curated_role_required", "release-eng")).toBe(
      "Published to the catalog — placing it into #release-eng takes a reviewer or owner.",
    );
    expect(placementNote("channel_not_found", "gone")).toBe(
      "Published to the catalog — #gone was not there to place it into.",
    );
    // The parameters are forgeable, so an unknown outcome renders nothing and a channel name
    // this app would never mint is never echoed back into the page.
    expect(placementNote("placed", "everyone")).toBeNull();
    expect(placementNote(null, null)).toBeNull();
    expect(placementNote("curated_role_required", "<script>alert(1)</script>")).toBe(
      "Published to the catalog — placing it into that channel takes a reviewer or owner.",
    );
  });

  it("tells a member which destinations would withhold the placement", async () => {
    const { channelOptionLabel } = await import("@/routes/mcp-new");
    const curated = { name: "release-eng", mode: "curated" };
    const open = { name: "everyone", mode: "open" };
    expect(channelOptionLabel(curated, "release-eng", "member")).toBe(
      "release-eng — curated; placement needs a reviewer",
    );
    // A reviewer or owner places into a curated channel freely — nothing extra to say.
    expect(channelOptionLabel(curated, "release-eng", "reviewer")).toBe("release-eng");
    expect(channelOptionLabel(curated, "release-eng", "owner")).toBe("release-eng");
    expect(channelOptionLabel(open, "everyone", "member")).toBe("everyone");
  });
});

describe("the SSRF guard", () => {
  const addresses =
    (...list: string[]) =>
    async () =>
      list.map((address) => ({ address, family: address.includes(":") ? 6 : 4 }));

  it.each([
    ["loopback v4", "127.0.0.1"],
    ["the cloud metadata address", "169.254.169.254"],
    ["private 10/8", "10.1.2.3"],
    ["private 172.16/12", "172.20.0.5"],
    ["private 192.168/16", "192.168.1.10"],
    ["carrier-grade NAT", "100.64.0.1"],
    ["loopback v6", "::1"],
    ["unique-local v6", "fd00::1"],
    ["link-local v6", "fe80::1"],
    ["a v4-mapped private v6", "::ffff:10.0.0.1"],
  ])("refuses %s", async (_label, address) => {
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    await expect(
      assertPublicHttpsUrl("https://internal.example/server.json", addresses(address)),
    ).rejects.toThrow(/private|not reachable/i);
  });

  it("refuses a host that answers with a public AND a private address (rebinding)", async () => {
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    await expect(
      assertPublicHttpsUrl(
        "https://mixed.example/server.json",
        addresses("93.184.216.34", "10.0.0.7"),
      ),
    ).rejects.toThrow(/private/i);
  });

  it("refuses anything but https, and refuses credentials in the URL", async () => {
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    await expect(
      assertPublicHttpsUrl("http://example.com/server.json", addresses("93.184.216.34")),
    ).rejects.toThrow();
    await expect(
      assertPublicHttpsUrl("https://user:pass@example.com/s.json", addresses("93.184.216.34")),
    ).rejects.toThrow();
  });

  it("refuses a host that does not resolve at all", async () => {
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    await expect(
      assertPublicHttpsUrl("https://nowhere.example/server.json", async () => {
        throw new Error("ENOTFOUND");
      }),
    ).rejects.toThrow();
  });

  it("allows a public v6 address written in its compressed form", async () => {
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    const vetted = await assertPublicHttpsUrl(
      "https://v6.example/server.json",
      addresses("2606:2800:220:1:248:1893:25c8:1946"),
    );
    expect(vetted.url.hostname).toBe("v6.example");
  });

  it("allows an ordinary public address", async () => {
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    const vetted = await assertPublicHttpsUrl(
      "https://example.com/server.json",
      addresses("93.184.216.34"),
    );
    expect(vetted.url.protocol).toBe("https:");
  });

  it("hands back the addresses it proved public, which are the ones the fetch dials", async () => {
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    const vetted = await assertPublicHttpsUrl(
      "https://example.com/server.json",
      addresses("93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"),
    );
    expect(vetted.addresses.map((a) => a.address)).toEqual([
      "93.184.216.34",
      "2606:2800:220:1:248:1893:25c8:1946",
    ]);
  });
});

/** The loader payload the page renders from — the same shape the real loader hands over. */
function pageData(servers: Record<string, unknown>[]) {
  return {
    wsName: "acme",
    channels: [{ name: "everyone", isDefault: true, mode: "open" }],
    role: "owner",
    servers,
  };
}

function renderWith(loaderData: unknown, Component: () => ReturnType<typeof createElement>) {
  const routes: RouteObject[] = [{ path: "/", loader: () => loaderData, Component }];
  const handler = createStaticHandler(routes);
  return (async () => {
    const context = await handler.query(new Request("http://localhost/"));
    if (context instanceof Response) {
      throw new Error("expected a rendered context, got a Response");
    }
    const router = createStaticRouter(handler.dataRoutes, context);
    return renderToStaticMarkup(createElement(StaticRouterProvider, { router, context }));
  })();
}

/**
 * WHAT THE PICKER'S CARDS SAY BEFORE ANYONE CLICKS. The list comes down with the page, so the page
 * is rendered whole here and read for the one thing a card must not leave for later: that this
 * server costs a person a step no agent can take for it — and for the one thing it must not offer
 * twice, a server this workspace already runs.
 */
describe("the picker, on the page", () => {
  const catalogRow = (over: Record<string, unknown>) => ({
    serverId: "mcps_x",
    registryName: "io.github.acme/x",
    displayName: "Acme X",
    description: "A server for the suite.",
    icon: null,
    authMode: null,
    authNote: null,
    url: "https://acme.example/mcp",
    transport: "streamable-http",
    host: "acme.example",
    suggestedName: "x",
    connectedAs: null,
    ...over,
  });

  async function renderPage(servers: Record<string, unknown>[]): Promise<string> {
    const McpNew = (await import("@/routes/mcp-new")).default;
    return await renderWith(pageData(servers), McpNew as () => ReturnType<typeof createElement>);
  }

  /** How many times a marker appears — the count IS the assertion for a per-row element. */
  const times = (html: string, needle: string) => html.split(needle).length - 1;

  it("draws one card per server, every one of them offered without a round trip", async () => {
    const html = await renderPage([
      catalogRow({ serverId: "mcps_a", displayName: "Alpha" }),
      catalogRow({ serverId: "mcps_b", displayName: "Beta" }),
    ]);
    expect(times(html, 'data-testid="mcp-picker-option"')).toBe(2);
    expect(html).toContain("2 servers");
  });

  it("marks a manual row with the chip alone and keeps the errand's sentence off the card", async () => {
    const html = await renderPage([
      catalogRow({
        serverId: "mcps_m",
        displayName: "Manual One",
        authMode: "manual",
        authNote: "Mint a token in the vendor console first.",
      }),
    ]);
    expect(times(html, ">manual setup<")).toBe(1);
    // The sentence lives in the pick dialog, where the person is actually deciding — a paragraph
    // per manual row would out-shout its neighbours. The note still rides the page's data payload
    // (the dialog opens without a round trip), so the assertion scopes to the card itself.
    const card = html.split('data-testid="mcp-picker-option"')[1] ?? "";
    expect(card.slice(0, card.indexOf("</button>"))).not.toContain(
      "Mint a token in the vendor console first.",
    );
  });

  it("says nothing about a server whose sign-in nobody established", async () => {
    const html = await renderPage([catalogRow({ authMode: null })]);
    expect(html).not.toContain("no sign-in");
    expect(html).not.toContain(">oauth<");
    expect(html).not.toContain(">manual setup<");
  });

  it("offers a server this workspace already runs as a link to it, not as a second add", async () => {
    const html = await renderPage([catalogRow({ connectedAs: "acme-x" })]);
    expect(html).not.toContain('data-testid="mcp-picker-option"');
    expect(html).toContain('data-testid="mcp-picker-added"');
    expect(html).toContain('href="/mcp/acme-x"');
    expect(html).toContain(">added<");
  });
});

describe("the preview card", () => {
  async function renderPreview(
    summary: Record<string, unknown>,
    document = JSON.stringify(WEATHER, null, 2),
  ): Promise<string> {
    const { PreviewCard } = await import("@/routes/mcp-new");
    type Preview = Parameters<typeof PreviewCard>[0]["preview"];
    const preview = {
      form: "preview",
      origin: "https://weather.acme.example/server.json",
      suggestedName: "weather",
      document,
      summary,
    } as unknown as Preview;
    return await renderWith(pageData([]), () => createElement(PreviewCard, { preview }));
  }

  const remoteSummary = {
    name: WEATHER.name,
    description: WEATHER.description,
    version: WEATHER.version,
    url: WEATHER.remotes[0]?.url ?? "",
    transport: "streamable-http",
    headers: [],
    packages: [],
    authHint: null,
  };

  it("shows the address the document places, and the transport it speaks", async () => {
    const html = await renderPreview(remoteSummary);
    expect(html).toContain("https://weather.acme.example/mcp");
    expect(html).toContain("streamable-http");
  });

  /**
   * A PACKAGE-ONLY DOCUMENT has no address, and the card must not leave the address line standing
   * empty where one would be: it names what each machine installs instead.
   */
  it("shows what a package-only document installs, and no empty address line", async () => {
    const html = await renderPreview(
      {
        name: "io.github.acme/files",
        description: "Files the agent is pointed at, over stdio.",
        version: "2.1.0",
        url: null,
        transport: null,
        headers: [],
        packages: [
          {
            registryType: "npm",
            identifier: "@acme/mcp-files",
            version: "2.1.0",
            transport: "stdio",
          },
        ],
        authHint: null,
      },
      // The document itself is what the card discloses, so a package-only case carries none of
      // the remote document's text — otherwise the assertions below would read its leftovers.
      "{}",
    );
    expect(html).toContain("npm @acme/mcp-files 2.1.0");
    expect(html).toContain("stdio");
    // No address, so no address element at all — and no transport chip claiming one.
    expect(html).not.toContain("mcp-preview-url");
    expect(html).not.toContain("streamable-http");
  });

  /**
   * THE RESTING DESTINATION. What an untouched form posts is the whole question here: it must be
   * the empty value, and the empty value must be spelled on the page as no channel — otherwise
   * the field looks optional while quietly handing the server to the workspace.
   */
  it("rests on no channel, with the default one an ordinary named option", async () => {
    const html = await renderPreview(remoteSummary);
    expect(html).toContain('<option value="" selected="">No channel</option>');
    expect(html).toContain('<option value="everyone">everyone (everyone here)</option>');
    expect(html).toContain("Optional — a channel is how it reaches people.");
  });
});
