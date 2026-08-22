/**
 * The client-era × upstream-era matrix for the bridged tool surface, plus the era rules around
 * it: independent negotiation per side, upstream identity echo, cross-era refusals for non-tool
 * features, 2025-03-26 batch splitting, and 2026-07-28 header/version validation.
 */
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { resetEngineMemoryForTests } from "../core/state";
import {
  BEARER,
  FakeStore,
  FakeUsage,
  bodyJson,
  fake2411,
  legacyFake,
  makeCtx,
  modernBody,
  modernFake,
  modernHeaders,
  nextId,
  open2411Client,
  resolvedServer,
  rpc,
  sessionRef,
  UPSTREAM_URL,
  WS,
  type Fake2411,
  type LegacyFake,
  type ModernFake,
  type TestCtx,
} from "./core-helpers";

type ClientEra = "2024-11-05" | "2025-03-26" | "2025-06-18" | "2026-07-28";
type UpstreamEra = "2024-11-05" | "2025-03-26" | "2025-06-18" | "2025-11-25" | "2026-07-28";

interface UpstreamRig {
  routes: Record<string, (req: Request) => Promise<Response> | Response>;
  serverInfoName: string;
  calls: () => { method: string; headers: Record<string, string> }[];
  toolCallCount: () => number;
}

function buildUpstream(era: UpstreamEra): UpstreamRig {
  if (era === "2024-11-05") {
    const f: Fake2411 = fake2411("https://up.test", { secret: "up-secret-1" });
    return {
      routes: { "https://up.test": f.handler },
      serverInfoName: "upstream-2411",
      calls: () => f.calls,
      toolCallCount: () => f.toolCallCount,
    };
  }
  if (era === "2026-07-28") {
    const f: ModernFake = modernFake({ secret: "up-secret-1" });
    return {
      routes: { [UPSTREAM_URL]: f.handler },
      serverInfoName: "upstream-modern",
      calls: () => f.calls,
      toolCallCount: () => f.toolCallCount,
    };
  }
  const f: LegacyFake = legacyFake(era, { secret: "up-secret-1" });
  return {
    routes: { [UPSTREAM_URL]: f.handler },
    serverInfoName: `upstream-${era}`,
    calls: () => f.calls,
    toolCallCount: () => f.toolCallCount,
  };
}

interface ClientDriver {
  setup(): Promise<void>;
  serverInfoName(): string;
  toolsList(): Promise<Record<string, unknown>>;
  toolsCall(name: string): Promise<Record<string, unknown>>;
}

