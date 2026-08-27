import { createHash, randomBytes } from "node:crypto";
import http from "node:http";
import type { AddressInfo } from "node:net";
import pg from "pg";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { GatewayContext, GatewayStore } from "../core/ports";
import { EnvelopeCrypto, FileMasterKey } from "../service/crypto";
import { createGuardedFetch } from "../service/guarded-fetch";
import { beginAuthorize, type OauthStore } from "../service/oauth";
import { createPublicHandler } from "../service/server";
import { PgStore } from "../service/store";
import {
  createServiceDb,
  seedConnectedServer,
  seedIdentity,
  type ServiceDb,
} from "./helpers/service-db";

/**
 * The oauth ceremony end to end, against a FAKE upstream that is both the MCP resource (RFC 9728
 * protected-resource metadata) and its authorization server (RFC 8414 metadata, DCR, PKCE-checked
 * token endpoint). The upstream runs on loopback http, so the walk rides a permissive guarded
 * fetch — the strict policy has its own suite. The DB rows are real; only the connected server's
 * ADDRESS is swapped to the fixture (the web schema rightly refuses to store an http url).
 */

const CONFIG = {
  gatewayPublicUrl: "https://gateway.example.com",
  toposPublicUrl: "https://team.example.com",
};
const REDIRECT_URI = "https://gateway.example.com/oauth/callback";
const quiet = () => {};
const walkFetch = createGuardedFetch({ allowPrivate: true, allowInsecure: true }, quiet);

let db: ServiceDb;
let pool: pg.Pool;
let realStore: PgStore;
let base = "";
let fixture: http.Server;

// What the fixture saw — assertions read these.
const registrations: Array<Record<string, unknown>> = [];
const tokenRequests: Array<Record<string, string>> = [];
let issuedChallenge: string | null = null;

function s256(verifier: string): string {
  return createHash("sha256")
    .update(verifier)
    .digest("base64")
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
}

function startFixture(): Promise<string> {
  fixture = http.createServer((request, response) => {
    const chunks: Buffer[] = [];
    request.on("data", (chunk: Buffer) => chunks.push(chunk));
    request.on("end", () => {
      const body = Buffer.concat(chunks).toString("utf8");
      const url = new URL(request.url ?? "/", base);
      const json = (status: number, payload: unknown) => {
        response.writeHead(status, { "content-type": "application/json" });
        response.end(JSON.stringify(payload));
      };
      switch (url.pathname) {
        // RFC 9728, path-scoped for the /mcp resource.
        case "/.well-known/oauth-protected-resource/mcp":
          return json(200, {
            resource: `${base}/mcp`,
            authorization_servers: [`${base}/as`],
            scopes_supported: ["mcp.read", "mcp.write"],
          });
        // A resource whose AS refuses S256 — the walk must refuse it.
        case "/.well-known/oauth-protected-resource/plainmcp":
          return json(200, { authorization_servers: [`${base}/plainas`] });
        // RFC 8414, path-inserted for the /as issuer.
        case "/.well-known/oauth-authorization-server/as":
          return json(200, {
            issuer: `${base}/as`,
            authorization_endpoint: `${base}/as/authorize`,
            token_endpoint: `${base}/as/token`,
            registration_endpoint: `${base}/as/register`,
            code_challenge_methods_supported: ["S256"],
          });
        case "/.well-known/oauth-authorization-server/plainas":
          return json(200, {
            issuer: `${base}/plainas`,
            authorization_endpoint: `${base}/plainas/authorize`,
            token_endpoint: `${base}/plainas/token`,
            registration_endpoint: `${base}/plainas/register`,
            code_challenge_methods_supported: ["plain"],
          });
        // The MCP endpoint itself — reached by the tool probe the callback runs once a credential
        // has landed. Bearer-gated, so the probe proves it attached the token it just stored.
        case "/mcp": {
          if (request.method !== "POST") {
            return json(405, { error: "method_not_allowed" });
          }
          if (request.headers.authorization !== "Bearer at-live") {
            return json(401, { error: "unauthorized" });
          }
          const rpc = JSON.parse(body) as Record<string, unknown>;
          if (rpc.method === "initialize") {
            return json(200, {
              jsonrpc: "2.0",
              id: rpc.id,
              result: {
                protocolVersion: "2025-06-18",
                capabilities: { tools: {} },
                serverInfo: { name: "oauth-fixture" },
              },
            });
          }
          if (rpc.method === "tools/list") {
            return json(200, {
              jsonrpc: "2.0",
              id: rpc.id,
              result: { tools: [{ name: "list_issues" }, { name: "create_issue" }] },
            });
          }
          response.writeHead(202);
          return response.end();
        }
        case "/as/register": {
          const parsed = JSON.parse(body) as Record<string, unknown>;
          registrations.push(parsed);
          return json(201, { client_id: "client-123" });
        }
        case "/as/token": {
          const form = Object.fromEntries(new URLSearchParams(body));
          tokenRequests.push(form);
          const verifierOk =
            typeof form.code_verifier === "string" &&
            issuedChallenge !== null &&
            s256(form.code_verifier) === issuedChallenge;
          if (
            form.grant_type !== "authorization_code" ||
            form.code !== "the-code" ||
            form.redirect_uri !== REDIRECT_URI ||
            form.client_id !== "client-123" ||
            !verifierOk
          ) {
            return json(400, { error: "invalid_grant" });
          }
          return json(200, {
            access_token: "at-live",
            token_type: "Bearer",
            refresh_token: "rt-live",
            expires_in: 3600,
          });
        }
        default:
          return json(404, { error: "not_found" });
      }
    });
  });
  return new Promise((resolve) => {
    fixture.listen(0, "127.0.0.1", () => {
      resolve(`http://127.0.0.1:${(fixture.address() as AddressInfo).port}`);
    });
  });
}

