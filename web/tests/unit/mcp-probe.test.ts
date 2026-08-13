import { Buffer } from "node:buffer";
import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  classifyProbeAnswer,
  jsonPayloadsOf,
  type ProbeTransport,
  probeAndRecord,
  probeEndpoint,
} from "@/lib/mcp/probe.server";
import { probeStateLine } from "@/lib/mcp/probe-state";
import {
  asSession,
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
 * THE ADVISORY PROBE — what the plane concludes from one answer, and what it does with it.
 *
 * Two halves, tested apart: the CLASSIFICATION is pure (a status, some headers, a body → one of
 * four words) and needs no socket; the RECORDING is a row, and needs a database. Nothing here
 * touches the network — the transport is injected, and every address a guard test uses is a
 * literal, so no name is ever resolved.
 *
 * The property this suite exists to protect: a probe is a report, never a gate. A publish that
 * has landed stays landed whatever the probe meets — a hostile endpoint, an unreachable one, a
 * row the database refuses.
 */

const OK_INITIALIZE = JSON.stringify({
  jsonrpc: "2.0",
  id: 1,
  result: { protocolVersion: "2025-11-25", serverInfo: { name: "acme", version: "1" } },
});

/** An answer the injected transport hands back — a real Response, so nothing is faked twice. */
function answer(
  status: number,
  body: string,
  headers: Record<string, string> = {},
): ProbeTransport {
  return async () => new Response(body, { status, headers });
}

describe("what one answer means", () => {
  it("reads 401 and 403 as a healthy server asking for a sign-in", () => {
    expect(
      classifyProbeAnswer({
        status: 401,
        wwwAuthenticate: 'Bearer resource_metadata="https://acme.example/.well-known"',
        contentType: "application/json",
        body: '{"error":"unauthorized"}',
      }),
    ).toEqual({ outcome: "sign_in_required", detail: "401 with a sign-in challenge" });
    // No challenge header: still a server demanding credentials, and the detail says which.
    expect(
      classifyProbeAnswer({
        status: 403,
        wwwAuthenticate: null,
        contentType: "text/html",
        body: "<html>forbidden</html>",
      }),
    ).toEqual({ outcome: "sign_in_required", detail: "403, no challenge header" });
  });

  it("lets 401 outrank the body, whatever the body says", () => {
    // A body that WOULD parse as a good initialize does not turn a 401 into anything else, and a
    // body that parses as nothing does not turn it into a fault.
    for (const body of [OK_INITIALIZE, "<html>sign in</html>", ""]) {
      expect(
        classifyProbeAnswer({
          status: 401,
          wwwAuthenticate: "Bearer",
          contentType: "application/json",
          body,
        }).outcome,
      ).toBe("sign_in_required");
    }
  });

  it("never reads a 5xx or a 429 as a protocol verdict", () => {
    for (const status of [500, 502, 503, 429]) {
      const verdict = classifyProbeAnswer({
        status,
        wwwAuthenticate: null,
        contentType: "text/html",
        body: "<html>oops</html>",
      });
      expect(verdict).toEqual({ outcome: "not_responding", detail: `answered ${status}` });
    }
  });

  it("records a redirect as not responding rather than following it", () => {
    expect(
      classifyProbeAnswer({
        status: 302,
        wwwAuthenticate: null,
        contentType: null,
        body: "",
      }),
    ).toEqual({ outcome: "not_responding", detail: "redirected (302), not followed" });
  });

  it("reads a correlated initialize result as responding", () => {
    expect(
      classifyProbeAnswer({
        status: 200,
        wwwAuthenticate: null,
        contentType: "application/json",
        body: OK_INITIALIZE,
      }),
    ).toEqual({ outcome: "responding", detail: null });
  });

  it("reads the SSE framing of the same answer the same way", () => {
    const stream = [
      ": keep-alive",
      "event: message",
      "id: 42",
      `data: ${OK_INITIALIZE}`,
      "",
      "",
    ].join("\n");
    expect(
      classifyProbeAnswer({
        status: 200,
        wwwAuthenticate: null,
        contentType: "text/event-stream; charset=utf-8",
        body: stream,
      }),
    ).toEqual({ outcome: "responding", detail: null });
  });

  it("counts a JSON-RPC error to our own initialize as an answering server", () => {
    const verdict = classifyProbeAnswer({
      status: 200,
      wwwAuthenticate: null,
      contentType: "application/json",
      body: JSON.stringify({
        jsonrpc: "2.0",
        id: 1,
        error: { code: -32602, message: "unsupported protocol version" },
      }),
    });
    // It IS an MCP server, and it IS talking — which is the whole question the catalog asks.
    expect(verdict).toEqual({ outcome: "responding", detail: "protocol error -32602" });
  });

  it("refuses to accept an uncorrelated payload as an answer", () => {
    for (const body of [
      JSON.stringify({ jsonrpc: "2.0", id: 7, result: {} }),
      JSON.stringify({ id: 1, result: {} }),
      JSON.stringify({ jsonrpc: "2.0", id: 1 }),
      "<html>hello</html>",
      "",
    ]) {
      expect(
        classifyProbeAnswer({
          status: 200,
          wwwAuthenticate: null,
          contentType: "application/json",
          body,
        }),
      ).toEqual({ outcome: "not_responding", detail: "answered 200, not as an MCP server" });
    }
  });

  it("reads the other 4xx as an answer that is not this protocol", () => {
    for (const status of [400, 404, 405]) {
      expect(
        classifyProbeAnswer({
          status,
          wwwAuthenticate: null,
          contentType: "text/html",
          body: "<html/>",
        }),
      ).toEqual({ outcome: "not_responding", detail: `answered ${status}` });
    }
  });
});

describe("the event-stream reader", () => {
  it("takes multi-line data, several events, and ignores comments and other fields", () => {
    const body = [
      ": a comment",
      "retry: 1000",
      "event: message",
      'data: {"a":',
      "data: 1}",
      "",
      'data: {"b":2}',
      "",
    ].join("\n");
    expect(jsonPayloadsOf(body, "text/event-stream")).toEqual([{ a: 1 }, { b: 2 }]);
  });

  it("drops an event that never finished arriving, and keeps the ones that did", () => {
    const body = ['data: {"a":1}', "", 'data: {"b":'].join("\n");
    expect(jsonPayloadsOf(body, "text/event-stream")).toEqual([{ a: 1 }]);
  });

  it("reads a plain JSON body as one payload", () => {
    expect(jsonPayloadsOf('{"a":1}', "application/json")).toEqual([{ a: 1 }]);
    expect(jsonPayloadsOf("not json", "application/json")).toEqual([]);
  });
});

describe("asking one endpoint", () => {
  it("calls nothing at all for an address the guard will not vet, and says which kind", async () => {
    const transport = vi.fn<ProbeTransport>();
    // A private address: an internal server is a first-class thing to share, so this is NEUTRAL.
    expect(await probeEndpoint("https://127.0.0.1/mcp", transport)).toEqual({
      outcome: "not_verifiable",
      detail: "private address",
    });
    // A name nothing here resolves is the OTHER neutral fact, and points somewhere else entirely:
    // calling it a private address sends a reader looking for a firewall that is not there.
    const unresolvable = async () => {
      throw new Error("ENOTFOUND");
    };
    expect(await probeEndpoint("https://nope.acme.example/mcp", transport, unresolvable)).toEqual({
      outcome: "not_verifiable",
      detail: "name does not resolve here",
    });
    // A refusal about the URL's SHAPE claims neither (the document gate refuses these anyway).
    expect(await probeEndpoint("http://93.184.216.34/mcp", transport)).toEqual({
      outcome: "not_verifiable",
      detail: null,
    });
    expect(transport).not.toHaveBeenCalled();
  });

  it("reads silence as an outage, and says which kind", async () => {
    const timeout = async () => {
      const error = new Error("timed out");
      error.name = "TimeoutError";
      throw error;
    };
    expect(await probeEndpoint("https://93.184.216.34/mcp", timeout)).toEqual({
      outcome: "not_responding",
      detail: "no answer in time",
    });
    const refused: ProbeTransport = async () => {
      throw new Error("ECONNREFUSED");
    };
    expect(await probeEndpoint("https://93.184.216.34/mcp", refused)).toEqual({
      outcome: "not_responding",
      detail: "no answer",
    });
  });

  it("classifies what an answering endpoint said", async () => {
    expect(
      await probeEndpoint(
        "https://93.184.216.34/mcp",
        answer(200, OK_INITIALIZE, { "content-type": "application/json" }),
      ),
    ).toEqual({ outcome: "responding", detail: null });
    expect(
      await probeEndpoint(
        "https://93.184.216.34/mcp",
        answer(401, "no", { "www-authenticate": "Bearer" }),
      ),
    ).toMatchObject({ outcome: "sign_in_required" });
  });
});

describe("the line a catalog shows", () => {
  it("says exactly what was seen, and says nothing when nothing was", () => {
    const probedAt = "2026-08-12T09:41:00.000Z";
    expect(probeStateLine(null)).toBe("not checked yet");
    expect(probeStateLine({ outcome: "responding", probedAt, detail: null })).toBe(
      "responding, checked 12 Aug 2026",
    );
    expect(probeStateLine({ outcome: "sign_in_required", probedAt, detail: null })).toBe(
      "sign-in required, checked 12 Aug 2026",
    );
    // The two things "not verifiable" can mean are two different next moves for a reader, so the
    // line says which one it saw — and says neither when the reason was not one of them.
    expect(probeStateLine({ outcome: "not_verifiable", probedAt, detail: "private address" })).toBe(
      "not verifiable from cloud (private address)",
    );
    expect(
      probeStateLine({
        outcome: "not_verifiable",
        probedAt,
        detail: "name does not resolve here",
      }),
    ).toBe("not verifiable from cloud (name does not resolve here)");
    expect(probeStateLine({ outcome: "not_verifiable", probedAt, detail: null })).toBe(
      "not verifiable from cloud",
    );
    expect(probeStateLine({ outcome: "not_responding", probedAt, detail: null })).toBe(
      "not responding when checked 12 Aug 2026",
    );
  });
});

// ── The row, and the promise that writing it can never cost a publish ────────────────────────

let signedIn: { user: { id: string; name: string; email: string } } | null = null;
vi.mock("@/lib/auth/server", () => ({
  getAuth: () => ({ api: { getSession: async () => signedIn } }),
}));

let db: ScratchDb;
let vault: StubVault;
let wsId = "";
const MEMBER = { id: "u_probe", name: "Prober", email: "prober@example.com" };

beforeAll(async () => {
  vault = await startStubVault();
  db = await createScratchDb("web_mcp_probe", { PLANE_INTERNAL_URL: vault.url });
  wsId = await bootWorkspace();
  await seedUser(db, MEMBER.id, MEMBER.name, MEMBER.email);
  await seatUser(db, wsId, MEMBER.id, "owner");
  await seedSession(db, "cs_probe", wsId, MEMBER.id);
}, 60000);

afterAll(async () => {
  await vault.close();
  await db.drop();
});

async function probeRowsOf(bundleId: string) {
  return await db.q<{ version_id: string; outcome: string; detail: string | null }>(
    "SELECT version_id, outcome, detail FROM web.mcp_probe WHERE bundle_id = $1",
    [bundleId],
  );
}

describe("recording what was seen", () => {
  it("writes one row per version, and a re-probe replaces its own answer", async () => {
    const actor = asSession(wsId, MEMBER.id, "cs_probe", "owner");
    await seedBundle(db, wsId, "s_probe_one", "weather", { kind: "mcp" });
    const versionId = versionIdFor("s_probe_one");
    const target = { bundleId: "s_probe_one", versionId, endpoint: "https://93.184.216.34/mcp" };

    await probeAndRecord(actor, target, answer(200, OK_INITIALIZE));
    expect(await probeRowsOf("s_probe_one")).toEqual([
      { version_id: versionId, outcome: "responding", detail: null },
    ]);

    await probeAndRecord(actor, target, answer(503, "down"));
    expect(await probeRowsOf("s_probe_one")).toEqual([
      { version_id: versionId, outcome: "not_responding", detail: "answered 503" },
    ]);

    const { mcpProbeFor } = await import("@/lib/db/queries.mcp.server");
    const read = await mcpProbeFor(actor, "s_probe_one", versionId);
    expect(read?.outcome).toBe("not_responding");
    expect(probeStateLine(read)).toMatch(/^not responding when checked /);
  });

  it("asks nothing about a package-only bundle, and records nothing for it", async () => {
    const actor = asSession(wsId, MEMBER.id, "cs_probe", "owner");
    await seedBundle(db, wsId, "s_probe_pkg", "files", { kind: "mcp" });
    const transport = vi.fn<ProbeTransport>();
    const verdict = await probeAndRecord(
      actor,
      { bundleId: "s_probe_pkg", versionId: versionIdFor("s_probe_pkg"), endpoint: null },
      transport,
    );
    expect(verdict).toBe(null);
    expect(transport).not.toHaveBeenCalled();
    expect(await probeRowsOf("s_probe_pkg")).toEqual([]);
  });

  it("swallows a storage failure whole — the publish it follows is already durable", async () => {
    const actor = asSession(wsId, MEMBER.id, "cs_probe", "owner");
    // No such bundle: the row's foreign key refuses it, which is a database error raised INSIDE
    // the probe. It must never surface — by the time this runs, a publish has already succeeded
    // and there is nothing left for an exception to do but break something that worked.
    await expect(
      probeAndRecord(
        actor,
        {
          bundleId: "s_probe_missing",
          versionId: versionIdFor("s_probe_missing"),
          endpoint: "https://93.184.216.34/mcp",
        },
        answer(200, OK_INITIALIZE),
      ),
    ).resolves.toBe(null);
  });

  it("rides a real genesis publish, after it has landed and without holding it up", async () => {
    // THE HOOK ITSELF, through the shared genesis path every door takes. The document names a
    // PRIVATE address, so the probe's own guard answers without any socket being opened — and the
    // publish must be finished and successful before that answer exists at all.
    const { publishGenesisBundle } = await import("@/lib/api/genesis.server");
    const document = `${JSON.stringify(
      {
        name: "io.github.acme/internal",
        description: "A server inside the network this workspace runs in.",
        version: "1.0.0",
        remotes: [{ type: "streamable-http", url: "https://127.0.0.1/mcp" }],
      },
      null,
      2,
    )}\n`;
    const landed = await publishGenesisBundle({
      actor: asSession(wsId, MEMBER.id, "cs_probe", "owner"),
      kind: "mcp",
      bundleId: "s_probe_genesis",
      candidate: {
        files: [
          {
            path: "server.json",
            mode: "100644",
            content_base64: Buffer.from(document, "utf8").toString("base64"),
          },
        ],
        attribution: MEMBER.name,
        message: "imported MCP server io.github.acme/internal",
      },
      displayName: "internal",
      destination: null,
    });
    // The publish is decided here, with nothing waited on.
    expect(landed.kind).toBe("ok");

    // …and the report arrives on its own clock, saying the neutral thing about an address this
    // plane cannot reach rather than anything about the server.
    let rows = await probeRowsOf("s_probe_genesis");
    for (let attempt = 0; attempt < 40 && rows.length === 0; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 50));
      rows = await probeRowsOf("s_probe_genesis");
    }
    expect(rows).toEqual([
      {
        version_id: landed.kind === "ok" ? landed.versionId : "",
        outcome: "not_verifiable",
        detail: "private address",
      },
    ]);
  });

  it("swallows a transport that throws something unexpected", async () => {
    const actor = asSession(wsId, MEMBER.id, "cs_probe", "owner");
    await seedBundle(db, wsId, "s_probe_boom", "boom", { kind: "mcp" });
    const exploding: ProbeTransport = () => {
      throw new TypeError("not a function");
    };
    await expect(
      probeAndRecord(
        actor,
        {
          bundleId: "s_probe_boom",
          versionId: versionIdFor("s_probe_boom"),
          endpoint: "https://93.184.216.34/mcp",
        },
        exploding,
      ),
    ).resolves.toEqual({ outcome: "not_responding", detail: "no answer" });
  });
});

