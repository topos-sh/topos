import { beforeAll, describe, expect, it } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * THE FEED'S RESTING STATE — off, and off means the paths are not there at all.
 *
 * Its own file because the env parse is memoized for the life of a module graph, which is the
 * point: a deployment decides once, at boot, whether it publishes a catalog other installs
 * follow. The refusal is the house's uniform miss and lands BEFORE any database read, so a
 * deployment that never turned this on cannot be told what it holds.
 */

beforeAll(() => {
  installTestEnv();
});

describe("a deployment that publishes no catalog", () => {
  async function get(path: string): Promise<Response> {
    const { loader } = await import("@/routes/mcp-catalog-feed");
    return await loader({
      request: new Request(`http://x${path}`),
      params: {},
      context: {},
    } as unknown as Parameters<typeof loader>[0]);
  }

  it("answers the uniform 404 on every shape the feed would otherwise serve", async () => {
    for (const path of [
      "/mcp-catalog/v0.1/servers",
      "/mcp-catalog/v0.1/servers/io.github.acme%2Fweather/versions",
      "/mcp-catalog/v0.1/servers/io.github.acme%2Fweather/versions/latest",
    ]) {
      const res = await get(path);
      expect(res.status).toBe(404);
      expect(await res.json()).toMatchObject({ error: { code: "NOT_FOUND" } });
    }
  });
});