/** The real store with the connected server's ADDRESS swapped onto the fixture. */
function fixtureStore(urlByServerId: Map<string, string>): GatewayStore & OauthStore {
  return {
    connectedServer: async (ws, serverId) => {
      const real = await realStore.connectedServer(ws, serverId);
      const url = urlByServerId.get(serverId);
      return real === null || url === undefined ? real : { ...real, url };
    },
    mintOauthFlow: (row) => realStore.mintOauthFlow(row),
    takeOauthFlow: (state) => realStore.takeOauthFlow(state),
    storeCredential: (input) => realStore.storeCredential(input),
    sessionByTokenSha256: (hex) => realStore.sessionByTokenSha256(hex),
    machineSessionByTokenSha256: (hex, ss) => realStore.machineSessionByTokenSha256(hex, ss),
    toolPolicy: (ws, serverId) => realStore.toolPolicy(ws, serverId),
    credentialFor: (ws, serverId, userId) => realStore.credentialFor(ws, serverId, userId),
    saveRotatedCredential: (id, next) => realStore.saveRotatedCredential(id, next),
    withRefreshLock: (id, fn) => realStore.withRefreshLock(id, fn),
    recordObservedTools: (ws, serverId, tools) => realStore.recordObservedTools(ws, serverId, tools),
  };
}

beforeAll(async () => {
  db = await createServiceDb();
  pool = new pg.Pool({ connectionString: db.gatewayUrl, max: 5 });
  realStore = new PgStore(pool, new EnvelopeCrypto(new FileMasterKey(randomBytes(32))), CONFIG.toposPublicUrl, quiet);
  await seedIdentity(db, "ws1", "u1");
  base = await startFixture();
}, 120_000);

afterAll(async () => {
  if (fixture !== undefined) {
    await new Promise((resolve) => fixture.close(resolve));
  }
  await pool?.end();
  await db?.drop();
});

