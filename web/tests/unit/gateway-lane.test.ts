import { beforeEach, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * THE GATEWAY LANE'S TRANSPORT — the same two invariants the vault's transport carries, proven the
 * same way.
 *
 * 1. THE ALLOWLIST IS RUNTIME, not types: an off-list (method, template) pair throws BEFORE any
 *    request is made, so a wrapper someone adds without listing its route fails closed rather than
 *    reaching a service with a shape nobody reviewed.
 * 2. THE LANE IS OPTIONAL: with no gateway configured there is nothing to call, and the transport
 *    says so instead of dialing `undefined`.
 *
 * `fetch` is stubbed throughout — a unit suite that reached the network would be testing the
 * network. The assertions are about what the transport does before and instead of a request.
 */

async function client() {
  return await import("@/lib/gateway/client.server");
}

/**
 * `installTestEnv` only ever SETS keys, so a test that armed the lane would leak it into the next
 * one. Empty is how this app spells unset everywhere (compose and every deploy panel do too), so
 * clearing is writing the empty string, not deleting the key.
 */
function clearGatewayEnv(): void {
  process.env.GATEWAY_INTERNAL_URL = "";
  process.env.GATEWAY_INTERNAL_TOKEN = "";
  process.env.GATEWAY_PUBLIC_URL = "";
}

beforeEach(() => {
  vi.resetModules();
  vi.unstubAllGlobals();
  clearGatewayEnv();
});

describe("the route allowlist", () => {
  it("carries exactly the four internal routes this tier may call", async () => {
    const { ALLOWED_ROUTES } = await client();
    expect([...ALLOWED_ROUTES]).toEqual([
      "POST /internal/v1/authorize/begin",
      "POST /internal/v1/credentials/manual",
      "DELETE /internal/v1/credentials/{credentialId}",
      "POST /internal/v1/tools/refresh",
    ]);
  });

  it("refuses an off-list route before any request is made", async () => {
    installTestEnv({
      GATEWAY_INTERNAL_URL: "http://gateway.internal:8080",
      GATEWAY_INTERNAL_TOKEN: "internal-token-unit",
    });
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    const { gatewayFetch, isAllowedRoute } = await client();

    expect(isAllowedRoute("GET", "/internal/v1/credentials/{credentialId}")).toBe(false);
    await expect(
      gatewayFetch({
        method: "DELETE",
        template: "/internal/v1/credentials",
      }),
    ).rejects.toThrow(/not allowlisted/);
    // The method matters as much as the path: a listed path under another verb is off-list too.
    await expect(
      gatewayFetch({ method: "POST", template: "/internal/v1/credentials/{credentialId}" }),
    ).rejects.toThrow(/not allowlisted/);
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("URL-encodes every path param, so an id can never escape its segment", async () => {
    const { fillTemplate } = await client();
    expect(fillTemplate("/internal/v1/credentials/{credentialId}", { credentialId: "a/b c" })).toBe(
      "/internal/v1/credentials/a%2Fb%20c",
    );
    expect(() => fillTemplate("/internal/v1/credentials/{credentialId}", {})).toThrow(
      /missing path param/,
    );
  });

  it("sends the internal bearer and nothing else identifying", async () => {
    installTestEnv({
      GATEWAY_INTERNAL_URL: "http://gateway.internal:8080",
      GATEWAY_INTERNAL_TOKEN: "internal-token-unit",
    });
    const fetchSpy = vi.fn(async () => new Response(null, { status: 204 }));
    vi.stubGlobal("fetch", fetchSpy);
    const { gatewayFetch } = await client();
    await gatewayFetch({
      method: "DELETE",
      template: "/internal/v1/credentials/{credentialId}",
      params: { credentialId: "cred_1" },
    });
    expect(fetchSpy).toHaveBeenCalledTimes(1);
    const [url, init] = fetchSpy.mock.calls[0] as unknown as [string, RequestInit];
    expect(url).toBe("http://gateway.internal:8080/internal/v1/credentials/cred_1");
    const headers = new Headers(init.headers);
    expect(headers.get("authorization")).toBe("Bearer internal-token-unit");
    expect([...headers.keys()].sort()).toEqual(["authorization"]);
  });
});

describe("a deployment with no gateway", () => {
  it("answers null for the lane and refuses to dial", async () => {
    installTestEnv();
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    const { gatewayFetch, gatewayLane } = await client();
    expect(gatewayLane()).toBeNull();
    await expect(
      gatewayFetch({ method: "POST", template: "/internal/v1/authorize/begin", body: {} }),
    ).rejects.toThrow(/no gateway is configured/);
    expect(fetchSpy).not.toHaveBeenCalled();
  });

  it("folds that refusal into a typed fault at the wrapper, never an exception into a ceremony", async () => {
    installTestEnv();
    vi.stubGlobal("fetch", vi.fn());
    const { beginAuthorize, deleteCredential, storeManualCredential } = await import(
      "@/lib/gateway/credentials.server"
    );
    expect(
      await beginAuthorize({ workspaceId: "w", serverId: "s", userId: null, returnTo: "/" }),
    ).toEqual({ kind: "fault" });
    expect(
      await storeManualCredential({
        workspaceId: "w",
        serverId: "s",
        userId: null,
        secret: "shh",
        createdByDisplay: "Mo",
      }),
    ).toEqual({ kind: "fault" });
    expect(await deleteCredential("cred_1")).toEqual({ kind: "fault" });
  });

  it("refuses to start with half a lane configured", async () => {
    installTestEnv({ GATEWAY_INTERNAL_URL: "http://gateway.internal:8080" });
    const { serverEnv } = await import("@/env.server");
    expect(() => serverEnv()).toThrow(/set together or not at all/);
  });
});

describe("the wrappers fold status into typed outcomes", () => {
  beforeEach(() => {
    installTestEnv({
      GATEWAY_INTERNAL_URL: "http://gateway.internal:8080",
      GATEWAY_INTERNAL_TOKEN: "internal-token-unit",
    });
  });

  it("hands back the authorize URL the gateway minted", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => Response.json({ authorizeUrl: "https://upstream.example/authorize" })),
    );
    const { beginAuthorize } = await import("@/lib/gateway/credentials.server");
    expect(
      await beginAuthorize({ workspaceId: "w", serverId: "s", userId: "u", returnTo: "/" }),
    ).toEqual({ kind: "authorize", authorizeUrl: "https://upstream.example/authorize" });
  });

  it("reads the gateway's own refusal vocabulary and nothing beyond it", async () => {
    const { beginAuthorize } = await import("@/lib/gateway/credentials.server");
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => Response.json({ code: "MANUAL_ONLY" }, { status: 409 })),
    );
    expect(
      await beginAuthorize({ workspaceId: "w", serverId: "s", userId: "u", returnTo: "/" }),
    ).toEqual({ kind: "refused", code: "MANUAL_ONLY" });

    vi.stubGlobal(
      "fetch",
      vi.fn(async () => Response.json({ code: "SOMETHING_ELSE" }, { status: 409 })),
    );
    expect(
      await beginAuthorize({ workspaceId: "w", serverId: "s", userId: "u", returnTo: "/" }),
    ).toEqual({ kind: "fault" });
  });

  it("treats an already-gone credential as deleted, so revoking twice is not a fault", async () => {
    vi.stubGlobal(
      "fetch",
      vi.fn(async () => new Response(null, { status: 404 })),
    );
    const { deleteCredential } = await import("@/lib/gateway/credentials.server");
    expect(await deleteCredential("cred_gone")).toEqual({ kind: "deleted" });
  });
});
