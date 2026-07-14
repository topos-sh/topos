import fs from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * Magic-link delivery — the sender a composition wires into the auth seam (the OSS default never
 * registers the rung). The URL is a LIVE credential: outside production it lands in
 * `.magic-links.jsonl` (its own file, whose reader hands sign-in flows their link); production
 * sends through the ONE transport or throws COARSE (better-auth surfaces the failure — a silently
 * dropped link would read as a broken sign-in).
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

async function importMagicLinkMail(appEnv: string, smtp: Record<string, string> = {}) {
  vi.resetModules();
  for (const [k, v] of Object.entries({ ...BASE_ENV, ...smtp })) {
    vi.stubEnv(k, v);
  }
  vi.stubEnv("APP_ENV", appEnv);
  return await import("@/lib/mail/magic-link-mail.server");
}

const ARGS = { email: "alice@example.com", url: "https://topos.example/magic?token=abc" };

afterEach(() => {
  vi.unstubAllEnvs();
  sendMailSpy.mockClear();
  createTransportSpy.mockClear();
});

describe("sendMagicLinkEmail in test mode", () => {
  it("records {email,url} to .magic-links.jsonl — its OWN file, never a send", async () => {
    const mail = await importMagicLinkMail("test", SMTP_ENV);
    const dir = await fs.mkdtemp(path.join(os.tmpdir(), "topos-magic-link-mail-"));
    const previousCwd = process.cwd();
    process.chdir(dir);
    try {
      await mail.sendMagicLinkEmail(ARGS);
      const lines = (await fs.readFile(path.join(dir, ".magic-links.jsonl"), "utf8"))
        .trim()
        .split("\n");
      expect(lines).toHaveLength(1);
      expect(JSON.parse(lines[0] as string)).toEqual(ARGS);
      // The accumulating dev outbox carries the FULL rendered mail, kind-tagged.
      const outbox = (await fs.readFile(path.join(dir, ".outbox.jsonl"), "utf8"))
        .trim()
        .split("\n");
      expect(outbox).toHaveLength(1);
      const recorded = JSON.parse(outbox[0] as string);
      expect(recorded.kind).toBe("magic-link");
      expect(recorded.to).toBe(ARGS.email);
      expect(recorded.subject).toBe("Sign in to Topos");
      expect(recorded.text).toContain(ARGS.url);
      expect(recorded.html).toContain('href="https://topos.example/magic?token=abc"');
      await expect(fs.access(path.join(dir, ".invite-emails.jsonl"))).rejects.toThrow();
      expect(sendMailSpy).not.toHaveBeenCalled();
    } finally {
      process.chdir(previousCwd);
      await fs.rm(dir, { recursive: true, force: true });
    }
  });
});

describe("sendMagicLinkEmail in production mode", () => {
  it("sends the sign-in mail through the armed transport, link in text and HTML href", async () => {
    const mail = await importMagicLinkMail("production", SMTP_ENV);
    await mail.sendMagicLinkEmail(ARGS);
    expect(sendMailSpy).toHaveBeenCalledTimes(1);
    const message = sendMailSpy.mock.calls[0]?.[0] as {
      to: string;
      subject: string;
      text: string;
      html?: string;
    };
    expect(message.to).toBe("alice@example.com");
    expect(message.subject).toBe("Sign in to Topos");
    expect(message.text).toContain(ARGS.url);
    expect(message.html).toContain('href="https://topos.example/magic?token=abc"');
  });

  it("throws COARSE without a transport — a dropped link must surface, never silently vanish", async () => {
    const mail = await importMagicLinkMail("production");
    await expect(mail.sendMagicLinkEmail(ARGS)).rejects.toThrow("mail transport is not configured");
  });
});