describe("the authorize walk + callback ceremony", () => {
  it("walks PRM → AS metadata → DCR, mints the flow, and redeems the callback", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "oauth1", { authMode: "oauth" });
    const store = fixtureStore(new Map([[seeded.serverId, `${base}/mcp`]]));
    const returnTo = `${CONFIG.toposPublicUrl}/mcp/${seeded.bundleName}`;

    const begun = await beginAuthorize(
      { workspaceId: "ws1", serverId: seeded.serverId, userId: "u1", returnTo },
      store,
      CONFIG,
      { fetch: walkFetch, log: quiet },
    );
    expect(begun.ok).toBe(true);
    if (!begun.ok) {
      return;
    }
    const authorize = new URL(begun.authorizeUrl);
    expect(authorize.origin).toBe(base);
    expect(authorize.pathname).toBe("/as/authorize");
    expect(authorize.searchParams.get("response_type")).toBe("code");
    expect(authorize.searchParams.get("client_id")).toBe("client-123");
    expect(authorize.searchParams.get("redirect_uri")).toBe(REDIRECT_URI);
    expect(authorize.searchParams.get("code_challenge_method")).toBe("S256");
    expect(authorize.searchParams.get("resource")).toBe(`${base}/mcp`);
    expect(authorize.searchParams.get("scope")).toBe("mcp.read mcp.write");
    const state = authorize.searchParams.get("state");
    const challenge = authorize.searchParams.get("code_challenge");
    expect(state).toBeTruthy();
    expect(challenge).toBeTruthy();
    issuedChallenge = challenge;

    // DCR registered exactly the callback address, as a public client.
    expect(registrations.at(-1)?.redirect_uris).toEqual([REDIRECT_URI]);
    expect(registrations.at(-1)?.token_endpoint_auth_method).toBe("none");

    // The ceremony row is standing, secrets server-side only.
    const flows = await db.q("SELECT state, token_endpoint, return_to FROM gateway.oauth_flow WHERE state = $1", [state]);
    expect(flows.length).toBe(1);
    expect(flows[0]?.token_endpoint).toBe(`${base}/as/token`);
    expect(flows[0]?.return_to).toBe(returnTo);

    // The callback, over the public HTTP surface.
    const engineCalls: string[] = [];
    const context: GatewayContext = {
      store,
      usage: { record: () => {} },
      env: { fetch: walkFetch, now: () => Date.now(), log: quiet },
    };
    const handler = createPublicHandler({
      engine: async (_ctx, req) => {
        engineCalls.push(`${req.sessionId}/${req.serverId}:${req.bearer ?? "-"}`);
        return new Response("engine", { status: 418 });
      },
      context,
      store,
      config: CONFIG,
      guardedFetch: walkFetch,
      log: quiet,
    });

    const redeemed = await handler(
      new Request(
        `https://gateway.example.com/oauth/callback?code=the-code&state=${encodeURIComponent(state ?? "")}`,
      ),
    );
    expect(redeemed.status).toBe(302);
    expect(redeemed.headers.get("location")).toBe(returnTo);

    // The token request carried PKCE + the RFC 8707 resource.
    expect(tokenRequests.at(-1)?.resource).toBe(`${base}/mcp`);

    // AND the server's tools were read down before the person was sent back, with the token that
    // had just been stored — so the Tools panel has a checklist the moment the page renders.
    const observed = await db.q<{ name: string }>(
      "SELECT name FROM gateway.observed_tool WHERE server_id = $1 ORDER BY name",
      [seeded.serverId],
    );
    expect(observed.map((row) => row.name)).toEqual(["create_issue", "list_issues"]);

    // The credential landed, personal, decryptable, refresh material intact.
    const credential = await realStore.credentialFor("ws1", seeded.serverId, "u1");
    expect(credential?.kind).toBe("oauth");
    expect(credential?.secret).toBe("at-live");
    expect(credential?.refreshToken).toBe("rt-live");
    expect(credential?.tokenEndpoint).toBe(`${base}/as/token`);
    expect(credential?.clientId).toBe("client-123");

    // Single-use: the flow row is gone, and a replayed state answers the constant 400.
    expect(await db.q("SELECT 1 AS x FROM gateway.oauth_flow WHERE state = $1", [state])).toEqual([]);
    const replay = await handler(
      new Request(
        `https://gateway.example.com/oauth/callback?code=the-code&state=${encodeURIComponent(state ?? "")}`,
      ),
    );
    expect(replay.status).toBe(400);

    // The MCP route itself reached the engine untouched by any of this.
    const proxied = await handler(
      new Request("https://gateway.example.com/sn_x/msrv_y", {
        headers: { authorization: "Bearer tok" },
      }),
    );
    expect(proxied.status).toBe(418);
    expect(engineCalls).toEqual(["sn_x/msrv_y:tok"]);
    // And the public surface answers healthz + the uniform 404.
    expect((await handler(new Request("https://gateway.example.com/healthz"))).status).toBe(200);
    expect((await handler(new Request("https://gateway.example.com/a/b/c"))).status).toBe(404);
  });

  it("an expired flow answers the constant 400", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "oauthexp", { authMode: "oauth" });
    const store = fixtureStore(new Map());
    const state = `expired-${randomBytes(8).toString("hex")}`;
    await realStore.mintOauthFlow({
      state,
      workspaceId: "ws1",
      serverId: seeded.serverId,
      userId: "u1",
      codeVerifier: "v",
      clientId: "client-123",
      tokenEndpoint: `${base}/as/token`,
      resource: `${base}/mcp`,
      returnTo: `${CONFIG.toposPublicUrl}/mcp/x`,
      expiresAt: new Date(Date.now() - 1000),
    });
    const context: GatewayContext = {
      store,
      usage: { record: () => {} },
      env: { fetch: walkFetch, now: () => Date.now(), log: quiet },
    };
    const handler = createPublicHandler({
      engine: async () => new Response(null, { status: 500 }),
      context,
      store,
      config: CONFIG,
      guardedFetch: walkFetch,
      log: quiet,
    });
    const response = await handler(
      new Request(`https://gateway.example.com/oauth/callback?code=x&state=${state}`),
    );
    expect(response.status).toBe(400);
  });

  it("an upstream denial still lands the person back, with no credential stored", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "oauthdeny", { authMode: "oauth" });
    const store = fixtureStore(new Map([[seeded.serverId, `${base}/mcp`]]));
    const returnTo = `${CONFIG.toposPublicUrl}/mcp/${seeded.bundleName}`;
    const begun = await beginAuthorize(
      { workspaceId: "ws1", serverId: seeded.serverId, userId: "u1", returnTo },
      store,
      CONFIG,
      { fetch: walkFetch, log: quiet },
    );
    expect(begun.ok).toBe(true);
    if (!begun.ok) {
      return;
    }
    const state = new URL(begun.authorizeUrl).searchParams.get("state") ?? "";
    const context: GatewayContext = {
      store,
      usage: { record: () => {} },
      env: { fetch: walkFetch, now: () => Date.now(), log: quiet },
    };
    const handler = createPublicHandler({
      engine: async () => new Response(null, { status: 500 }),
      context,
      store,
      config: CONFIG,
      guardedFetch: walkFetch,
      log: quiet,
    });
    const response = await handler(
      new Request(
        `https://gateway.example.com/oauth/callback?error=access_denied&state=${encodeURIComponent(state)}`,
      ),
    );
    expect(response.status).toBe(302);
    expect(response.headers.get("location")).toBe(returnTo);
    expect(await realStore.credentialFor("ws1", seeded.serverId, "u1")).toBeNull();
    expect(await db.q("SELECT 1 AS x FROM gateway.oauth_flow WHERE state = $1", [state])).toEqual([]);
  });

  it("refuses an authorization server that will not take S256", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "oauthplain", { authMode: "oauth" });
    const store = fixtureStore(new Map([[seeded.serverId, `${base}/plainmcp`]]));
    const begun = await beginAuthorize(
      {
        workspaceId: "ws1",
        serverId: seeded.serverId,
        userId: "u1",
        returnTo: `${CONFIG.toposPublicUrl}/mcp/x`,
      },
      store,
      CONFIG,
      { fetch: walkFetch, log: quiet },
    );
    expect(begun).toEqual({ ok: false, code: "DISCOVERY_FAILED" });
  });

  it("refuses a returnTo off the web's origin before any walk", async () => {
    const seeded = await seedConnectedServer(db, "ws1", "oauthret", { authMode: "oauth" });
    const store = fixtureStore(new Map([[seeded.serverId, `${base}/mcp`]]));
    const begun = await beginAuthorize(
      {
        workspaceId: "ws1",
        serverId: seeded.serverId,
        userId: "u1",
        returnTo: "https://evil.example.net/phish",
      },
      store,
      CONFIG,
      { fetch: walkFetch, log: quiet },
    );
    expect(begun).toEqual({ ok: false, code: "BAD_RETURN_TO" });
  });
});
