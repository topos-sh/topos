import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import {
  bootWorkspace,
  createScratchDb,
  type ScratchDb,
  seatUser,
  seedBundle,
  seedUser,
  versionIdFor,
} from "./helpers/scratch-db";
import { type StubVault, startStubVault } from "./helpers/stub-vault";

/**
 * THE ADD-AN-MCP-SERVER PAGE — its two phases against a real scratch Postgres, the app's one
 * custody transport re-pointed at an in-process stub vault, and the fetch seam replaced so no
 * test touches the network.
 *
 * The three CUSTOM sources are one code path by design: whatever the bytes came from, the preview
 * canonicalizes them and runs the same gate the session lane runs, and the publish arm runs it
 * AGAIN on the bytes the form posted back — the form is a client, and a client's word is not the
 * gate. That second run is what these tests lean on hardest.
 *
 * The PICKER never previews at all — its documents are committed data the loader ships, so the
 * dialog answers on the click. What it posts is an ID, and the publish arm looks the bytes up on
 * this side: the tests below assert exactly that, including that a document smuggled alongside
 * the id is ignored.
 *
 * The SSRF guard is exercised directly, with the resolver mocked: what matters is which
 * ADDRESSES it refuses, and a test that needed real DNS to say so would be testing DNS.
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
let vault: StubVault;
let wsId = "";

const ORIGIN = "http://x";

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

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_mcp_import", { PLANE_INTERNAL_URL: vault.url });
  wsId = await bootWorkspace();
  await seedUser(db, "u_mem", "Member", "mem@example.com");
  await seatUser(db, wsId, "u_mem", "member");
  session = { user: { id: "u_mem", name: "Member", email: "mem@example.com" } };
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

beforeEach(() => {
  fetched.fail = null;
  fetched.calls.length = 0;
  vault.calls.length = 0;
  vault.published.length = 0;
});

describe("the preview", () => {
  it("reads a REGISTRY answer, unwrapping the { server, _meta } envelope", async () => {
    fetched.text = JSON.stringify({ server: WEATHER, _meta: { official: { status: "active" } } });
    const { body } = await post({
      intent: "preview",
      source: "registry",
      registry_name: "io.github.acme/weather",
    });
    expect(fetched.calls).toEqual([{ kind: "registry", value: "io.github.acme/weather" }]);
    expect(body.summary).toMatchObject({
      name: "io.github.acme/weather",
      url: "https://weather.acme.example/mcp",
      transport: "streamable-http",
    });
    expect(body.suggestedName).toBe("weather");
    // The stored bytes are the SERVER document alone — the registry's own envelope is dropped.
    expect(JSON.parse(String(body.document))).toEqual(WEATHER);
    // Nothing was written by a preview.
    expect(vault.calls).toEqual([]);
  });

  it("reads a bare document from a URL", async () => {
    fetched.text = JSON.stringify(WEATHER);
    const { body } = await post({
      intent: "preview",
      source: "url",
      url: "https://example.test/server.json",
    });
    expect(fetched.calls[0]?.kind).toBe("url");
    expect(body.summary).toMatchObject({ name: "io.github.acme/weather" });
  });

  it("reads a PASTED document without any fetch at all", async () => {
    const { body } = await post({
      intent: "preview",
      source: "paste",
      document: JSON.stringify(WEATHER),
    });
    expect(body.summary).toMatchObject({ name: "io.github.acme/weather" });
    expect(body.origin).toBe("pasted");
  });

  it("surfaces a fetch failure as the fetcher worded it", async () => {
    fetched.fail = "that host is on a private network — this server will not fetch it";
    const { status, body } = await post({
      intent: "preview",
      source: "url",
      url: "https://internal.test/server.json",
    });
    expect(status).toBe(400);
    expect(String(body.error)).toContain("private network");
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
    const { status } = await post({ intent: "preview", source: "url", url: "  " });
    expect(status).toBe(400);
    expect(fetched.calls).toEqual([]);
  });

  it("has no arm for a picked row at all — the picker never previews", async () => {
    const { CURATED_MCP_SERVERS } = await import("@/lib/mcp/curated.server");
    const entry = CURATED_MCP_SERVERS[0];
    expect(entry).toBeDefined();
    // `curated` was a source once. It is gone: the built-in list's documents ship with the page,
    // so there is nothing here to ask, and this arm must not quietly come back.
    const { status } = await post({
      intent: "preview",
      source: "curated",
      server: entry?.name ?? "",
    });
    expect(status).toBe(400);
    expect(fetched.calls).toEqual([]);
    expect(vault.calls).toEqual([]);
  });
});

describe("the publish", () => {
  it("mints an mcp bundle whose one file is the canonical server.json", async () => {
    const document = `${JSON.stringify(WEATHER, null, 2)}\n`;
    const { status, location } = await post({
      intent: "publish",
      document,
      name: "weather",
      channel: "",
    });
    // The arm lands by redirect to the new server's page — in the MCP section, never under
    // /skills: the kind decides which base addresses a bundle.
    expect(status).toBe(302);
    expect(location).toBe("/mcp/weather");

    expect(vault.published).toHaveLength(1);
    expect(vault.published[0]?.files).toEqual([{ path: "server.json", content: document }]);

    const rows = await db.q<{ id: string; kind: string; name: string }>(
      `SELECT id, kind, name FROM web.bundle WHERE name = 'weather'`,
    );
    expect(rows[0]?.kind).toBe("mcp");
    // The default placement still applies — an import is an ordinary genesis publish.
    const placed = await db.q<{ n: string }>(
      `SELECT count(*)::int AS n FROM web.channel_bundle cb
       JOIN web.channel c ON c.id = cb.channel_id AND c.is_default
       WHERE cb.bundle_id = $1`,
      [rows[0]?.id],
    );
    expect(Number(placed[0]?.n)).toBe(1);
    // And the act is on the record.
    const audit = await db.q<{ kind: string; details: Record<string, unknown> }>(
      `SELECT kind, details FROM web.audit_event WHERE kind = 'mcp_imported'`,
    );
    expect(audit[0]?.details).toMatchObject({
      server: "io.github.acme/weather",
      version: "1.4.0",
    });
  });

  it("re-runs the gate on the posted bytes — a doctored form is refused, nothing published", async () => {
    const { status, body } = await post({
      intent: "publish",
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
    expect(vault.published).toEqual([]);
  });

  it("refuses a name another bundle's document already claims", async () => {
    const versionId = versionIdFor("s_taken");
    await seedBundle(db, wsId, "s_taken", "tides", { kind: "mcp", versionId });
    vault.seed(wsId, "s_taken", versionId, [
      {
        path: "server.json",
        content: JSON.stringify({ ...WEATHER, name: "io.github.acme/tides" }, null, 2),
      },
    ]);
    const { status, body } = await post({
      intent: "publish",
      document: JSON.stringify({ ...WEATHER, name: "io.github.acme/tides" }, null, 2),
      name: "tides-again",
      channel: "",
    });
    expect(status).toBe(400);
    expect(body.code).toBe("MCP_NAME_TAKEN");
    expect(vault.published).toEqual([]);
  });

  it("refuses an empty payload rather than publishing nothing", async () => {
    const { status } = await post({ intent: "publish", document: "", name: "x", channel: "" });
    expect(status).toBe(400);
    expect(vault.published).toEqual([]);
  });
});

describe("publishing a picked row", () => {
  it("derives the bytes from the built-in list, not from the form", async () => {
    const { CURATED_MCP_SERVERS, curatedDocumentFor } = await import("@/lib/mcp/curated.server");
    const entry = CURATED_MCP_SERVERS[0];
    expect(entry).toBeDefined();
    const name = entry?.name ?? "";
    const { status, location } = await post({
      intent: "publish",
      server: name,
      name: entry?.slug ?? "",
      channel: "",
      // A document field alongside the id is IGNORED — the pick names a row, it does not carry
      // one, so a doctored form cannot publish bytes this list never held.
      document: JSON.stringify({ ...WEATHER, name: "io.github.acme/smuggled" }),
    });
    expect(status).toBe(302);
    expect(location).toBe(`/mcp/${entry?.slug}`);
    expect(vault.published).toHaveLength(1);
    expect(vault.published[0]?.files).toEqual([
      { path: "server.json", content: curatedDocumentFor(name) },
    ]);
    // No fetch was spent on it either: a pick reaches nothing outside this process.
    expect(fetched.calls).toEqual([]);

    const rows = await db.q<{ kind: string; name: string }>(
      `SELECT kind, name FROM web.bundle WHERE name = $1`,
      [entry?.slug],
    );
    expect(rows[0]?.kind).toBe("mcp");
    // The document's own embedded name is what the audit records, not the catalog slug.
    const audit = await db.q<{ details: Record<string, unknown> }>(
      `SELECT details FROM web.audit_event WHERE kind = 'mcp_imported' AND details->>'server' = $1`,
      [name],
    );
    expect(audit).toHaveLength(1);
  });

  it("refuses an id this list does not hold, rather than inventing a document", async () => {
    const { status, body } = await post({
      intent: "publish",
      server: "io.github.nobody/not-on-the-list",
      name: "nobody",
      channel: "",
    });
    expect(status).toBe(400);
    expect(String(body.error)).toContain("not one of the servers on this list");
    // The refusal is addressed to the dialog, and names the row it answers about — a page-level
    // answer would leave the open dialog silent.
    expect(body.form).toBe("pick");
    expect(body.server).toBe("io.github.nobody/not-on-the-list");
    expect(vault.published).toEqual([]);
  });

  it("sends a gate refusal back to the dialog that asked", async () => {
    const { CURATED_MCP_SERVERS } = await import("@/lib/mcp/curated.server");
    const entry = CURATED_MCP_SERVERS[1];
    expect(entry).toBeDefined();
    const name = entry?.name ?? "";
    const versionId = versionIdFor("s_picked_taken");
    // Another bundle already claims this server's embedded name — the live collision.
    await seedBundle(db, wsId, "s_picked_taken", "already-here", { kind: "mcp", versionId });
    vault.seed(wsId, "s_picked_taken", versionId, [
      { path: "server.json", content: JSON.stringify({ ...WEATHER, name }, null, 2) },
    ]);
    const { status, body } = await post({
      intent: "publish",
      server: name,
      name: entry?.slug ?? "",
      channel: "",
    });
    expect(status).toBe(400);
    expect(body.code).toBe("MCP_NAME_TAKEN");
    expect(body.form).toBe("pick");
    expect(body.server).toBe(name);
    expect(vault.published).toEqual([]);
  });
});

/**
 * WHAT THE PUBLISH DID TO THE REACH. A curated channel withholds a MEMBER's placement — the
 * default `everyone` included — so the bundle lands in the catalog and reaches nobody. The dialog
 * promises "every agent the channel reaches gets that address", so the outcome has to survive the
 * redirect and be said on the page it lands on; before this it was dropped on the floor.
 */
