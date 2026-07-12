import { describe, expect, it } from "vitest";
import {
  actorFromSession,
  normalizeEmail,
  resolveAdmission,
  type SessionData,
  safeNextPath,
} from "@/lib/auth/guards.server";

// Pure parts only — DB-free by design. The session-dependent paths (requireSession redirect,
// requireMember/requireWorkspaceOwner/requireReviewer 404s) exercise a live Better Auth instance
// and the request headers, and are covered by the e2e suite; the admission DECISION they all feed
// into is the exported pure resolveAdmission, truth-tabled below.

function session(email: string, emailVerified = true): SessionData {
  return { user: { email, emailVerified } } as unknown as SessionData;
}

describe("normalizeEmail", () => {
  it("lowercases and trims", () => {
    expect(normalizeEmail("  Alice@Example.COM ")).toBe("alice@example.com");
  });

  it("is a no-op on an already-normal email", () => {
    expect(normalizeEmail("bob@example.com")).toBe("bob@example.com");
  });
});

describe("actorFromSession", () => {
  it("mints a normalized actor from a verified session", () => {
    expect(actorFromSession(session("  Alice@Example.COM "))).toEqual({
      email: "alice@example.com",
    });
  });

  it("refuses an unverified email and an absent session", () => {
    expect(actorFromSession(session("alice@example.com", false))).toBeNull();
    expect(actorFromSession(null)).toBeNull();
    expect(actorFromSession(undefined)).toBeNull();
  });

  it("rejects the U+212A Kelvin lookalike on the RAW email — before normalize would fold it", () => {
    // JS toLowerCase() folds KELVIN SIGN to ASCII 'k': normalizing first would let a verified
    // Unicode-lookalike address false-match a real ASCII directory seat. The gate must run on the
    // raw bytes, so the lookalike never becomes an actor at all.
    const kelvin = "\u212AK@example.com"; // U+212A KELVIN SIGN + ASCII K
    expect(kelvin.toLowerCase()).toBe("kk@example.com");
    expect(actorFromSession(session(kelvin))).toBeNull();
  });

  it("rejects any non-printable-ASCII email (directory principals are ASCII-canonical by CHECK)", () => {
    expect(actorFromSession(session("café@example.com"))).toBeNull();
    expect(actorFromSession(session("замо́к@example.com"))).toBeNull();
    // Control characters sit outside the printable range too.
    expect(actorFromSession(session("a\tb@example.com"))).toBeNull();
    expect(actorFromSession(session(""))).toBeNull();
  });
});

describe("resolveAdmission (the full truth table)", () => {
  it("confirmed seat → roster with the directory's role", () => {
    expect(resolveAdmission({ role: "owner", status: "confirmed" })).toEqual({
      kind: "roster",
      role: "owner",
    });
    expect(resolveAdmission({ role: "reviewer", status: "confirmed" })).toEqual({
      kind: "roster",
      role: "reviewer",
    });
    expect(resolveAdmission({ role: "member", status: "confirmed" })).toEqual({
      kind: "roster",
      role: "member",
    });
  });

  it("invited seat → miss (an invite promises index visibility, never admission)", () => {
    expect(resolveAdmission({ role: "member", status: "invited" })).toEqual({
      kind: "miss",
    });
    // Even an invited OWNER seat admits nothing before the enrollment redeem gate ran.
    expect(resolveAdmission({ role: "owner", status: "invited" })).toEqual({
      kind: "miss",
    });
  });

  it("no seat → miss", () => {
    expect(resolveAdmission(undefined)).toEqual({ kind: "miss" });
  });
});

describe("safeNextPath", () => {
  it("accepts a legit same-app relative path", () => {
    expect(safeNextPath("/verify/ABC")).toBe("/verify/ABC");
    expect(safeNextPath("/")).toBe("/");
  });

  it("falls back to the dashboard when next is absent", () => {
    expect(safeNextPath(undefined)).toBe("/workspaces");
    expect(safeNextPath("")).toBe("/workspaces");
  });

  it("rejects absolute URLs and protocol-relative //host", () => {
    expect(safeNextPath("https://evil.com")).toBe("/workspaces");
    expect(safeNextPath("//evil.com")).toBe("/workspaces");
    expect(safeNextPath("//evil")).toBe("/workspaces");
  });

  it("rejects the backslash smuggle (WHATWG treats \\ as /)", () => {
    expect(safeNextPath("/\\evil.com")).toBe("/workspaces");
    expect(safeNextPath("/\\evil")).toBe("/workspaces");
    expect(safeNextPath("\\/evil")).toBe("/workspaces");
  });

  it("rejects percent-escapes a redirect layer could decode off-origin", () => {
    expect(safeNextPath("/%5Cevil.com")).toBe("/workspaces");
    expect(safeNextPath("/%5Cevil")).toBe("/workspaces");
    expect(safeNextPath("/%2F%5Cevil.com")).toBe("/workspaces");
  });
});
