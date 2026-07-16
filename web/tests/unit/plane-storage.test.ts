import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { storageStats } from "@/lib/plane/storage.server";
import { installTestEnv } from "./helpers/test-env";

/**
 * The storage-stat transport read against an in-process stub vault (the same fixture pattern as
 * the skill-current stub in api-v1-routes.test.ts): the stub records the exact path + bearer the
 * transport sent and answers a configurable body, so both the route (allowlisted, correct
 * template) and the defensive parse (a malformed body is an error, never NaN) are pinned.
 */

let stub: Server;
let stubStatus = 200;
let stubBody = "";
const seen: { path?: string; auth?: string } = {};

beforeAll(async () => {
  stub = createServer((request, response) => {
    seen.path = request.url;
    seen.auth = request.headers.authorization;
    response.statusCode = stubStatus;
    response.setHeader("content-type", "application/json");
    response.end(stubBody);
  });
  await new Promise<void>((resolve) => stub.listen(0, "127.0.0.1", resolve));
  const port = (stub.address() as AddressInfo).port;
  installTestEnv({ PLANE_INTERNAL_URL: `http://127.0.0.1:${port}` });
});

afterAll(async () => {
  await new Promise<void>((resolve, reject) => stub.close((e) => (e ? reject(e) : resolve())));
});

function answer(status: number, body: string): void {
  stubStatus = status;
  stubBody = body;
}

describe("storageStats", () => {
  it("calls the allowlisted lane path with the internal bearer and maps the workspaces", async () => {
    answer(
      200,
      JSON.stringify({
        workspaces: [
          { workspace_id: "w1", stored_bytes: 5 },
          { workspace_id: "w2", stored_bytes: 0 },
        ],
      }),
    );
    const stats = await storageStats();
    expect(seen.path).toBe("/internal/v1/storage");
    expect(seen.auth).toBe("Bearer internal-token-unit");
    expect(stats.size).toBe(2);
    expect(stats.get("w1")).toBe(5);
    expect(stats.get("w2")).toBe(0);
  });

  it("answers an empty map for an empty workspaces list", async () => {
    answer(200, JSON.stringify({ workspaces: [] }));
    expect((await storageStats()).size).toBe(0);
  });

  it("throws on a non-2xx status", async () => {
    answer(500, JSON.stringify({ code: "INTERNAL" }));
    await expect(storageStats()).rejects.toThrow("storage stats read failed (status 500)");
  });

  it("throws on a non-JSON body", async () => {
    answer(200, "not json");
    await expect(storageStats()).rejects.toThrow("non-JSON body");
  });

  it.each([
    ["a non-object body", JSON.stringify([])],
    ["a missing workspaces key", JSON.stringify({})],
    ["a non-array workspaces", JSON.stringify({ workspaces: {} })],
    ["a non-object entry", JSON.stringify({ workspaces: [7] })],
    ["a missing workspace id", JSON.stringify({ workspaces: [{ stored_bytes: 5 }] })],
    [
      "an empty workspace id",
      JSON.stringify({ workspaces: [{ workspace_id: "", stored_bytes: 5 }] }),
    ],
    [
      "stringly bytes (never NaN)",
      JSON.stringify({ workspaces: [{ workspace_id: "w1", stored_bytes: "5" }] }),
    ],
    ["missing bytes (never NaN)", JSON.stringify({ workspaces: [{ workspace_id: "w1" }] })],
    [
      "fractional bytes",
      JSON.stringify({ workspaces: [{ workspace_id: "w1", stored_bytes: 1.5 }] }),
    ],
    ["negative bytes", JSON.stringify({ workspaces: [{ workspace_id: "w1", stored_bytes: -1 }] })],
  ])("throws on %s", async (_label, body) => {
    answer(200, body);
    await expect(storageStats()).rejects.toThrow("storage stats body is malformed");
  });
});
