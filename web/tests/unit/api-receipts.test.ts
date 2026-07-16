import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { describe, expect, it } from "vitest";
import {
  HEX_64,
  OP_ID,
  parseCandidate,
  parsePublishHead,
  receiptNow,
  WIRE_ID,
} from "@/lib/api/candidate.server";
import {
  buildReceipt,
  conflictEnvelope,
  deniedEnvelope,
  envelopeResponse,
  errorReceiptEnvelope,
  okReceiptEnvelope,
} from "@/lib/api/receipts.server";

/**
 * The publish-family envelope/receipt builders + the door's body validation — PURE, pinned
 * against the committed contract fixtures where the fields survive the identity flip. NOTE the
 * fixtures are current: `Generation` is a BARE NUMBER now (`expected_generation: 42`), not the
 * historical `{epoch, seq}` pair — the builders and these assertions speak the new shape.
 */

const FIXTURES = resolve(__dirname, "..", "..", "..", "contracts", "fixtures", "json");

function fixture(name: string): Record<string, unknown> {
  return JSON.parse(readFileSync(join(FIXTURES, name), "utf8"));
}

describe("buildReceipt + okReceiptEnvelope against publish.downgraded.json", () => {
  it("reconstructs the fixture byte-for-byte from its own fields", () => {
    const expected = fixture("publish.downgraded.json");
    const receipt = buildReceipt({
      opId: "f47ac10b-58cc-4372-a567-0e02b2c3d479",
      command: "publish",
      outcome: "NEEDS_REVIEW",
      workspaceId: "w_demo",
      skillId: "s_prdescribe",
      versionId: "3f786850e387550fdab836ed7e6dc881de23001b3f786850e387550fdab836ed",
      bundleDigest: "89e6c98d92887913cadf06b2adb97f26cde4849b89e6c98d92887913cadf06b2",
      expectedGeneration: 42,
      createdAt: "2026-06-25T00:00:00Z",
      details: { downgraded: true },
    });
    expect(receipt).toEqual(expected.receipt);
    // The generation is a BARE number on the wire.
    expect(receipt.expected_generation).toBe(42);
    expect(okReceiptEnvelope("publish", receipt)).toEqual(expected);
  });

  it("omits every absent optional field — never serialized as null", () => {
    const receipt = buildReceipt({
      opId: "f47ac10b-58cc-4372-a567-0e02b2c3d479",
      command: "publish",
      outcome: "OK",
      workspaceId: "w_demo",
      createdAt: "2026-06-25T00:00:00Z",
    });
    expect(Object.keys(receipt).sort()).toEqual([
      "command",
      "created_at",
      "op_id",
      "outcome",
      "schema_version",
      "workspace_id",
    ]);
  });
});

describe("conflictEnvelope against publish.conflict.json", () => {
  it("reconstructs the stale-CAS fixture: receipt + flat error, generations as bare numbers", () => {
    const expected = fixture("publish.conflict.json");
    const receipt = buildReceipt({
      opId: "9f1b8c2e-7a6d-4e3f-9b0a-1c2d3e4f5a6b",
      command: "publish",
      outcome: "CONFLICT",
      workspaceId: "w_demo",
      skillId: "s_prdescribe",
      expectedGeneration: 42,
      currentGeneration: 43,
      createdAt: "2026-06-25T00:00:00Z",
    });
    expect(receipt).toEqual(expected.receipt);
    expect(
      conflictEnvelope({
        command: "publish",
        skillName: "pr-describe",
        receipt,
        expectedGeneration: 42,
        currentGeneration: 43,
      }),
    ).toEqual(expected);
  });
});

