import { describe, expect, it } from "vitest";
import {
  actorFromSession,
  resolveAdmission,
  type SessionData,
  safeNextPath,
} from "@/lib/auth/guards.server";

// Pure parts only — DB-free by design. The session-dependent paths (requireSession redirect,
// requireMember/requireWorkspaceOwner/requireReviewer 404s, the device lane's requireDeviceActor)
// exercise a live Better Auth instance / a real credential resolve and are covered by the DB-backed
// suites; the admission DECISION they all feed into is the exported pure resolveAdmission,
// truth-tabled below. There is no email gate here anymore: one identity is `user.id`, email is a
// login name and a display attribute — nothing authorizes by email equality, so no normalization
// or lookalike defense exists to test.

function session(user: { id: string; name: string; email?: string }): SessionData {
  return { user } as unknown as SessionData;
}

describe("actorFromSession", () => {
  it("mints an actor from the session's user id, with the name as the display snapshot", () => {
    expect(actorFromSession(session({ id: "u_1", name: "Ada", email: "ada@example.com" }))).toEqual(
      { userId: "u_1", display: "Ada" },
    );
  });

  it("falls back to the email as a readable display when the name is blank", () => {
    expect(actorFromSession(session({ id: "u_2", name: "", email: "bo@example.com" }))).toEqual({
      userId: "u_2",
      display: "bo@example.com",
    });
    // Whitespace-only is blank too.
    expect(actorFromSession(session({ id: "u_2", name: "   ", email: "bo@example.com" }))).toEqual({
      userId: "u_2",
      display: "bo@example.com",
    });
  });

  it("degrades to 'unknown' when neither name nor email exists — never a null display", () => {
    expect(actorFromSession(session({ id: "u_3", name: "" }))).toEqual({
      userId: "u_3",
      display: "unknown",
    });
  });

  it("returns null on no session and on a session without a user id", () => {
    expect(actorFromSession(null)).toBeNull();
    expect(actorFromSession(undefined)).toBeNull();
    expect(actorFromSession(session({ id: "", name: "Ghost" }))).toBeNull();
  });
});

describe("resolveAdmission (the full truth table)", () => {
  it("a seat admits with the seat's role", () => {
    expect(resolveAdmission({ role: "owner" })).toEqual({ kind: "seat", role: "owner" });
    expect(resolveAdmission({ role: "reviewer" })).toEqual({ kind: "seat", role: "reviewer" });
    expect(resolveAdmission({ role: "member" })).toEqual({ kind: "seat", role: "member" });
  });

  it("no seat → miss (invitations are claims on FUTURE users in their own table — holding one admits nothing)", () => {
    expect(resolveAdmission(undefined)).toEqual({ kind: "miss" });
  });
});

describe("safeNextPath", () => {
  it("accepts a legit same-app relative path", () => {
    expect(safeNextPath("/verify?code=ABCD-EFGH")).toBe("/verify?code=ABCD-EFGH");
    expect(safeNextPath("/")).toBe("/");
  });

  it("falls back to the dashboard when next is absent", () => {
    expect(safeNextPath(undefined)).toBe("/app");
    expect(safeNextPath("")).toBe("/app");
  });

  it("rejects absolute URLs and protocol-relative //host", () => {
    expect(safeNextPath("https://evil.com")).toBe("/app");
    expect(safeNextPath("//evil.com")).toBe("/app");
    expect(safeNextPath("//evil")).toBe("/app");
  });

  it("rejects the backslash smuggle (WHATWG treats \\ as /)", () => {
    expect(safeNextPath("/\\evil.com")).toBe("/app");
    expect(safeNextPath("/\\evil")).toBe("/app");
    expect(safeNextPath("\\/evil")).toBe("/app");
  });

  it("rejects percent-escapes a redirect layer could decode off-origin", () => {
    expect(safeNextPath("/%5Cevil.com")).toBe("/app");
    expect(safeNextPath("/%5Cevil")).toBe("/app");
    expect(safeNextPath("/%2F%5Cevil.com")).toBe("/app");
  });

  it("rejects ASCII control characters (WHATWG URL parsing strips them before parsing)", () => {
    expect(safeNextPath("/\t//evil.com")).toBe("/app");
    expect(safeNextPath("/\n//evil.com")).toBe("/app");
    expect(safeNextPath("/a\x00b")).toBe("/app");
    expect(safeNextPath("/a\x7fb")).toBe("/app");
  });
});