describe("what the bundle page carries", () => {
  /** The face's loader, for a bundle addressed under the MCP base. */
  async function faceOf(name: string, base: "mcp" | "skills") {
    const { loader } = await import("@/routes/skill-current");
    return (await loader({
      request: new Request(`http://x/${base}/${name}`),
      params: base === "mcp" ? { server: name } : { skill: name },
      context: {},
    } as never)) as { probeLine: string | null };
  }

  it("says what the probe saw, and says the honest thing when nothing was recorded", async () => {
    signedIn = { user: MEMBER };
    const actor = asSession(wsId, MEMBER.id, "cs_probe", "owner");
    await seedBundle(db, wsId, "s_face_probed", "probed", { kind: "mcp" });
    await seedBundle(db, wsId, "s_face_quiet", "unprobed", { kind: "mcp" });
    await seedBundle(db, wsId, "s_face_skill", "runbook", { kind: "skill" });
    await probeAndRecord(
      actor,
      {
        bundleId: "s_face_probed",
        versionId: versionIdFor("s_face_probed"),
        endpoint: "https://93.184.216.34/mcp",
      },
      answer(401, "no", { "www-authenticate": "Bearer" }),
    );

    expect((await faceOf("probed", "mcp")).probeLine).toMatch(/^sign-in required, checked /);
    // A version nobody asked about — a fact about this plane, never about the server.
    expect((await faceOf("unprobed", "mcp")).probeLine).toBe("not checked yet");
    // A skill has no server to ask about, so it carries no line at all.
    expect((await faceOf("runbook", "skills")).probeLine).toBe(null);
    signedIn = null;
  });
});
