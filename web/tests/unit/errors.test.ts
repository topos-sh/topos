import { describe, expect, it } from "vitest";
import {
  CAP_REASON,
  createFailureFrom,
  failureFromResponse,
  REVERT_DENIED_REASONS,
  REVIEW_DENIED_REASONS,
  readOutcome,
  revertFailureFrom,
  reviewFailureFrom,
  tooLargeFailure,
  unreachableFailure,
} from "@/lib/plane/errors";

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

describe("readOutcome", () => {
  it("parses a typed outcome body", async () => {
    const res = new Response(JSON.stringify({ outcome: "approved" }), { status: 200 });
    expect(await readOutcome<{ outcome: string }>(res)).toEqual({ outcome: "approved" });
  });

  it("returns undefined for a non-JSON body (a fault page)", async () => {
    const res = new Response("not json at all", { status: 500 });
    expect(await readOutcome(res)).toBeUndefined();
  });
});

describe("createFailureFrom", () => {
  it("maps a denied create outcome, relaying the static cap reason", () => {
    const failure = createFailureFrom({ outcome: "denied", reason: CAP_REASON });
    expect(failure).toMatchObject({ ok: false, kind: "create_denied", reason: CAP_REASON });
  });

  it("an absent outcome folds to create_denied with no reason", () => {
    expect(createFailureFrom(undefined)).toMatchObject({ kind: "create_denied", reason: "" });
  });
});

describe("REVIEW_DENIED_REASONS (the byte contract)", () => {
  it("pins the vault's four static reason strings verbatim", () => {
    // These are the exact bytes the vault emits — the same literals are asserted on the wire in
    // the vault's own e2e. A drift here silently degrades the action's branching to generic
    // denied copy, so any change must land on both sides at once.
    expect(REVIEW_DENIED_REASONS).toEqual({
      roleGate: "approving or rejecting needs an owner or reviewer seat",
      fourEyes: "the proposer may not approve their own proposal under review-required",
      notOpen: "no open proposal for this candidate and base",
      alreadyAccepted: "the proposal is already accepted",
    });
  });
});

describe("reviewFailureFrom", () => {
  it("dispatches the conflict outcome (the stale-base CAS refusal)", () => {
    const failure = reviewFailureFrom({ outcome: "conflict" });
    expect(failure).toMatchObject({
      ok: false,
      kind: "review_conflict",
      code: "review_conflict",
      retryable: false,
    });
    expect(failure.reason).toBeUndefined();
  });

  it("dispatches the not_found outcome to the uniform miss", () => {
    expect(reviewFailureFrom({ outcome: "not_found" })).toMatchObject({
      ok: false,
      kind: "not_found",
    });
  });

  it("dispatches denied carrying the vault's static reason verbatim", () => {
    for (const reason of Object.values(REVIEW_DENIED_REASONS)) {
      const failure = reviewFailureFrom({ outcome: "denied", reason });
      expect(failure).toMatchObject({
        ok: false,
        kind: "review_denied",
        reason,
        retryable: false,
      });
    }
  });

  it("an unrecognized/absent outcome folds to review_denied with no reason (generic copy)", () => {
    for (const outcome of [undefined, { outcome: "surprise" }, { outcome: "" }]) {
      const failure = reviewFailureFrom(outcome);
      expect(failure).toMatchObject({ ok: false, kind: "review_denied", reason: "" });
    }
  });

  it("never leaks a URL or token into a review message", () => {
    for (const outcome of [
      { outcome: "conflict", reason: SECRET_URL },
      { outcome: "denied", reason: REVIEW_DENIED_REASONS.notOpen },
    ]) {
      const failure = reviewFailureFrom(outcome);
      expect(failure.message).not.toContain("http");
      expect(failure.message).not.toContain(SECRET_TOKEN);
      expect(failure.message).not.toContain("/i/");
    }
  });
});

describe("REVERT_DENIED_REASONS (the byte contract)", () => {
  it("pins the vault's static reason strings verbatim", () => {
    // The exact bytes the vault emits (the vault's e2e pins the same literals on the wire). The
    // role gate is DELIBERATELY the shared approve/reject string — the same op relays it for
    // both verbs — which the revert action maps to its own verb-appropriate copy; a drift here
    // silently breaks the benign-double-click and role-gate branches, so any change lands on both
    // sides at once.
    expect(REVERT_DENIED_REASONS).toEqual({
      roleGate: "approving or rejecting needs an owner or reviewer seat",
      opIdReused: "op id reused with a different request",
    });
    // The role gate is byte-identical to the review verbs' role gate (one shared vault string).
    expect(REVERT_DENIED_REASONS.roleGate).toBe(REVIEW_DENIED_REASONS.roleGate);
  });
});

describe("revertFailureFrom", () => {
  it("dispatches the conflict outcome (the stale-generation CAS refusal)", () => {
    const failure = revertFailureFrom({ outcome: "conflict" });
    expect(failure).toMatchObject({
      ok: false,
      kind: "revert_conflict",
      code: "revert_conflict",
      retryable: false,
    });
    expect(failure.reason).toBeUndefined();
  });

  it("dispatches the not_found outcome to the uniform miss", () => {
    expect(revertFailureFrom({ outcome: "not_found" })).toMatchObject({
      ok: false,
      kind: "not_found",
    });
  });

  it("dispatches denied carrying the vault's static reason verbatim", () => {
    for (const reason of [
      ...Object.values(REVERT_DENIED_REASONS),
      "revert target is not an accepted version",
    ]) {
      const failure = revertFailureFrom({ outcome: "denied", reason });
      expect(failure).toMatchObject({
        ok: false,
        kind: "revert_denied",
        reason,
        retryable: false,
      });
    }
  });

  it("an unrecognized/absent outcome folds to revert_denied with no reason (generic copy)", () => {
    for (const outcome of [undefined, { outcome: "surprise" }, { outcome: "" }]) {
      const failure = revertFailureFrom(outcome);
      expect(failure).toMatchObject({ ok: false, kind: "revert_denied", reason: "" });
    }
  });

  it("never leaks a URL or token into a revert message", () => {
    for (const outcome of [
      { outcome: "conflict", reason: SECRET_URL },
      { outcome: "denied", reason: REVERT_DENIED_REASONS.roleGate },
    ]) {
      const failure = revertFailureFrom(outcome);
      expect(failure.message).not.toContain("http");
      expect(failure.message).not.toContain(SECRET_TOKEN);
      expect(failure.message).not.toContain("/i/");
    }
  });
});
