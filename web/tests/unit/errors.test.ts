import { describe, expect, it } from "vitest";
import { failureFromResponse, tooLargeFailure, unreachableFailure } from "@/lib/plane/errors";

function responseWith(status: number, headers: Record<string, string> = {}): Response {
  // Response() forbids some statuses as constructor args; synthesize via a plain object shape.
  return {
    status,
    headers: new Headers(headers),
  } as unknown as Response;
}

const SECRET_TOKEN = "tok-super-secret-abc123";
const SECRET_URL = `http://vault.internal:8080/i/${SECRET_TOKEN}`;

describe("failureFromResponse", () => {
  it("maps 404 to not_found with the exact 'not found' copy (never an access claim)", () => {
    const failure = failureFromResponse(responseWith(404), undefined);
    expect(failure).toMatchObject({
      ok: false,
      kind: "not_found",
      retryable: false,
      status: 404,
      message: "not found",
    });
    expect(failure.message).not.toMatch(/access|denied|permission/i);
  });

  it("maps 429 to rate_limited, retryable", () => {
    const failure = failureFromResponse(responseWith(429, { "retry-after": "3" }), undefined);
    expect(failure).toMatchObject({ kind: "rate_limited", retryable: true, status: 429 });
  });

  it("maps 401/403 to denied", () => {
    expect(failureFromResponse(responseWith(403), undefined).kind).toBe("denied");
    expect(failureFromResponse(responseWith(401), undefined).kind).toBe("denied");
  });

  it("maps 5xx (and anything unrecognized) to plane_fault", () => {
    expect(failureFromResponse(responseWith(500), undefined).kind).toBe("plane_fault");
    expect(failureFromResponse(responseWith(502), undefined).kind).toBe("plane_fault");
    expect(failureFromResponse(responseWith(418), undefined).kind).toBe("plane_fault");
  });

  it("extracts the stable code and retryability from an error-envelope body", () => {
    const envelope = {
      schema_version: 1,
      command: "pull",
      ok: false,
      error: { code: "not_entitled", outcome: "Refused", retryable: false },
    };
    const failure = failureFromResponse(responseWith(403), envelope);
    expect(failure.code).toBe("not_entitled");
    expect(failure.retryable).toBe(false);
  });

  it("a Retry-After header marks any failure retryable", () => {
    const failure = failureFromResponse(responseWith(500, { "retry-after": "10" }), undefined);
    expect(failure.retryable).toBe(true);
  });

  it("tolerates non-envelope bodies", () => {
    expect(failureFromResponse(responseWith(500), "plain text").kind).toBe("plane_fault");
    expect(failureFromResponse(responseWith(500), null).code).toBeUndefined();
    expect(failureFromResponse(responseWith(500), { error: "string" }).code).toBeUndefined();
  });

  it("never leaks a URL or token into any message", () => {
    const envelope = {
      ok: false,
      error: {
        code: "some_code",
        retryable: false,
        context: { url: SECRET_URL, token: SECRET_TOKEN },
      },
    };
    for (const status of [404, 429, 401, 403, 500, 503]) {
      const failure = failureFromResponse(responseWith(status), envelope);
      expect(failure.message).not.toContain("http");
      expect(failure.message).not.toContain(SECRET_TOKEN);
      expect(failure.message).not.toContain("/i/");
    }
    for (const failure of [unreachableFailure(), tooLargeFailure()]) {
      expect(failure.message).not.toContain("http");
      expect(failure.message).not.toContain(SECRET_TOKEN);
    }
  });
});

describe("unreachableFailure / tooLargeFailure", () => {
  it("unreachable is retryable, too_large is not", () => {
    expect(unreachableFailure()).toMatchObject({
      ok: false,
      kind: "unreachable",
      retryable: true,
    });
    expect(tooLargeFailure()).toMatchObject({ ok: false, kind: "too_large", retryable: false });
  });
});
