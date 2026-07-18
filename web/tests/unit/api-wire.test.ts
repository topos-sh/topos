import { beforeAll, describe, expect, it } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * The device lane's transport-fault envelopes + the bearer parser (app/lib/api/wire.server.ts) —
 * pinned as LITERALS against the vault's frozen shapes. These bodies are the wire contract's
 * error family; a field drifting here is a cross-tier regression the CLI would meet in
 * production, so the assertions are exact-object, not shape-ish.
 */

let wire: typeof import("@/lib/api/wire.server");

beforeAll(async () => {
  installTestEnv();
  wire = await import("@/lib/api/wire.server");
});

describe("the uniform 404", () => {
  it("is the exact envelope the vault answers for every miss", async () => {
    const res = wire.uniformNotFound();
    expect(res.status).toBe(404);
    expect(res.headers.get("content-type")).toContain("application/json");
    expect(await res.json()).toEqual({
      schema_version: 1,
      command: "error",
      ok: false,
      data: {},
      warnings: [],
      next_actions: [],
      error: {
        code: "NOT_FOUND",
        outcome: "PERMANENT_FAILURE",
        retryable: false,
        affected: {},
        context: { message: "not found" },
        next_actions: [],
      },
    });
  });

  it("is byte-identical across calls (no request detail ever shapes it)", async () => {
    const a = await wire.uniformNotFound().text();
    const b = await wire.uniformNotFound().text();
    expect(a).toBe(b);
  });
});

describe("the 400 family", () => {
  it("carries the caller's message in context and nothing else request-shaped", async () => {
    const res = wire.badRequest("malformed report entry: skill_id");
    expect(res.status).toBe(400);
    const body = (await res.json()) as { error: { code: string; context: unknown } };
    expect(body.error.code).toBe("BAD_REQUEST");
    expect(body.error.context).toEqual({ message: "malformed report entry: skill_id" });
  });
});

describe("the frozen 429", () => {
  it("answers Retry-After + the RATE_LIMITED retryable envelope with one Retry action", async () => {
    const res = wire.rateLimited(7);
    expect(res.status).toBe(429);
    expect(res.headers.get("retry-after")).toBe("7");
    expect(await res.json()).toEqual({
      schema_version: 1,
      command: "rate_limited",
      ok: false,
      data: {},
      warnings: [],
      next_actions: [{ code: "RETRY", argv: [], needs_network: true }],
      error: {
        code: "RATE_LIMITED",
        outcome: "RETRYABLE_FAILURE",
        retryable: true,
        affected: {},
        context: { retry_after_seconds: 7 },
        next_actions: [{ code: "RETRY", argv: [], needs_network: true }],
      },
    });
  });
});

describe("the 500", () => {
  it("is flat and retryable — no internal detail crosses the wire", async () => {
    const res = wire.internalError();
    expect(res.status).toBe(500);
    const body = (await res.json()) as { error: { code: string; retryable: boolean } };
    expect(body.error.code).toBe("INTERNAL");
    expect(body.error.retryable).toBe(true);
  });
});

describe("readCappedBody — cap before buffering", () => {
  const CAP = 64 * 1024;
  const put = (body: string, contentLength?: string) =>
    new Request("http://x/api/v1/workspaces/w/report", {
      method: "PUT",
      body,
      headers: contentLength === undefined ? {} : { "content-length": contentLength },
    });

  it("returns the body text when under the cap", async () => {
    const r = await wire.readCappedBody(put("{}"), CAP, "report body");
    expect(r).toBe("{}");
  });

  it("refuses up front on a declared Content-Length over the cap (no buffering)", async () => {
    // A small actual body but a lying oversize Content-Length is refused before the read —
    // the precheck the vault's stream-enforced cap mirrors.
    const r = await wire.readCappedBody(put("{}", String(CAP + 1)), CAP, "report body");
    expect(r).toBeInstanceOf(Response);
    expect((r as Response).status).toBe(400);
  });

  it("still length-checks the read when no Content-Length is declared (chunked)", async () => {
    const big = "x".repeat(CAP + 10);
    const r = await wire.readCappedBody(put(big), CAP, "report body");
    expect(r).toBeInstanceOf(Response);
    expect((r as Response).status).toBe(400);
  });
});

describe("bearerToken — the vault edge's exact extraction", () => {
  const req = (auth?: string) =>
    new Request("http://x/api/v1/workspaces/w/me", {
      headers: auth === undefined ? {} : { authorization: auth },
    });

  it("accepts the two literal prefixes and trims the remainder", () => {
    expect(wire.bearerToken(req("Bearer cred-1"))).toBe("cred-1");
    expect(wire.bearerToken(req("bearer cred-1"))).toBe("cred-1");
    expect(wire.bearerToken(req("Bearer   padded   "))).toBe("padded");
  });

  it("refuses everything else exactly like the vault: missing, blank, foreign scheme, odd casing", () => {
    expect(wire.bearerToken(req())).toBeNull();
    expect(wire.bearerToken(req(""))).toBeNull();
    expect(wire.bearerToken(req("Bearer "))).toBeNull();
    expect(wire.bearerToken(req("Bearer"))).toBeNull();
    expect(wire.bearerToken(req("Basic dXNlcg=="))).toBeNull();
    // The vault strips ONLY `Bearer ` / `bearer ` — any other casing is a foreign scheme.
    expect(wire.bearerToken(req("BEARER cred-1"))).toBeNull();
    expect(wire.bearerToken(req("BeArEr cred-1"))).toBeNull();
  });
});
