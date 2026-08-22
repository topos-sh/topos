import { beforeAll, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * THE DELIVERY FLIP, decided in one pure function.
 *
 * Three answers this suite pins, because each one is a deployment shape rather than a preference:
 *  - GATEWAY_PUBLIC_URL unset ⇒ the document is handed over UNTOUCHED, byte for byte, which is
 *    what makes the flip reversible by clearing one variable;
 *  - a PACKAGE-ONLY document passes through whatever the env says: there is no address to
 *    redirect, and inventing one would promise a capability that does not exist;
 *  - a rewritten document keeps everything else it carried — name, version, packages, and any
 *    `_meta` the catalog put there (the auth tier especially) — and replaces exactly two keys.
 */

const ADDRESSED = {
  name: "com.example/weather",
  version: "1.4.0",
  remotes: [
    { type: "streamable-http", url: "https://weather.example.com/mcp" },
    { type: "sse", url: "https://weather.example.com/sse" },
  ],
  _meta: { "sh.topos/auth": "oauth" },
} as const;

const PACKAGED = {
  name: "com.example/local",
  version: "2.0.0",
  packages: [{ registryType: "npm", identifier: "@example/local-mcp", version: "2.0.0" }],
} as const;

async function delivery() {
  return await import("@/lib/gateway/delivery.server");
}

beforeAll(() => {
  installTestEnv();
});

describe("the rewrite itself", () => {
  it("replaces every remote with the ONE gateway address and marks it", async () => {
    const { GATEWAY_META_KEY, gatewayDeliveryDocument } = await delivery();
    const out = gatewayDeliveryDocument({ ...ADDRESSED } as Record<string, unknown>, {
      base: "https://gw.example.com",
      sessionId: "cs_laptop",
      serverId: "mcps_weather",
    });
    expect(out.remotes).toEqual([
      { type: "streamable-http", url: "https://gw.example.com/cs_laptop/mcps_weather" },
    ]);
    expect((out._meta as Record<string, unknown>)[GATEWAY_META_KEY]).toBe(true);
    // The tier is REWRITTEN to none beside the flag: the machine-side question the key answers
    // ("what must this machine do before tools work") has a new answer through the gateway.
    expect((out._meta as Record<string, unknown>)["sh.topos/auth"]).toBe("none");
    expect(out.name).toBe("com.example/weather");
    expect(out.version).toBe("1.4.0");
  });

  it("does not mutate the document it was handed", async () => {
    const { gatewayDeliveryDocument } = await delivery();
    const original = JSON.parse(JSON.stringify(ADDRESSED)) as Record<string, unknown>;
    gatewayDeliveryDocument(original, {
      base: "https://gw.example.com",
      sessionId: "cs_laptop",
      serverId: "mcps_weather",
    });
    expect(original).toEqual(ADDRESSED);
  });

  it("URL-encodes the two ids it puts in the path", async () => {
    const { gatewayDeliveryDocument } = await delivery();
    const out = gatewayDeliveryDocument({ ...ADDRESSED } as Record<string, unknown>, {
      base: "https://gw.example.com",
      sessionId: "cs/../evil",
      serverId: "a b",
    });
    expect((out.remotes as { url: string }[])[0]?.url).toBe(
      "https://gw.example.com/cs%2F..%2Fevil/a%20b",
    );
  });

  it("hands a PACKAGE-ONLY document straight back, unchanged and identical", async () => {
    const { gatewayDeliveryDocument } = await delivery();
    const input = { ...PACKAGED } as Record<string, unknown>;
    const out = gatewayDeliveryDocument(input, {
      base: "https://gw.example.com",
      sessionId: "cs_laptop",
      serverId: "mcps_local",
    });
    expect(out).toBe(input);
    expect(out._meta).toBeUndefined();
  });

  it("leaves a document whose only remotes are of another transport alone", async () => {
    const { gatewayDeliveryDocument, hasStreamableRemote } = await delivery();
    const sseOnly = { name: "x/y", remotes: [{ type: "sse", url: "https://x.example/sse" }] };
    expect(hasStreamableRemote(sseOnly)).toBe(false);
    expect(
      gatewayDeliveryDocument(sseOnly, {
        base: "https://gw.example.com",
        sessionId: "cs",
        serverId: "s",
      }),
    ).toBe(sseOnly);
  });
});

describe("the env decides whether the flip happens at all", () => {
  it("answers null while GATEWAY_PUBLIC_URL is unset — nothing is rewritten", async () => {
    process.env.GATEWAY_PUBLIC_URL = "";
    const { gatewayPublicBase } = await import("@/lib/gateway/delivery.server");
    expect(gatewayPublicBase()).toBeNull();
  });

  it("trims the trailing slash so the built address never doubles it", async () => {
    // serverEnv() memoizes on first read, so this needs its own module registry.
    vi.resetModules();
    installTestEnv({ GATEWAY_PUBLIC_URL: "https://gw.example.com/" });
    const { gatewayPublicBase } = await import("@/lib/gateway/delivery.server");
    expect(gatewayPublicBase()).toBe("https://gw.example.com");
  });
});