function buildClient(era: ClientEra, t: TestCtx): ClientDriver {
  if (era === "2026-07-28") {
    return {
      async setup() {},
      serverInfoName: () => "", // Modern identity is read via server/discover, asserted separately.
      async toolsList() {
        const resp = await rpc(t, modernBody("tools/list"), modernHeaders("tools/list"));
        const body = await bodyJson(resp);
        if (body["result"] === undefined) throw new Error(`tools/list failed: ${JSON.stringify(body)}`);
        return body["result"] as Record<string, unknown>;
      },
      async toolsCall(name) {
        const resp = await rpc(t, modernBody("tools/call", { name, arguments: { a: 1 } }), modernHeaders("tools/call", name));
        const body = await bodyJson(resp);
        if (body["result"] === undefined) throw new Error(`tools/call failed: ${JSON.stringify(body)}`);
        return body["result"] as Record<string, unknown>;
      },
    };
  }
  if (era === "2024-11-05") {
    let client: Awaited<ReturnType<typeof open2411Client>> | null = null;
    let info = "";
    return {
      async setup() {
        client = await open2411Client(t);
        const id = nextId();
        await client.post({ jsonrpc: "2.0", id, method: "initialize", params: { protocolVersion: "2024-11-05", capabilities: {} } });
        const reply = await client.nextMessage();
        const result = reply["result"] as Record<string, unknown>;
        info = ((result["serverInfo"] as Record<string, unknown>)["name"] as string) ?? "";
        await client.post({ jsonrpc: "2.0", method: "notifications/initialized" });
      },
      serverInfoName: () => info,
      async toolsList() {
        if (!client) throw new Error("setup first");
        await client.post({ jsonrpc: "2.0", id: nextId(), method: "tools/list" });
        const reply = await client.nextMessage();
        if (reply["result"] === undefined) throw new Error(`tools/list failed: ${JSON.stringify(reply)}`);
        return reply["result"] as Record<string, unknown>;
      },
      async toolsCall(name) {
        if (!client) throw new Error("setup first");
        await client.post({ jsonrpc: "2.0", id: nextId(), method: "tools/call", params: { name, arguments: { a: 1 } } });
        const reply = await client.nextMessage();
        if (reply["result"] === undefined) throw new Error(`tools/call failed: ${JSON.stringify(reply)}`);
        return reply["result"] as Record<string, unknown>;
      },
    };
  }
  // Legacy Streamable HTTP client (2025-03-26 or 2025-06-18).
  let sid = "";
  let info = "";
  const headers = () => ({
    "mcp-session-id": sid,
    ...(era === "2025-03-26" ? {} : { "mcp-protocol-version": era }),
  });
  return {
    async setup() {
      const resp = await rpc(t, {
        jsonrpc: "2.0",
        id: nextId(),
        method: "initialize",
        params: { protocolVersion: era, capabilities: { sampling: {} }, clientInfo: { name: "test-client", version: "1" } },
      });
      const body = await bodyJson(resp);
      const result = body["result"] as Record<string, unknown>;
      expect(result["protocolVersion"]).toBe(era); // Client-side version is negotiated with US.
      info = ((result["serverInfo"] as Record<string, unknown>)["name"] as string) ?? "";
      sid = resp.headers.get("mcp-session-id") ?? "";
      expect(sid).not.toBe("");
      await rpc(t, { jsonrpc: "2.0", method: "notifications/initialized" }, headers());
    },
    serverInfoName: () => info,
    async toolsList() {
      const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, headers());
      const body = await bodyJson(resp);
      if (body["result"] === undefined) throw new Error(`tools/list failed: ${JSON.stringify(body)}`);
      return body["result"] as Record<string, unknown>;
    },
    async toolsCall(name) {
      const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/call", params: { name, arguments: { a: 1 } } }, headers());
      const body = await bodyJson(resp);
      if (body["result"] === undefined) throw new Error(`tools/call failed: ${JSON.stringify(body)}`);
      return body["result"] as Record<string, unknown>;
    },
  };
}

async function seed(routes: UpstreamRig["routes"]): Promise<TestCtx> {
  const store = new FakeStore();
  const usage = new FakeUsage();
  const t = makeCtx(store, usage, routes);
  await store.addBearer(BEARER, sessionRef());
  store.setServer(WS, resolvedServer());
  store.setCredential(WS, "srv1", null, {
    id: "cred_1",
    kind: "oauth",
    secret: "up-secret-1",
    refreshToken: "refresh-1",
    tokenEndpoint: "https://auth.test/token",
    clientId: "client-1",
  });
  return t;
}

const CLIENT_ERAS: ClientEra[] = ["2024-11-05", "2025-03-26", "2025-06-18", "2026-07-28"];
const UPSTREAM_ERAS: UpstreamEra[] = ["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"];

describe("tool surface bridges across every revision pair", () => {
  afterEach(() => resetEngineMemoryForTests());

  for (const upstreamEra of UPSTREAM_ERAS) {
    for (const clientEra of CLIENT_ERAS) {
      it(`client ${clientEra} × upstream ${upstreamEra}: tools/list + tools/call round-trip`, async () => {
        const rig = buildUpstream(upstreamEra);
        const t = await seed(rig.routes);
        const client = buildClient(clientEra, t);
        await client.setup();

        const list = await client.toolsList();
        const names = (list["tools"] as { name: string }[]).map((x) => x.name);
        expect(names).toEqual(["issue_create", "issue_delete", "issue_list"]);

        const result = await client.toolsCall("issue_create");
        const content = result["content"] as { type: string; text: string }[];
        const echoed = JSON.parse(content[0]?.text ?? "{}") as Record<string, unknown>;
        expect(echoed["name"]).toBe("issue_create"); // Tool names verbatim, arguments intact.
        expect(echoed["arguments"]).toEqual({ a: 1 });
        expect(rig.toolCallCount()).toBe(1);

        if (clientEra !== "2026-07-28" && clientEra !== "2024-11-05") {
          expect(client.serverInfoName()).toBe(rig.serverInfoName); // Impersonation: upstream identity echoed.
        }
        if (clientEra === "2024-11-05") {
          expect(client.serverInfoName()).toBe(rig.serverInfoName);
        }
        if (clientEra === "2026-07-28") {
          // Modern clients see resultType everywhere and a private cache scope on lists.
          expect(list["resultType"]).toBe("complete");
          expect(list["cacheScope"]).toBe("private");
          expect(typeof list["ttlMs"]).toBe("number");
          expect(result["resultType"]).toBe("complete");
        }

        const upstreamCalls = rig.calls();
        if (upstreamEra === "2026-07-28") {
          const call = upstreamCalls.find((c) => c.method === "tools/call");
          expect(call?.headers["mcp-method"]).toBe("tools/call");
          expect(call?.headers["mcp-name"]).toBe("issue_create");
          expect(call?.headers["mcp-protocol-version"]).toBe("2026-07-28");
          expect(call?.headers["mcp-session-id"]).toBeUndefined(); // Never sent to a stateless upstream.
          const body = call ? ((call as { body?: unknown }).body as Record<string, unknown>) : {};
          const params = body["params"] as Record<string, unknown>;
          const meta = params["_meta"] as Record<string, unknown>;
          expect(meta["io.modelcontextprotocol/protocolVersion"]).toBe("2026-07-28");
          expect(meta["io.modelcontextprotocol/clientCapabilities"]).toBeDefined();
        } else if (upstreamEra !== "2024-11-05") {
          const call = upstreamCalls.find((c) => c.method === "tools/list");
          expect(call?.headers["mcp-session-id"]).toBeDefined(); // Upstream session echoed per request.
          if (upstreamEra !== "2025-03-26") {
            expect(call?.headers["mcp-protocol-version"]).toBe(upstreamEra);
          }
        }
      });
    }
  }
});