describe("deniedEnvelope + errorReceiptEnvelope", () => {
  it("a DENIED names its code and rides the access-recovery actions on both halves", () => {
    // The receipt parameter is REQUIRED by the type now — a receipt-less DENIED is the op-WAL
    // wedge class, structurally closed at the signature.
    const receipt = buildReceipt({
      opId: "f47ac10b-58cc-4372-a567-0e02b2c3d478",
      command: "publish",
      outcome: "DENIED",
      workspaceId: "w1",
      createdAt: "2026-07-16T00:00:00Z",
    });
    const envelope = deniedEnvelope(
      "publish",
      "REVIEWER_ROLE_REQUIRED",
      "pr-describe",
      receipt,
    ) as {
      ok: boolean;
      next_actions: unknown;
      receipt?: unknown;
      error: Record<string, unknown>;
    };
    const actions = [
      { code: "REQUEST_ACCESS", argv: [] },
      { code: "CONTACT_ADMIN", argv: [] },
    ];
    expect(envelope.ok).toBe(false);
    expect(envelope.next_actions).toEqual(actions);
    expect(envelope.error).toEqual({
      code: "REVIEWER_ROLE_REQUIRED",
      outcome: "DENIED",
      retryable: false,
      affected: { skill: "pr-describe" },
      context: {},
      next_actions: actions,
    });
    expect(envelope.receipt).toEqual(receipt);
    // No skill name ⇒ `affected` stays {}.
    const bare = deniedEnvelope("publish", "OP_KEY_REUSED", undefined, receipt) as {
      error: { affected: unknown };
    };
    expect(bare.error.affected).toEqual({});
  });

  it("a DENIED carries its receipt when handed one — outcome DENIED, the op_id echoed", () => {
    // A write-family denial (four-eyes, role gate, key reuse) is still a write 200: the CLI
    // contract is that it carries a receipt, so its op-WAL clears instead of wedging.
    const receipt = buildReceipt({
      opId: "f47ac10b-58cc-4372-a567-0e02b2c3d479",
      command: "review",
      outcome: "DENIED",
      workspaceId: "w_demo",
      skillId: "s_prdescribe",
      createdAt: "2026-06-25T00:00:00Z",
    });
    const envelope = deniedEnvelope("review", "FOUR_EYES_REQUIRED", "pr-describe", receipt) as {
      ok: boolean;
      receipt?: { outcome: string; op_id: string };
      error: { code: string };
    };
    expect(envelope.ok).toBe(false);
    expect(envelope.error.code).toBe("FOUR_EYES_REQUIRED");
    expect(envelope.receipt).toEqual(receipt);
    expect(envelope.receipt?.outcome).toBe("DENIED");
    expect(envelope.receipt?.op_id).toBe("f47ac10b-58cc-4372-a567-0e02b2c3d479");
  });

  it("errorReceiptEnvelope carries the receipt only when the op minted one", () => {
    const error = {
      code: "TARGET_PURGED",
      outcome: "DENIED",
      retryable: false,
      affected: {},
      context: {},
      next_actions: [],
    };
    expect("receipt" in errorReceiptEnvelope("revert", error)).toBe(false);
    const receipt = buildReceipt({
      opId: "f47ac10b-58cc-4372-a567-0e02b2c3d479",
      command: "revert",
      outcome: "DENIED",
      workspaceId: "w_demo",
      createdAt: "2026-06-25T00:00:00Z",
    });
    const withReceipt = errorReceiptEnvelope("revert", error, receipt) as { receipt?: unknown };
    expect(withReceipt.receipt).toEqual(receipt);
  });
});

describe("envelopeResponse", () => {
  it("serializes verbatim as application/json (replays re-serve stored bytes)", async () => {
    const stored = { schema_version: 1, command: "publish", ok: true };
    const res = envelopeResponse(stored);
    expect(res.status).toBe(200);
    expect(res.headers.get("content-type")).toBe("application/json");
    expect(await res.json()).toEqual(stored);
  });
});

