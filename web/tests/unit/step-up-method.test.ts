import { beforeAll, describe, expect, it, vi } from "vitest";
import { installTestEnv } from "./helpers/test-env";

/**
 * `stepUpMethod` picks the re-authentication rung for a session user: a password account
 * re-enters its password; a password-less account (magic-link/social) confirms by mail when the
 * transport is armed; with neither, there is no rung. The two inputs — whether a credential
 * password exists, and whether mail is armed — are stubbed via importActual overrides so all three
 * outcomes are exercised without a database.
 */

let hasPassword = false;
let canSend = false;

vi.mock("@/lib/auth/server", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/auth/server")>();
  return { ...actual, hasCredentialPassword: async () => hasPassword };
});

vi.mock("@/lib/mail/transport.server", async (importOriginal) => {
  const actual = await importOriginal<typeof import("@/lib/mail/transport.server")>();
  return { ...actual, mailDelivery: () => ({ canSend }) };
});

let stepUpMethod: typeof import("@/lib/auth/step-up.server").stepUpMethod;

beforeAll(async () => {
  installTestEnv();
  ({ stepUpMethod } = await import("@/lib/auth/step-up.server"));
});

describe("stepUpMethod", () => {
  it("an account WITH a password → the password rung (mail state irrelevant)", async () => {
    hasPassword = true;
    canSend = false;
    expect(await stepUpMethod("u_pw")).toBe("password");
    canSend = true;
    expect(await stepUpMethod("u_pw")).toBe("password");
  });

  it("a password-LESS account with armed mail → the email rung", async () => {
    hasPassword = false;
    canSend = true;
    expect(await stepUpMethod("u_magic")).toBe("email");
  });

  it("a password-less account with NO armed mail → unavailable (no silent dead end)", async () => {
    hasPassword = false;
    canSend = false;
    expect(await stepUpMethod("u_stuck")).toBe("unavailable");
  });
});
