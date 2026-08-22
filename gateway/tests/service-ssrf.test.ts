import http from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, describe, expect, it } from "vitest";
import { addressBlocked, createGuardedFetch } from "../service/guarded-fetch";

const quiet = () => {};
const strict = createGuardedFetch({ allowPrivate: false }, quiet);

const servers: http.Server[] = [];
afterAll(async () => {
  for (const server of servers) {
    await new Promise((resolve) => server.close(resolve));
  }
});

function listen(handler: http.RequestListener): Promise<string> {
  const server = http.createServer(handler);
  servers.push(server);
  return new Promise((resolve) => {
    server.listen(0, "127.0.0.1", () => {
      resolve(`http://127.0.0.1:${(server.address() as AddressInfo).port}`);
    });
  });
}

describe("address classification", () => {
  it("blocks loopback, RFC 1918, CGNAT, link-local, metadata, and v6 equivalents", () => {
    for (const addr of [
      "127.0.0.1",
      "127.8.8.8",
      "10.0.0.5",
      "172.16.0.1",
      "172.31.255.255",
      "192.168.1.1",
      "169.254.169.254", // cloud metadata
      "100.100.100.200", // Alibaba metadata (CGNAT range)
      "100.64.0.1",
      "0.0.0.0",
      "224.0.0.1",
      "255.255.255.255",
      "::1",
      "::",
      "::ffff:127.0.0.1", // v4-mapped loopback
      "::ffff:10.0.0.1",
      "fe80::1",
      "fd00::1",
      "fc00::abcd",
      "ff02::1",
      "64:ff9b::a00:1", // NAT64
      "not-an-ip",
    ]) {
      expect(addressBlocked(addr), addr).toBe(true);
    }
  });

  it("passes public addresses", () => {
    for (const addr of ["8.8.8.8", "1.1.1.1", "93.184.216.34", "2606:4700::1111", "172.32.0.1"]) {
      expect(addressBlocked(addr), addr).toBe(false);
    }
  });
});

describe("the strict policy", () => {
  it("refuses plain http", async () => {
    await expect(strict("http://example.com/x")).rejects.toThrow(/https-only/);
  });

  it("refuses userinfo in the URL", async () => {
    // The Request constructor already throws on embedded credentials; the wrapper's own check
    // covers the redirect path, where no Request has been built yet. Either way: refused.
    await expect(strict("https://user:pass@example.com/")).rejects.toThrow(/credential|userinfo/i);
  });

  it("refuses private IP literals before any dial", async () => {
    for (const url of [
      "https://127.0.0.1/",
      "https://10.1.2.3/mcp",
      "https://169.254.169.254/latest/meta-data",
      "https://[::1]/",
      "https://[::ffff:127.0.0.1]/",
    ]) {
      await expect(strict(url), url).rejects.toThrow(/blocked range/);
    }
  });

  it("refuses a hostname that resolves privately", async () => {
    await expect(strict("https://localhost:9/")).rejects.toThrow(/blocked range/);
  });
});

describe("redirect handling (stubbed transport — public IP literals resolve nothing)", () => {
  it("blocks a redirect into a private range", async () => {
    const stub = (async (input: RequestInfo | URL) => {
      const url = new URL(input instanceof Request ? input.url : String(input));
      if (url.hostname === "8.8.8.8") {
        return new Response(null, {
          status: 302,
          headers: { location: "https://169.254.169.254/latest" },
        });
      }
      throw new Error(`unexpected dial: ${url.hostname}`);
    }) as typeof fetch;
    const guarded = createGuardedFetch({ allowPrivate: false }, quiet, stub);
    await expect(guarded("https://8.8.8.8/start")).rejects.toThrow(/blocked range/);
  });

  it("caps the redirect chain", async () => {
    let hops = 0;
    const stub = (async () => {
      hops += 1;
      return new Response(null, { status: 302, headers: { location: "https://8.8.8.8/next" } });
    }) as typeof fetch;
    const guarded = createGuardedFetch({ allowPrivate: false }, quiet, stub);
    await expect(guarded("https://8.8.8.8/start")).rejects.toThrow(/too long/);
    expect(hops).toBe(6); // the original dial + five followed hops, then refusal
  });

  it("drops Authorization when a redirect crosses origins, keeps it within one", async () => {
    const seen: Array<{ host: string; auth: string | null }> = [];
    const stub = (async (input: RequestInfo | URL) => {
      const request = input as Request;
      const url = new URL(request.url);
      seen.push({ host: url.hostname, auth: request.headers.get("authorization") });
      if (url.pathname === "/a") {
        return new Response(null, { status: 302, headers: { location: "/b" } });
      }
      if (url.pathname === "/b") {
        return new Response(null, { status: 302, headers: { location: "https://9.9.9.9/c" } });
      }
      return new Response("done");
    }) as typeof fetch;
    const guarded = createGuardedFetch({ allowPrivate: false }, quiet, stub);
    const response = await guarded("https://8.8.8.8/a", {
      headers: { authorization: "Bearer secret" },
    });
    expect(await response.text()).toBe("done");
    expect(seen).toEqual([
      { host: "8.8.8.8", auth: "Bearer secret" },
      { host: "8.8.8.8", auth: "Bearer secret" },
      { host: "9.9.9.9", auth: null },
    ]);
  });

  it("refuses to replay a body a 307 would resend", async () => {
    const stub = (async () =>
      new Response(null, {
        status: 307,
        headers: { location: "https://8.8.8.8/again" },
      })) as typeof fetch;
    const guarded = createGuardedFetch({ allowPrivate: false }, quiet, stub);
    await expect(
      guarded("https://8.8.8.8/post", { method: "POST", body: "payload" }),
    ).rejects.toThrow(/cannot be replayed/);
  });

  it("demotes POST to GET on 303 and drops the body", async () => {
    const seen: Array<{ method: string; hasBody: boolean }> = [];
    const stub = (async (input: RequestInfo | URL) => {
      const request = input as Request;
      seen.push({ method: request.method, hasBody: request.body !== null });
      if (seen.length === 1) {
        return new Response(null, { status: 303, headers: { location: "/next" } });
      }
      return new Response("ok");
    }) as typeof fetch;
    const guarded = createGuardedFetch({ allowPrivate: false }, quiet, stub);
    const response = await guarded("https://8.8.8.8/form", { method: "POST", body: "x=1" });
    expect(await response.text()).toBe("ok");
    expect(seen).toEqual([
      { method: "POST", hasBody: true },
      { method: "GET", hasBody: false },
    ]);
  });
});

describe("the private-upstreams override", () => {
  it("dials a loopback upstream when the deployment allows it", async () => {
    const base = await listen((_request, response) => {
      response.end("internal-ok");
    });
    const permissive = createGuardedFetch({ allowPrivate: true, allowInsecure: true }, quiet);
    const response = await permissive(`${base}/tools`);
    expect(await response.text()).toBe("internal-ok");
  });
});