describe("a withheld placement is disclosed", () => {
  async function defaultChannelMode(mode: "open" | "curated"): Promise<void> {
    await db.q(`UPDATE web.channel SET mode = $2 WHERE workspace_id = $1 AND is_default`, [
      wsId,
      mode,
    ]);
  }

  it("carries the withheld outcome and the channel through the redirect", async () => {
    await defaultChannelMode("curated");
    try {
      const { status, location } = await post({
        intent: "publish",
        document: JSON.stringify({ ...WEATHER, name: "io.github.acme/withheld" }, null, 2),
        name: "withheld",
        channel: "",
      });
      expect(status).toBe(302);
      expect(location).toBe("/mcp/withheld?placement=curated_role_required&channel=everyone");
      // The publish itself still landed — custody is never curation-blocked.
      const rows = await db.q<{ kind: string }>(`SELECT kind FROM web.bundle WHERE name = $1`, [
        "withheld",
      ]);
      expect(rows[0]?.kind).toBe("mcp");
      // And it is in NO channel, which is exactly what the note says.
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
    const { status, location } = await post({
      intent: "publish",
      document: JSON.stringify({ ...WEATHER, name: "io.github.acme/placed" }, null, 2),
      name: "placed-here",
      channel: "",
    });
    expect(status).toBe(302);
    expect(location).toBe("/mcp/placed-here");
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
    ["this-network 0/8", "0.0.0.0"],
    ["multicast", "239.1.1.1"],
    ["loopback v6", "::1"],
    ["unique-local v6", "fd00::1"],
    ["link-local v6", "fe80::1"],
    ["a v4-mapped loopback", "::ffff:127.0.0.1"],
    // The same three addresses a resolver is just as free to hand back in HEX: `::ffff:7f00:1`
    // IS 127.0.0.1 to the socket layer, and reading the spelling instead of the address is
    // exactly how a guard lets one through.
    ["a v4-mapped loopback in hex", "::ffff:7f00:1"],
    ["a v4-mapped private 10/8 in hex", "::ffff:a00:1"],
    ["a v4-translated loopback (the NAT64-ish prefix)", "::ffff:0:7f00:1"],
    // The REAL NAT64 prefixes: a translator on the path turns these back into an IPv4
    // connection, so the whole v4 private range is reachable through an address the v4 rules
    // never see. Both the well-known /96 and the local-use /48 are refused entire.
    ["the well-known NAT64 prefix carrying the metadata address", "64:ff9b::a9fe:a9fe"],
    ["the well-known NAT64 prefix in dotted form", "64:ff9b::169.254.169.254"],
    ["the well-known NAT64 prefix carrying loopback", "64:ff9b::7f00:1"],
    ["the local-use NAT64 prefix", "64:ff9b:1::1"],
    ["the local-use NAT64 prefix, deeper in", "64:ff9b:1:2:3:4:5:6"],
  ])("refuses %s", async (_label, address) => {
    const { assertPublicHttpsUrl, McpFetchError } = await import("@/lib/mcp/fetch.server");
    await expect(
      assertPublicHttpsUrl("https://internal.test/server.json", addresses(address)),
    ).rejects.toBeInstanceOf(McpFetchError);
  });

  it("refuses a host that answers with a public AND a private address (rebinding)", async () => {
    const { assertPublicHttpsUrl, McpFetchError } = await import("@/lib/mcp/fetch.server");
    await expect(
      assertPublicHttpsUrl(
        "https://mixed.test/server.json",
        addresses("93.184.216.34", "127.0.0.1"),
      ),
    ).rejects.toBeInstanceOf(McpFetchError);
  });

  it("refuses anything but https, and refuses credentials in the URL", async () => {
    const { assertPublicHttpsUrl, McpFetchError } = await import("@/lib/mcp/fetch.server");
    const publicOnly = addresses("93.184.216.34");
    await expect(
      assertPublicHttpsUrl("http://example.test/server.json", publicOnly),
    ).rejects.toBeInstanceOf(McpFetchError);
    await expect(
      assertPublicHttpsUrl("https://user:pw@example.test/server.json", publicOnly),
    ).rejects.toBeInstanceOf(McpFetchError);
    await expect(assertPublicHttpsUrl("not a url", publicOnly)).rejects.toBeInstanceOf(
      McpFetchError,
    );
  });

  it("refuses a host that does not resolve at all", async () => {
    const { assertPublicHttpsUrl, McpFetchError } = await import("@/lib/mcp/fetch.server");
    await expect(
      assertPublicHttpsUrl("https://nowhere.test/server.json", async () => []),
    ).rejects.toBeInstanceOf(McpFetchError);
    await expect(
      assertPublicHttpsUrl("https://nowhere.test/server.json", async () => {
        throw new Error("ENOTFOUND");
      }),
    ).rejects.toBeInstanceOf(McpFetchError);
  });

  it("allows a public v6 address written in its compressed form", async () => {
    // The other half of folding v4-embedded shapes: an ordinary public v6 with a `::` run must
    // still parse and pass, or the guard would refuse most of the v6 internet as unreadable.
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    const vetted = await assertPublicHttpsUrl(
      "https://v6.test/server.json",
      addresses("2606:4700::6810:84e5"),
    );
    expect(vetted.url.host).toBe("v6.test");
  });

  it("allows an ordinary public address", async () => {
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    const vetted = await assertPublicHttpsUrl(
      "https://example.test/server.json",
      addresses("93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"),
    );
    expect(vetted.url.host).toBe("example.test");
  });

  it("hands back the addresses it proved public, which are the ones the fetch dials", async () => {
    // The rebinding window closes only if nothing resolves twice: the guard's answer carries the
    // vetted addresses so the connection is made to exactly them, family tags and all.
    const { assertPublicHttpsUrl } = await import("@/lib/mcp/fetch.server");
    const vetted = await assertPublicHttpsUrl(
      "https://example.test/server.json",
      addresses("93.184.216.34", "2606:2800:220:1:248:1893:25c8:1946"),
    );
    expect(vetted.addresses).toEqual([
      { address: "93.184.216.34", family: 4 },
      { address: "2606:2800:220:1:248:1893:25c8:1946", family: 6 },
    ]);
  });
});