describe("era rules around the bridge", () => {
  afterEach(() => resetEngineMemoryForTests());

  it("negotiates each side independently: a 2025-03-26 client rides a 2025-06-18 upstream", async () => {
    const rig = buildUpstream("2025-06-18");
    const t = await seed(rig.routes);
    const client = buildClient("2025-03-26", t);
    await client.setup(); // Asserts the client-side echo is 2025-03-26 inside setup.
    const init = rig.calls().find((c) => c.method === "initialize");
    expect(init).toBeDefined(); // While the upstream side ran its own 2025-06-18 handshake.
  });

  it("caches the upstream era verdict: one probe, not one per request", async () => {
    const f = modernFake({ secret: "up-secret-1" });
    const t = await seed({ [UPSTREAM_URL]: f.handler });
    const client = buildClient("2025-06-18", t);
    await client.setup();
    await client.toolsList();
    await client.toolsList();
    expect(f.discoverCount).toBe(1);
  });

  it("refuses a non-tool method across eras with the plain sentence (legacy client, modern upstream)", async () => {
    const rig = buildUpstream("2026-07-28");
    const t = await seed(rig.routes);
    const initResp = await rpc(t, {
      jsonrpc: "2.0",
      id: nextId(),
      method: "initialize",
      params: { protocolVersion: "2025-06-18", capabilities: {} },
    });
    const sid = initResp.headers.get("mcp-session-id") ?? "";
    const direct = await rpc(
      t,
      { jsonrpc: "2.0", id: nextId(), method: "resources/list" },
      { "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" },
    );
    const body = await bodyJson(direct);
    const error = body["error"] as Record<string, unknown>;
    expect(error["message"]).toBe("The method resources/list is not supported across protocol revisions.");
    expect(direct.status).toBe(200); // Legacy clients read errors at the JSON-RPC layer, not HTTP.
  });

  it("refuses a non-tool method across eras for a modern client with 404 + the sentence", async () => {
    const rig = buildUpstream("2025-06-18");
    const t = await seed(rig.routes);
    const resp = await rpc(t, modernBody("resources/read", { uri: "res://a" }), modernHeaders("resources/read", "res://a"));
    expect(resp.status).toBe(404);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["message"]).toBe(
      "The method resources/read is not supported across protocol revisions.",
    );
  });

  it("passes a non-tool method through untranslated when both sides share an era (legacy)", async () => {
    const rig = buildUpstream("2025-06-18");
    const t = await seed(rig.routes);
    const initResp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {} } });
    const sid = initResp.headers.get("mcp-session-id") ?? "";
    const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "resources/list" }, { "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" });
    const body = await bodyJson(resp);
    const result = body["result"] as Record<string, unknown>;
    expect((result["resources"] as unknown[]).length).toBe(1);
  });

  it("passes a non-tool method through untranslated when both sides are modern", async () => {
    const rig = buildUpstream("2026-07-28");
    const t = await seed(rig.routes);
    const resp = await rpc(t, modernBody("resources/read", { uri: "res://a" }), modernHeaders("resources/read", "res://a"));
    const body = await bodyJson(resp);
    const result = body["result"] as Record<string, unknown>;
    expect((result["contents"] as { text: string }[])[0]?.text).toBe("resource body");
  });

  it("splits a 2025-03-26 batch: array in, array out, members reach the upstream individually", async () => {
    const f = legacyFake("2025-06-18", { secret: "up-secret-1" }); // Would 400 on any forwarded array.
    const t = await seed({ [UPSTREAM_URL]: f.handler });
    const initResp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-03-26", capabilities: {} } });
    const sid = initResp.headers.get("mcp-session-id") ?? "";
    t.usage.events.length = 0;
    const resp = await rpc(
      t,
      [
        { jsonrpc: "2.0", id: "b1", method: "tools/list" },
        { jsonrpc: "2.0", id: "b2", method: "tools/call", params: { name: "issue_list", arguments: {} } },
      ],
      { "mcp-session-id": sid },
    );
    expect(resp.status).toBe(200);
    const body = (await resp.json()) as Record<string, unknown>[];
    expect(body).toHaveLength(2);
    expect(body[0]?.["id"]).toBe("b1");
    expect(body[1]?.["id"]).toBe("b2");
    expect(t.usage.events).toHaveLength(2); // One usage row per batch member.
    expect(f.toolCallCount).toBe(1);
  });

  it("refuses a batch from a non-batching revision", async () => {
    const rig = buildUpstream("2025-06-18");
    const t = await seed(rig.routes);
    const initResp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {} } });
    const sid = initResp.headers.get("mcp-session-id") ?? "";
    const resp = await rpc(t, [{ jsonrpc: "2.0", id: "b1", method: "tools/list" }], { "mcp-session-id": sid });
    expect(resp.status).toBe(400);
  });

  it("passes a modern input_required result through to a modern client, requestState untouched", async () => {
    const f = modernFake({ secret: "up-secret-1", inputRequired: true });
    const t = await seed({ [UPSTREAM_URL]: f.handler });
    const resp = await rpc(t, modernBody("tools/call", { name: "issue_create", arguments: {} }), modernHeaders("tools/call", "issue_create"));
    const body = await bodyJson(resp);
    const result = body["result"] as Record<string, unknown>;
    expect(result["resultType"]).toBe("input_required");
    expect(result["requestState"]).toBe("opaque-xyz");
  });

  it("answers a legacy client with the plain sentence when a modern upstream needs input", async () => {
    const f = modernFake({ secret: "up-secret-1", inputRequired: true });
    const t = await seed({ [UPSTREAM_URL]: f.handler });
    const initResp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {} } });
    const sid = initResp.headers.get("mcp-session-id") ?? "";
    const resp = await rpc(
      t,
      { jsonrpc: "2.0", id: nextId(), method: "tools/call", params: { name: "issue_create", arguments: {} } },
      { "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" },
    );
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["message"]).toBe(
      "The server asked for additional input, which is not supported across protocol revisions.",
    );
  });

  it("serves server/discover to modern clients: gateway versions, upstream identity, private scope", async () => {
    const rig = buildUpstream("2025-06-18");
    const t = await seed(rig.routes);
    const resp = await rpc(t, modernBody("server/discover"), modernHeaders("server/discover"));
    const body = await bodyJson(resp);
    const result = body["result"] as Record<string, unknown>;
    expect(result["supportedVersions"]).toEqual(["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25", "2026-07-28"]);
    expect(result["resultType"]).toBe("complete");
    expect(result["cacheScope"]).toBe("private");
    const meta = result["_meta"] as Record<string, unknown>;
    expect(((meta["io.modelcontextprotocol/serverInfo"] as Record<string, unknown>)["name"] as string)).toBe("upstream-2025-06-18");
    // Cross-era: only the tool surface is advertised.
    expect(Object.keys(result["capabilities"] as Record<string, unknown>)).toEqual(["tools"]);
  });

  it("rejects a modern request whose header and body versions disagree (−32020)", async () => {
    const rig = buildUpstream("2026-07-28");
    const t = await seed(rig.routes);
    const resp = await rpc(t, modernBody("tools/list"), { ...modernHeaders("tools/list"), "mcp-protocol-version": "2025-11-25" });
    expect(resp.status).toBe(400);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["code"]).toBe(-32020);
  });

  it("rejects an unsupported modern version with −32022 and the supported list", async () => {
    const rig = buildUpstream("2026-07-28");
    const t = await seed(rig.routes);
    const msg = modernBody("tools/list");
    ((msg["params"] as Record<string, unknown>)["_meta"] as Record<string, unknown>)["io.modelcontextprotocol/protocolVersion"] = "2027-01-01";
    const resp = await rpc(t, msg, { ...modernHeaders("tools/list"), "mcp-protocol-version": "2027-01-01" });
    expect(resp.status).toBe(400);
    const body = await bodyJson(resp);
    const error = body["error"] as Record<string, unknown>;
    expect(error["code"]).toBe(-32022);
    expect((error["data"] as Record<string, unknown>)["supported"]).toContain("2026-07-28");
  });

  it("rejects a modern request missing required _meta fields (−32602, HTTP 400)", async () => {
    const rig = buildUpstream("2026-07-28");
    const t = await seed(rig.routes);
    const resp = await rpc(
      t,
      {
        jsonrpc: "2.0",
        id: nextId(),
        method: "tools/list",
        params: { _meta: { "io.modelcontextprotocol/protocolVersion": "2026-07-28" } },
      },
      modernHeaders("tools/list"),
    );
    expect(resp.status).toBe(400);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["code"]).toBe(-32602);
  });

  it("rejects an Mcp-Method header that does not match the body (−32020)", async () => {
    const rig = buildUpstream("2026-07-28");
    const t = await seed(rig.routes);
    const resp = await rpc(t, modernBody("tools/list"), { ...modernHeaders("tools/call"), "mcp-name": "x" });
    expect(resp.status).toBe(400);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["code"]).toBe(-32020);
  });

  it("rejects a tools/call whose Mcp-Name header does not match params.name", async () => {
    const rig = buildUpstream("2026-07-28");
    const t = await seed(rig.routes);
    const resp = await rpc(t, modernBody("tools/call", { name: "issue_create", arguments: {} }), modernHeaders("tools/call", "issue_delete"));
    expect(resp.status).toBe(400);
    const body = await bodyJson(resp);
    expect((body["error"] as Record<string, unknown>)["code"]).toBe(-32020);
  });

  it("answers ping locally for legacy clients and never forwards it to a modern upstream", async () => {
    const f = modernFake({ secret: "up-secret-1" });
    const t = await seed({ [UPSTREAM_URL]: f.handler });
    const initResp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {} } });
    const sid = initResp.headers.get("mcp-session-id") ?? "";
    const resp = await rpc(t, { jsonrpc: "2.0", id: "p1", method: "ping" }, { "mcp-session-id": sid, "mcp-protocol-version": "2025-06-18" });
    const body = await bodyJson(resp);
    expect(body["result"]).toEqual({});
    expect(f.calls.filter((c) => c.method === "ping")).toHaveLength(0);
  });

  it("echoes the requested legacy version and falls back to 2025-11-25 for an unknown ask", async () => {
    const rig = buildUpstream("2025-06-18");
    const t = await seed(rig.routes);
    const known = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-11-25", capabilities: {} } });
    expect(((await bodyJson(known))["result"] as Record<string, unknown>)["protocolVersion"]).toBe("2025-11-25");
    resetEngineMemoryForTests();
    const unknown = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "1999-01-01", capabilities: {} } });
    expect(((await bodyJson(unknown))["result"] as Record<string, unknown>)["protocolVersion"]).toBe("2025-11-25");
  });

  it("keeps a dead client session honest: 404 tells the legacy client to re-initialize", async () => {
    const rig = buildUpstream("2025-06-18");
    const t = await seed(rig.routes);
    const resp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": "never-minted" });
    expect(resp.status).toBe(404);
  });

  it("terminates a client session on DELETE and refuses the id afterwards", async () => {
    const rig = buildUpstream("2025-06-18");
    const t = await seed(rig.routes);
    const initResp = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "initialize", params: { protocolVersion: "2025-06-18", capabilities: {} } });
    const sid = initResp.headers.get("mcp-session-id") ?? "";
    const del = await rpc(t, undefined as never, { "mcp-session-id": sid }, { method: "DELETE" });
    expect(del.status).toBe(200);
    const after = await rpc(t, { jsonrpc: "2.0", id: nextId(), method: "tools/list" }, { "mcp-session-id": sid });
    expect(after.status).toBe(404);
  });
});
