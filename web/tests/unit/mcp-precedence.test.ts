import { describe, expect, it } from "vitest";
import {
  compareServerVersion,
  isStaffAuthoredSource,
  isStrictlyNewer,
  type McpPrecedenceFacts,
} from "@/lib/mcp/precedence.server";

/**
 * PRECEDENCE — the pure comparison that decides whether an upstream document is worth more than the
 * one this install already holds. No database: the whole point of the module is that the rule is a
 * function of two revisions' facts and nothing else, and the `$schema` string is never one of them.
 */

function facts(version: string | null, protocolVersions: unknown = null): McpPrecedenceFacts {
  return { version, protocolVersions };
}

describe("ordering server versions", () => {
  it("orders the release core numerically, not lexically", () => {
    expect(compareServerVersion("2.0.0", "10.0.0")).toBe(-1);
    expect(compareServerVersion("1.2.0", "1.10.0")).toBe(-1);
    expect(compareServerVersion("1.2.3", "1.2.3")).toBe(0);
    expect(compareServerVersion("2.0.0", "1.9.9")).toBe(1);
  });

  it("treats a missing patch as a zero, and tolerates a leading v", () => {
    expect(compareServerVersion("1.2", "1.2.0")).toBe(0);
    expect(compareServerVersion("v1.3.0", "1.2.9")).toBe(1);
  });

  it("ranks a pre-release below its release, per semver", () => {
    expect(compareServerVersion("1.0.0-rc.1", "1.0.0")).toBe(-1);
    expect(compareServerVersion("1.0.0-rc.2", "1.0.0-rc.10")).toBe(-1);
    expect(compareServerVersion("1.0.0-alpha", "1.0.0-beta")).toBe(-1);
  });

  it("says it cannot order a version it does not read, rather than guessing", () => {
    expect(compareServerVersion("nightly", "1.0.0")).toBeNull();
    expect(compareServerVersion("1.0.0", "")).toBeNull();
  });

  it("cannot order a version that is ABSENT — a null is 'cannot say', never a zero", () => {
    // A revision that names no version has nothing to compare, so neither side of a null orders —
    // the accept guard reads this as "freely superseded", never as "1.0.0 beats null".
    expect(compareServerVersion(null, "1.0.0")).toBeNull();
    expect(compareServerVersion("1.0.0", null)).toBeNull();
    expect(compareServerVersion(null, null)).toBeNull();
  });

  it("treats a malformed label as ambiguous, never as a clean release", () => {
    // A trailing dash is an empty pre-release — not `2.0.0`, and it must not be able to supersede.
    expect(compareServerVersion("2.0.0-", "1.0.0")).toBeNull();
    expect(compareServerVersion("1.0.0-", "1.0.0")).toBeNull();
    // An empty pre-release identifier is malformed too.
    expect(compareServerVersion("1.0.0-rc.", "1.0.0")).toBeNull();
    expect(compareServerVersion("1.0.0-a..b", "1.0.0")).toBeNull();
  });

  it("orders identifiers past the safe-integer range losslessly", () => {
    // 20 digits vs 19 — `Number` would collapse both, this compares by magnitude.
    expect(compareServerVersion("1.0.10000000000000000000", "1.0.9999999999999999999")).toBe(1);
    expect(compareServerVersion("1.0.99999999999999999999", "1.0.99999999999999999998")).toBe(1);
    expect(compareServerVersion("1.0.99999999999999999999", "1.0.99999999999999999999")).toBe(0);
  });
});

describe("strictly newer", () => {
  it("a greater server version is strictly newer, whatever the schema might be", () => {
    expect(isStrictlyNewer(facts("2.0.0"), facts("1.0.0"))).toBe(true);
  });

  it("an older or equal server version is not", () => {
    expect(isStrictlyNewer(facts("1.0.0"), facts("2.0.0"))).toBe(false);
    expect(isStrictlyNewer(facts("1.0.0"), facts("1.0.0"))).toBe(false);
  });

  it("breaks a version tie on protocol support — but only for the candidate, never a tie", () => {
    // Equal server version, the candidate's endpoint reaches a newer MCP revision → strictly newer.
    expect(isStrictlyNewer(facts("1.0.0", ["2025-06-18"]), facts("1.0.0", ["2025-03-26"]))).toBe(
      true,
    );
    // Ours reaches the newer revision → keep ours.
    expect(isStrictlyNewer(facts("1.0.0", ["2025-03-26"]), facts("1.0.0", ["2025-06-18"]))).toBe(
      false,
    );
    // Same protocol on both → a tie keeps ours.
    expect(isStrictlyNewer(facts("1.0.0", ["2025-06-18"]), facts("1.0.0", ["2025-06-18"]))).toBe(
      false,
    );
  });

  it("keeps ours on any ambiguity — a missing probe or an unreadable version", () => {
    expect(isStrictlyNewer(facts("1.0.0", null), facts("1.0.0", ["2025-06-18"]))).toBe(false);
    expect(isStrictlyNewer(facts("1.0.0", ["2025-06-18"]), facts("1.0.0", null))).toBe(false);
    expect(isStrictlyNewer(facts("rolling"), facts("1.0.0"))).toBe(false);
  });

  it("cannot call a candidate strictly newer than a version-less current — that is the guard's job", () => {
    // This pure comparison never says "newer" over a null on either side; a version-less CURRENT
    // being freely superseded is a decision the accept guard makes on the null itself, not here.
    expect(isStrictlyNewer(facts("2.0.0"), facts(null))).toBe(false);
    expect(isStrictlyNewer(facts(null), facts("1.0.0"))).toBe(false);
    expect(isStrictlyNewer(facts(null), facts(null))).toBe(false);
  });

  it("a malformed candidate label never supersedes a real release", () => {
    expect(isStrictlyNewer(facts("2.0.0-"), facts("1.0.0"))).toBe(false);
    expect(isStrictlyNewer(facts("1.0.0-rc."), facts("1.0.0"))).toBe(false);
  });
});

describe("which sources a person authored", () => {
  it("a staff correction and an owner's edit are protected; upstream and a seed are not", () => {
    expect(isStaffAuthoredSource("staff")).toBe(true);
    expect(isStaffAuthoredSource("owner")).toBe(true);
    expect(isStaffAuthoredSource("registry")).toBe(false);
    // A seed is a starting point the catalog stood up, not a decision a person made — so it is not
    // provenance-protected. It is instead freely superseded because it carries no version (the
    // accept guard's general "null version" rule), never by naming the source.
    expect(isStaffAuthoredSource("seed")).toBe(false);
  });
});
