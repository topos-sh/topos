import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The pass-through half of the door (`forwardDeviceLane`) — what may cross, in each direction,
 * and what dies at the boundary. The vault behind it is stubbed at global fetch: these tests pin
 * the CONTAINMENT rules (path pinned under /v1, no traversal into the internal lane, the header
 * allowlists both ways), not vault behavior.
 */

let forwardDeviceLane: typeof import("@/lib/plane/client.server").forwardDeviceLane;

beforeAll(async () => {
  installTestEnv();
  ({ forwardDeviceLane } = await import("@/lib/plane/client.server"));
});

afterEach(() => {
  vi.unstubAllGlobals();
});

interface Captured {
  url: string;
  method: string;
  headers: Headers;
}

/** Stub the vault: capture the outbound request, answer a recognizable upstream response. */
function stubVault(status = 200): { calls: Captured[] } {
  const calls: Captured[] = [];
  vi.stubGlobal("fetch", (url: string | URL, init?: RequestInit) => {
    calls.push({
      url: String(url),
      method: init?.method ?? "GET",
      headers: new Headers(init?.headers),
    });
    // A 204/304 Response must carry a null body (the fetch spec refuses otherwise).
    const body = status === 204 || status === 304 ? null : '{"up":"stream"}';
    return Promise.resolve(
      new Response(body, {
        status,
        headers: {
          "content-type": "application/json",
          etag: '"1.2"',
          "cache-control": "no-store",
          // Hop-by-hop / internal headers the forwarder must NOT relay back.
          "set-cookie": "sneaky=1",
          "x-internal-detail": "topology",
        },
      }),
    );
  });
  return { calls };
}

function req(path: string, init?: RequestInit): Request {
  return new Request(`http://door.example${path}`, init);
}

describe("path containment", () => {
  it("forwards /api/v1/* to the vault's /v1/* with the query preserved", async () => {
    const vault = stubVault();
    const res = await forwardDeviceLane(req("/api/v1/workspaces/w1/skills?x=1"));
    expect(res.status).toBe(200);
    expect(vault.calls[0]?.url).toBe("http://vault.internal:8080/v1/workspaces/w1/skills?x=1");
  });

  it("refuses any path outside /api/v1 with the uniform 404, without dialing the vault", async () => {
    const vault = stubVault();
    for (const path of ["/api/v2/x", "/api/internal/v1/x", "/api/v10/x", "/other"]) {
      const res = await forwardDeviceLane(req(path));
      expect(res.status, path).toBe(404);
    }
    expect(vault.calls).toHaveLength(0);
  });

  it("refuses encoded-dot and backslash traversal shapes before any URL is built", async () => {
    const vault = stubVault();
    for (const path of [
      "/api/v1/%2e%2e/internal/v1/workspaces",
      "/api/v1/%2E%2E/internal/v1/workspaces",
      "/api/v1/a%5cb",
      "/api/v1/.%2e/x",
    ]) {
      const res = await forwardDeviceLane(req(path));
      expect(res.status, path).toBe(404);
    }
    expect(vault.calls).toHaveLength(0);
  });

  it("normalized dot-dot cannot escape either — the URL parser resolves it out of /api/v1", async () => {
    const vault = stubVault();
    // `new URL` normalizes literal `..` BEFORE the prefix check, so this lands outside /api/v1.
    const res = await forwardDeviceLane(req("/api/v1/../internal/v1/x"));
    expect(res.status).toBe(404);
    expect(vault.calls).toHaveLength(0);
  });
});

describe("header containment", () => {
  it("relays only the protocol request headers — cookies and the internal-lane identity die here", async () => {
    const vault = stubVault();
    await forwardDeviceLane(
      req("/api/v1/publish-shaped-path", {
        method: "POST",
        headers: {
          authorization: "Bearer device-cred",
          accept: "application/json",
          "topos-known-version-id": "abc",
          cookie: "session=hijack",
          "x-topos-acting-email": "smuggled@evil.example",
          "x-forwarded-for": "1.2.3.4",
        },
        body: "{}",
      }),
    );
    const sent = vault.calls[0]?.headers as Headers;
    expect(sent.get("authorization")).toBe("Bearer device-cred");
    expect(sent.get("topos-known-version-id")).toBe("abc");
    expect(sent.get("cookie")).toBeNull();
    expect(sent.get("x-topos-acting-email")).toBeNull();
    expect(sent.get("x-forwarded-for")).toBeNull();
  });

  it("relays only the protocol response headers — upstream cookies and internals never reach the client", async () => {
    stubVault();
    const res = await forwardDeviceLane(req("/api/v1/workspaces/w1/skills"));
    expect(res.headers.get("etag")).toBe('"1.2"');
    expect(res.headers.get("cache-control")).toBe("no-store");
    expect(res.headers.get("set-cookie")).toBeNull();
    expect(res.headers.get("x-internal-detail")).toBeNull();
  });

  it("passes upstream status through untouched (the vault's outcome is the outcome)", async () => {
    stubVault(304);
    const res = await forwardDeviceLane(req("/api/v1/workspaces/w1/skills/s1/current"));
    expect(res.status).toBe(304);
  });
});

describe("vault unreachable", () => {
  it("answers the flat retryable 500 — never a connection detail", async () => {
    vi.stubGlobal("fetch", () => Promise.reject(new Error("ECONNREFUSED 10.0.0.7:8787")));
    const consoleError = vi.spyOn(console, "error").mockImplementation(() => {});
    const res = await forwardDeviceLane(req("/api/v1/workspaces/w1/delivery"));
    expect(res.status).toBe(500);
    const text = await res.text();
    expect(text).not.toContain("10.0.0.7");
    const body = JSON.parse(text) as { error: { code: string } };
    expect(body.error.code).toBe("INTERNAL");
    consoleError.mockRestore();
  });
});