describe("parseCandidate (the device wire's candidate validation)", () => {
  const good = {
    files: [{ path: "SKILL.md", mode: "100644", content_base64: "aGk=" }],
    parents: ["ab".repeat(32)],
    author: "Ada <ada@example.com>",
    message: "tighten the rollback step",
  };

  it("accepts a well-formed candidate, both file modes", () => {
    expect(parseCandidate(good)).toEqual(good);
    const executable = {
      ...good,
      files: [{ path: "run.sh", mode: "100755", content_base64: "" }],
    };
    expect(parseCandidate(executable)).toEqual(executable);
  });

  it("refuses each malformed arm with a human-readable reason", () => {
    expect(parseCandidate(null)).toBe("malformed candidate");
    expect(parseCandidate("nope")).toBe("malformed candidate");
    expect(parseCandidate({ ...good, files: "x" })).toBe("malformed candidate: files");
    expect(
      parseCandidate({ ...good, files: [{ path: "", mode: "100644", content_base64: "" }] }),
    ).toBe("malformed candidate file");
    expect(
      parseCandidate({ ...good, files: [{ path: "a", mode: "040000", content_base64: "" }] }),
    ).toBe("malformed candidate file");
    expect(parseCandidate({ ...good, parents: ["not-hex"] })).toBe("malformed candidate: parents");
    expect(parseCandidate({ ...good, parents: "x" })).toBe("malformed candidate: parents");
    expect(parseCandidate({ ...good, author: "" })).toBe("malformed candidate: author");
    expect(parseCandidate({ ...good, message: 7 })).toBe("malformed candidate: message");
  });
});

describe("parsePublishHead (the shared head fields)", () => {
  const good = {
    workspace_id: "w_demo",
    skill_id: "s_prdescribe",
    op_id: "f47ac10b-58cc-4372-a567-0e02b2c3d479",
    expected: 42,
  };

  it("accepts a well-formed head; the genesis generation 0 included", () => {
    expect(parsePublishHead(good)).toEqual({
      workspaceId: "w_demo",
      skillId: "s_prdescribe",
      opId: "f47ac10b-58cc-4372-a567-0e02b2c3d479",
      expected: 42,
    });
    expect(parsePublishHead({ ...good, expected: 0 })).toMatchObject({ expected: 0 });
  });

  it("refuses each malformed field by name — expected is a bare non-negative safe integer", () => {
    expect(parsePublishHead({ ...good, workspace_id: "w/../x" })).toBe("malformed workspace_id");
    expect(parsePublishHead({ ...good, skill_id: "" })).toBe("malformed skill_id");
    expect(parsePublishHead({ ...good, op_id: "F47AC10B-58CC-4372-A567-0E02B2C3D479" })).toBe(
      "malformed op_id",
    );
    expect(parsePublishHead({ ...good, expected: -1 })).toBe("malformed expected generation");
    expect(parsePublishHead({ ...good, expected: 1.5 })).toBe("malformed expected generation");
    // The historical {epoch, seq} pair is NOT the wire shape anymore — a bare number is.
    expect(parsePublishHead({ ...good, expected: { epoch: 1, seq: 2 } })).toBe(
      "malformed expected generation",
    );
  });
});

describe("the shared wire regexes + the receipt clock", () => {
  it("HEX_64 / WIRE_ID / OP_ID pin their charsets", () => {
    expect(HEX_64.test("ab".repeat(32))).toBe(true);
    expect(HEX_64.test("AB".repeat(32))).toBe(false);
    expect(WIRE_ID.test("w_demo.x-1")).toBe(true);
    expect(WIRE_ID.test("a/b")).toBe(false);
    expect(OP_ID.test("f47ac10b-58cc-4372-a567-0e02b2c3d479")).toBe(true);
    expect(OP_ID.test("f47ac10b58cc4372a5670e02b2c3d479")).toBe(false);
  });

  it("receiptNow speaks RFC-3339 seconds + Z — no millis, matching the vault's now_utc", () => {
    expect(receiptNow()).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$/);
  });
});
