import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * Passcode delivery — the send half of the enrollment second factor (the vault only MINTS since
 * the mail unification). The code is a live credential: outside production it lands in its OWN
 * `.passcode-emails.jsonl` (never the magic-link file); production without a transport drops it
 * SILENTLY (the old vault no-op posture — the public ack stays constant-shaped); production with
 * SMTP sends the old vault mail body, text-only.
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

const SMTP_ENV: Record<string, string> = {
  TOPOS_MAIL_SMTP_HOST: "smtp.example.com",
  TOPOS_MAIL_SMTP_PORT: "465",
  TOPOS_MAIL_SMTP_USER: "api_token",
  TOPOS_MAIL_SMTP_PASS: "relay-secret",
  TOPOS_MAIL_SMTP_FROM: "Topos <no-reply@example.com>",
};

async function importPasscodeMail(appEnv: string, smtp: Record<string, string> = {}) {
  vi.resetModules();
  for (const [k, v] of Object.entries({ ...BASE_ENV, ...smtp })) {
    vi.stubEnv(k, v);
  }
  vi.stubEnv("APP_ENV", appEnv);
  return await import("@/lib/mail/passcode-mail.server");
}

const INPUT = {
  to: "alice@example.com",
  code: "424242",
  workspaceDisplayName: "Acme",
  verifyBaseUrl: "https://topos.example",
};

afterEach(() => {
  vi.unstubAllEnvs();
  sendMailSpy.mockClear();
  createTransportSpy.mockClear();
});

describe("sendPasscodeEmail in test mode", () => {
  it("records {to,code,...} to .passcode-emails.jsonl — never the magic-link file, never a send", async () => {
    const mail = await importPasscodeMail("test", SMTP_ENV);
    const dir = await fs.mkdtemp(path.join(os.tmpdir(), "topos-passcode-mail-"));
    const previousCwd = process.cwd();
    process.chdir(dir);
    try {
      await mail.sendPasscodeEmail(INPUT);
      const lines = (await fs.readFile(path.join(dir, ".passcode-emails.jsonl"), "utf8"))
        .trim()
        .split("\n");
      expect(lines).toHaveLength(1);
      expect(JSON.parse(lines[0] as string)).toEqual({
        to: INPUT.to,
        code: INPUT.code,
        workspaceDisplayName: INPUT.workspaceDisplayName,
      });
      // The accumulating dev outbox carries the FULL rendered mail, kind-tagged.
      const outbox = (await fs.readFile(path.join(dir, ".outbox.jsonl"), "utf8"))
        .trim()
        .split("\n");
      expect(outbox).toHaveLength(1);
      const recorded = JSON.parse(outbox[0] as string);
      expect(recorded.kind).toBe("passcode");
      expect(recorded.to).toBe(INPUT.to);
      expect(recorded.subject).toBe("Your Topos verification code");
      expect(recorded.text).toContain("Your Topos verification code for Acme is 424242.");
      expect(recorded.text).toContain("https://topos.example/verify");
      await expect(fs.access(path.join(dir, ".magic-links.jsonl"))).rejects.toThrow();
      expect(sendMailSpy).not.toHaveBeenCalled();
    } finally {
      process.chdir(previousCwd);
      await fs.rm(dir, { recursive: true, force: true });
    }
  });
});

describe("sendPasscodeEmail in production mode", () => {
  it("drops SILENTLY without a transport (the vault's old no-op posture — the ack stays constant)", async () => {
    const mail = await importPasscodeMail("production");
    const dir = await fs.mkdtemp(path.join(os.tmpdir(), "topos-passcode-mail-"));
    const previousCwd = process.cwd();
    process.chdir(dir);
    try {
      await expect(mail.sendPasscodeEmail(INPUT)).resolves.toBeUndefined();
      await expect(fs.access(path.join(dir, ".passcode-emails.jsonl"))).rejects.toThrow();
      await expect(fs.access(path.join(dir, ".outbox.jsonl"))).rejects.toThrow();
      expect(sendMailSpy).not.toHaveBeenCalled();
    } finally {
      process.chdir(previousCwd);
      await fs.rm(dir, { recursive: true, force: true });
    }
  });

  it("sends the old vault mail body, TEXT-ONLY, through the armed transport", async () => {
    const mail = await importPasscodeMail("production", SMTP_ENV);
    await mail.sendPasscodeEmail(INPUT);
    expect(sendMailSpy).toHaveBeenCalledTimes(1);
    const message = sendMailSpy.mock.calls[0]?.[0] as {
      to: string;
      subject: string;
      text: string;
      html?: string;
    };
    expect(message.to).toBe("alice@example.com");
    expect(message.subject).toBe("Your Topos verification code");
    expect(message.text).toContain("Your Topos verification code for Acme is 424242.");
    expect(message.text).toContain("https://topos.example/verify");
    expect(message.html).toBeUndefined();
  });
});
