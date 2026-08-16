import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * Invite-mail HARDENING: every user-influenced value is made mail-safe at COMPOSE time — our
 * own control-character strip (never a reliance on the mail library's internal header
 * folding) plus a display-length cut — and the transport wears a second subject strip as a
 * belt. The load-bearing pin: a `\r\n` smuggled through a workspace name can NEVER reach the
 * subject line (header injection), and a 10k-character name cannot balloon one.
 */

const { sendMailSpy, createTransportSpy } = vi.hoisted(() => {
  const sendMailSpy = vi.fn(async (_message: unknown) => ({}));
  const createTransportSpy = vi.fn(() => ({ sendMail: sendMailSpy }));
  return { sendMailSpy, createTransportSpy };
});
vi.mock("nodemailer", () => ({ default: { createTransport: createTransportSpy } }));
vi.mock("@/lib/db/mail-log.server", () => ({ recordMailEvent: vi.fn(async () => {}) }));

const BASE_ENV: Record<string, string> = {
  DATABASE_URL: "postgres://user:pass@localhost:5439/db",
  BETTER_AUTH_SECRET: "0123456789abcdef0123456789abcdef",
  BETTER_AUTH_URL: "http://localhost:3000",
  PLANE_INTERNAL_URL: "http://vault.internal:8080",
  PLANE_INTERNAL_TOKEN: "internal-token-value",
  TOPOS_MAIL_SMTP_HOST: "smtp.example.com",
  TOPOS_MAIL_SMTP_PORT: "465",
  TOPOS_MAIL_SMTP_USER: "api_token",
  TOPOS_MAIL_SMTP_PASS: "relay-secret",
  TOPOS_MAIL_SMTP_FROM: "Topos <no-reply@example.com>",
};

async function importArmedProduction() {
  vi.resetModules();
  for (const [k, v] of Object.entries(BASE_ENV)) {
    vi.stubEnv(k, v);
  }
  vi.stubEnv("APP_ENV", "production");
  return {
    inviteMail: await import("@/lib/mail/invite-mail.server"),
    transport: await import("@/lib/mail/transport.server"),
  };
}

afterEach(() => {
  vi.unstubAllEnvs();
  sendMailSpy.mockClear();
});

type SentMessage = { subject: string; text: string; html?: string };

describe("compose-time strip + cut", () => {
  it("a CRLF-smuggling workspace name never reaches the subject (or any line raw)", async () => {
    const { inviteMail } = await importArmedProduction();
    await inviteMail.sendInviteEmail({
      to: "victim@example.com",
      workspaceDisplayName: "Acme\r\nBcc: everyone@example.com\r\nX-Evil: 1",
      inviteUrl: "https://topos.example/acme/invite/tok-123",
      agentUrl: "https://topos.example/agent",
      invitedBy: "owner\r\nReply-To: attacker@evil.example",
    });
    expect(sendMailSpy).toHaveBeenCalledTimes(1);
    const sent = sendMailSpy.mock.calls[0]?.[0] as SentMessage;
    expect(sent.subject).not.toMatch(/[\r\n]/);
    expect(sent.subject).toContain("Acme Bcc: everyone@example.com");
    expect(sent.text).not.toContain("owner\r\nReply-To");
  });

  it("display fields are cut to 60 chars with an ellipsis; the hint name too", async () => {
    const { inviteMail } = await importArmedProduction();
    const longName = "w".repeat(300);
    await inviteMail.sendInviteEmail({
      to: "victim@example.com",
      workspaceDisplayName: longName,
      inviteUrl: "https://topos.example/acme/invite/tok-123",
      agentUrl: "https://topos.example/agent",
      invitedBy: "o".repeat(300),
      hint: { kind: "skill", name: "h".repeat(300) },
    });
    const sent = sendMailSpy.mock.calls[0]?.[0] as SentMessage;
    expect(sent.subject).toContain(`${"w".repeat(59)}…`);
    expect(sent.subject).not.toContain("w".repeat(61));
    expect(sent.text).toContain(`${"o".repeat(59)}…`);
    expect(sent.subject).toContain(`${"h".repeat(59)}…`);
  });
});

describe("the transport-level subject belt", () => {
  it("sendMail strips control characters from ANY caller's subject", async () => {
    const { transport } = await importArmedProduction();
    await transport.sendMail({
      kind: "invite",
      to: "victim@example.com",
      subject: "Hello\r\nBcc: everyone@example.com",
      text: "body",
    });
    const sent = sendMailSpy.mock.calls[0]?.[0] as SentMessage;
    expect(sent.subject).toBe("Hello Bcc: everyone@example.com");
  });

  it("stripMailControl collapses control runs to one space", async () => {
    const { transport } = await importArmedProduction();
    expect(transport.stripMailControl("a\r\n\tb\x00c")).toBe("a b c");
    expect(transport.stripMailControl("plain subject")).toBe("plain subject");
  });
});
