import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * The invitation-mail seam. Outside production the notice lands in `.invite-emails.jsonl` — its
 * OWN file, NEVER `.magic-links.jsonl` (whose reader parses every line of its file and would hand
 * a sign-in flow the wrong thing). The OSS default has NO outbound transport: production is a
 * deliberate no-op and `inviteMailDelivery().canSend` is always false. The notice carries the
 * workspace ADDRESS + display name — a plain slug, never a tokened link.
 */

const BASE_ENV: Record<string, string> = {
  DATABASE_URL: "postgres://user:pass@localhost:5439/db",
  BETTER_AUTH_SECRET: "0123456789abcdef0123456789abcdef",
  BETTER_AUTH_URL: "http://localhost:3000",
  PLANE_INTERNAL_URL: "http://vault.internal:8080",
  PLANE_INTERNAL_TOKEN: "internal-token-value",
};

async function importInviteMail(appEnv: string) {
  vi.resetModules();
  for (const [k, v] of Object.entries(BASE_ENV)) {
    vi.stubEnv(k, v);
  }
  vi.stubEnv("APP_ENV", appEnv);
  return await import("@/lib/mail/invite-mail.server");
}

const INVITE = {
  to: "newbie@example.com",
  workspaceDisplayName: "Acme Platform",
  address: "acme-platform",
  invitedBy: "owner@example.com",
};

afterEach(() => {
  vi.unstubAllEnvs();
  vi.unstubAllGlobals();
});

describe("inviteMailDelivery", () => {
  it("reports no real delivery in every environment (the OSS default)", async () => {
    for (const env of ["test", "development", "production"]) {
      const mail = await importInviteMail(env);
      expect(mail.inviteMailDelivery()).toEqual({ canSend: false });
    }
  });
});

describe("sendInviteEmail in test mode", () => {
  it("appends {to,address,...} to .invite-emails.jsonl — never .magic-links.jsonl, never a send", async () => {
    const mail = await importInviteMail("test");
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    const dir = await fs.mkdtemp(path.join(os.tmpdir(), "topos-invite-mail-"));
    const previousCwd = process.cwd();
    process.chdir(dir);
    try {
      await mail.sendInviteEmail(INVITE);
      const lines = (await fs.readFile(path.join(dir, ".invite-emails.jsonl"), "utf8"))
        .trim()
        .split("\n");
      expect(lines).toHaveLength(1);
      const parsed = JSON.parse(lines[0] as string);
      expect(parsed).toMatchObject({
        to: INVITE.to,
        address: INVITE.address,
        workspaceDisplayName: INVITE.workspaceDisplayName,
      });
      // The address is a plain slug — no tokened link machinery of any kind.
      expect(lines[0]).not.toContain("/i/");
      expect(lines[0]).not.toContain("token");
      await expect(fs.access(path.join(dir, ".magic-links.jsonl"))).rejects.toThrow();
      expect(fetchSpy).not.toHaveBeenCalled();
    } finally {
      process.chdir(previousCwd);
      await fs.rm(dir, { recursive: true, force: true });
    }
  });
});

describe("sendInviteEmail in production mode", () => {
  it("is a no-op in the OSS default: writes nothing, sends nothing", async () => {
    const mail = await importInviteMail("production");
    const fetchSpy = vi.fn();
    vi.stubGlobal("fetch", fetchSpy);
    const dir = await fs.mkdtemp(path.join(os.tmpdir(), "topos-invite-mail-"));
    const previousCwd = process.cwd();
    process.chdir(dir);
    try {
      await mail.sendInviteEmail(INVITE);
      // No file is written (the seat + address already stand) and no transport is called.
      await expect(fs.access(path.join(dir, ".invite-emails.jsonl"))).rejects.toThrow();
      expect(fetchSpy).not.toHaveBeenCalled();
      expect(mail.inviteMailDelivery().canSend).toBe(false);
    } finally {
      process.chdir(previousCwd);
      await fs.rm(dir, { recursive: true, force: true });
    }
  });
});
