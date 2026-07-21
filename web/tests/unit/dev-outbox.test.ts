import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * The accumulating dev outbox — outside production EVERY product mail lands in ONE
 * `.outbox.jsonl` as the full rendered message, kind-tagged, so a human tester watches a single
 * file. The per-flow credential files keep their own contracts (their own suites); this suite
 * pins the superset view: the four mail kinds — magic-link, invite, and the two auth rungs
 * (verification + reset) — one file, four lines, in send order.
 */

const { sendMailSpy, createTransportSpy } = vi.hoisted(() => {
  const sendMailSpy = vi.fn(async (_message: unknown) => ({}));
  const createTransportSpy = vi.fn(() => ({ sendMail: sendMailSpy }));
  return { sendMailSpy, createTransportSpy };
});
vi.mock("nodemailer", () => ({ default: { createTransport: createTransportSpy } }));

const BASE_ENV: Record<string, string> = {
  DATABASE_URL: "postgres://user:pass@localhost:5439/db",
  BETTER_AUTH_SECRET: "0123456789abcdef0123456789abcdef",
  BETTER_AUTH_URL: "http://localhost:3000",
  PLANE_INTERNAL_URL: "http://vault.internal:8080",
  PLANE_INTERNAL_TOKEN: "internal-token-value",
};

afterEach(() => {
  vi.unstubAllEnvs();
  sendMailSpy.mockClear();
  createTransportSpy.mockClear();
});

describe("the dev outbox accumulates across mail kinds", () => {
  it("collects magic-link, invite, verification, and reset mails as kind-tagged lines in ONE file", async () => {
    vi.resetModules();
    for (const [k, v] of Object.entries(BASE_ENV)) {
      vi.stubEnv(k, v);
    }
    vi.stubEnv("APP_ENV", "test");
    const magicLink = await import("@/lib/mail/magic-link-mail.server");
    const invite = await import("@/lib/mail/invite-mail.server");
    const authMail = await import("@/lib/mail/auth-mail.server");
    const dir = await fs.mkdtemp(path.join(os.tmpdir(), "topos-dev-outbox-"));
    const previousCwd = process.cwd();
    process.chdir(dir);
    try {
      await magicLink.sendMagicLinkEmail({
        email: "alice@example.com",
        url: "https://topos.example/magic?token=abc",
      });
      await invite.sendInviteEmail({
        to: "carol@example.com",
        workspaceDisplayName: "Acme",
        inviteUrl: "https://topos.example/invite/tok-abc",
        agentUrl: "https://topos.example/agent",
        invitedBy: "owner@example.com",
      });
      await authMail.sendVerificationMail(
        "dana@example.com",
        "https://topos.example/verify-email?token=def",
      );
      await authMail.sendResetMail("erin@example.com", "https://topos.example/reset?token=ghi");
      const lines = (await fs.readFile(path.join(dir, ".outbox.jsonl"), "utf8")).trim().split("\n");
      expect(lines).toHaveLength(4);
      const mails = lines.map((line) => JSON.parse(line));
      expect(mails.map((m) => m.kind)).toEqual([
        "magic-link",
        "invite",
        "auth-verify",
        "auth-reset",
      ]);
      expect(mails.map((m) => m.to)).toEqual([
        "alice@example.com",
        "carol@example.com",
        "dana@example.com",
        "erin@example.com",
      ]);
      for (const mail of mails) {
        // Every line is the full rendered message a transport would have been handed.
        expect(typeof mail.at).toBe("string");
        expect(typeof mail.subject).toBe("string");
        expect(mail.subject).not.toBe("");
        expect(typeof mail.text).toBe("string");
        expect(mail.text).not.toBe("");
      }
      // SMTP is unarmed (no TOPOS_MAIL_SMTP_*): nothing ever reached a real transport.
      expect(sendMailSpy).not.toHaveBeenCalled();
    } finally {
      process.chdir(previousCwd);
      await fs.rm(dir, { recursive: true, force: true });
    }
  });
});
