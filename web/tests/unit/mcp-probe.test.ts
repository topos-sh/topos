import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import {
  classifyProbeAnswer,
  jsonPayloadsOf,
  type ProbeTransport,
  probeAndRecord,
  probeEndpoint,
} from "@/lib/mcp/probe.server";
import { probeStateLine } from "@/lib/mcp/probe-state";
import { bootWorkspace, createScratchDb, type ScratchDb } from "./helpers/scratch-db";

/**
 * THE ADVISORY PROBE — what the plane concludes from one answer, and what it does with it.
 *
 * Two halves, tested apart: the CLASSIFICATION is pure (a status, some headers, a body → one of
 * four words) and needs no socket; the RECORDING is a row, and needs a database. Nothing here
 * touches the network — the transport is injected, and every address a guard test uses is a
 * literal, so no name is ever resolved.
 *
 * The property this suite exists to protect: a probe is a report, never a gate. The revision it
 * is about stands whatever the probe meets — a hostile endpoint, an unreachable one, a row that
 * is no longer there.
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

// ── The row, and the promise that writing it can never cost the act it follows ──────────────

let db: ScratchDb;
let wsId = "";

beforeAll(async () => {
  db = await createScratchDb("web_mcp_probe");
  wsId = await bootWorkspace();
}, 60000);

afterAll(async () => {
  await db.drop();
});

/** A server row and one revision of it — the shape a probe is asked about. */
async function seedRevision(serverId: string, revisionId: string): Promise<void> {
  await db.q(
    `INSERT INTO web.mcp_server (id, workspace_id, registry_name, display_name, auth_mode, status)
     VALUES ($1, $2, $3, $3, 'none', 'active')
     ON CONFLICT (id) DO NOTHING`,
    [serverId, wsId, `io.github.acme/${serverId}`],
  );
  await db.q(
    `INSERT INTO web.mcp_server_revision
       (id, server_id, seq, status, upstream_version, document, transport, url, source,
        published_at, published_by)
     VALUES ($1, $2, 1, 'published', '1.0.0', $3::jsonb, 'streamable-http',
             'https://acme.example/mcp', 'owner', now(), 'Owner')`,
    [
      revisionId,
      serverId,
      JSON.stringify({
        name: `io.github.acme/${serverId}`,
        version: "1.0.0",
        remotes: [{ type: "streamable-http", url: "https://acme.example/mcp" }],
      }),
    ],
  );
}

async function probeStateOf(revisionId: string) {
  const rows = await db.q<{
    probe_outcome: string | null;
    probed_at: Date | null;
    verification: { probeDetail?: string | null } | null;
  }>("SELECT probe_outcome, probed_at, verification FROM web.mcp_server_revision WHERE id = $1", [
    revisionId,
  ]);
  const row = rows[0];
  return {
    outcome: row?.probe_outcome ?? null,
    detail: row?.verification?.probeDetail ?? null,
    probedAt: row?.probed_at ?? null,
  };
}

describe("recording what was seen", () => {
  it("writes the answer onto the revision, and a re-probe replaces it", async () => {
    await seedRevision("mcps_one", "mcpr_one");
    const target = { revisionId: "mcpr_one", endpoint: "https://93.184.216.34/mcp" };

    await probeAndRecord(target, answer(200, OK_INITIALIZE));
    let state = await probeStateOf("mcpr_one");
    expect(state.outcome).toBe("responding");
    expect(state.detail).toBe(null);
    expect(state.probedAt).not.toBe(null);

    await probeAndRecord(target, answer(503, "down"));
    state = await probeStateOf("mcpr_one");
    expect(state.outcome).toBe("not_responding");
    expect(state.detail).toBe("answered 503");
    // The line a surface renders reads from exactly those two facts.
    expect(
      probeStateLine({
        outcome: "not_responding",
        probedAt: (state.probedAt as Date).toISOString(),
        detail: state.detail,
      }),
    ).toMatch(/^not responding when checked /);
  });

  it("asks nothing about a package-only document, and records nothing for it", async () => {
    await seedRevision("mcps_pkg", "mcpr_pkg");
    const transport = vi.fn<ProbeTransport>();
    const verdict = await probeAndRecord({ revisionId: "mcpr_pkg", endpoint: null }, transport);
    expect(verdict).toBe(null);
    expect(transport).not.toHaveBeenCalled();
    expect(await probeStateOf("mcpr_pkg")).toMatchObject({ outcome: null, probedAt: null });
  });

  it("a revision that is no longer there costs nothing — the act it followed is durable", async () => {
    await expect(
      probeAndRecord(
        { revisionId: "mcpr_gone", endpoint: "https://93.184.216.34/mcp" },
        answer(200, OK_INITIALIZE),
      ),
    ).resolves.toEqual({ outcome: "responding", detail: null });
    expect(await probeStateOf("mcpr_gone")).toMatchObject({ outcome: null });
  });

  it("swallows a transport that throws something unexpected", async () => {
    await seedRevision("mcps_boom", "mcpr_boom");
    const exploding: ProbeTransport = () => {
      throw new TypeError("not a function");
    };
    await expect(
      probeAndRecord({ revisionId: "mcpr_boom", endpoint: "https://93.184.216.34/mcp" }, exploding),
    ).resolves.toEqual({ outcome: "not_responding", detail: "no answer" });
  });

  it("fires without being waited on, and cannot reject", async () => {
    const { scheduleRevisionProbe } = await import("@/lib/mcp/probe.server");
    expect(() =>
      scheduleRevisionProbe({ revisionId: "mcpr_absent", endpoint: "https://127.0.0.1/mcp" }),
    ).not.toThrow();
  });
});
