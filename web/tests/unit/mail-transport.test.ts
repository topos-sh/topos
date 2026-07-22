import { afterEach, describe, expect, it, vi } from "vitest";

/**
 * The ONE outbound-mail transport. BRING YOUR OWN SMTP: the five `TOPOS_MAIL_SMTP_*` variables
 * arm it all-or-nothing (the vault's old five-flag rule, moved app-side with the mail
 * unification). A send failure is COARSE — never the message, the recipient, or the relay's
 * response (a body can carry a live credential).
 */

const { sendMailSpy, createTransportSpy } = vi.hoisted(() => {
  const sendMailSpy = vi.fn(async () => ({}));
  const createTransportSpy = vi.fn(() => ({ sendMail: sendMailSpy }));
  return { sendMailSpy, createTransportSpy };
});
vi.mock("nodemailer", () => ({ default: { createTransport: createTransportSpy } }));

// The metadata-only send log — mocked to a spy so this suite stays DB-free; the row shape and
// the real insert are pinned by the scratch-DB mail-log suite.
const { recordMailEventSpy } = vi.hoisted(() => ({
  recordMailEventSpy: vi.fn(async () => {}),
}));
vi.mock("@/lib/db/mail-log.server", () => ({ recordMailEvent: recordMailEventSpy }));

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

async function importTransport(smtp: Record<string, string>) {
  vi.resetModules();
  // Unstub first — the five-or-none loop imports repeatedly, and a stale stub from a previous
  // iteration would silently re-arm a variable the case meant to omit.
  vi.unstubAllEnvs();
  for (const [k, v] of Object.entries({ ...BASE_ENV, ...smtp })) {
    vi.stubEnv(k, v);
  }
  return await import("@/lib/mail/transport.server");
}

afterEach(() => {
  vi.unstubAllEnvs();
  sendMailSpy.mockClear();
  createTransportSpy.mockClear();
  recordMailEventSpy.mockClear();
});

describe("mailDelivery", () => {
  it("is armed only when ALL FIVE variables are set (the five-or-none rule)", async () => {
    const armed = await importTransport(SMTP_ENV);
    expect(armed.mailDelivery()).toEqual({ canSend: true });
    for (const missing of Object.keys(SMTP_ENV)) {
      const partial = { ...SMTP_ENV };
      delete partial[missing];
      const mail = await importTransport(partial);
      expect(mail.mailDelivery(), `without ${missing}`).toEqual({ canSend: false });
    }
  });

  it("treats an EMPTY value as unset (how compose and deploy panels spell it)", async () => {
    const mail = await importTransport({ ...SMTP_ENV, TOPOS_MAIL_SMTP_PASS: "" });
    expect(mail.mailDelivery()).toEqual({ canSend: false });
  });
});

describe("sendMail", () => {
  it("refuses coarse when unconfigured — no transport is ever half-built", async () => {
    const mail = await importTransport({});
    await expect(
      mail.sendMail({ kind: "invite", to: "a@b.c", subject: "s", text: "t" }),
    ).rejects.toThrow("mail transport is not configured");
    expect(createTransportSpy).not.toHaveBeenCalled();
    // Even the no-transport refusal is a logged attempt — metadata only.
    expect(recordMailEventSpy).toHaveBeenCalledWith("invite", "a@b.c", {
      outcome: "failed",
      code: "unconfigured",
    });
  });

  it("sends through the configured relay (465 ⇒ implicit TLS) with the configured from", async () => {
    const mail = await importTransport(SMTP_ENV);
    await mail.sendMail({
      kind: "auth-verify",
      to: "person@example.com",
      subject: "Hello",
      text: "Body",
    });
    expect(createTransportSpy).toHaveBeenCalledWith({
      host: "smtp.example.com",
      port: 465,
      secure: true,
      auth: { user: "api_token", pass: "relay-secret" },
    });
    expect(sendMailSpy).toHaveBeenCalledWith({
      from: "Topos <no-reply@example.com>",
      to: "person@example.com",
      subject: "Hello",
      text: "Body",
    });
    // The landed send logs ONE ok attempt (kind + recipient, nothing of the message).
    expect(recordMailEventSpy).toHaveBeenCalledTimes(1);
    expect(recordMailEventSpy).toHaveBeenCalledWith("auth-verify", "person@example.com", {
      outcome: "ok",
    });
  });

  it("maps a relay failure to ONE coarse error — never the recipient or the body", async () => {
    const mail = await importTransport(SMTP_ENV);
    sendMailSpy.mockRejectedValueOnce(
      new Error("550 relay says: person@example.com rejected, body preview: secret-code-123"),
    );
    const failed = mail.sendMail({
      kind: "magic-link",
      to: "person@example.com",
      subject: "s",
      text: "secret-code-123",
    });
    await expect(failed).rejects.toThrow("mail send failed");
    await failed.catch((error: Error) => {
      expect(error.message).not.toContain("person@example.com");
      expect(error.message).not.toContain("secret-code-123");
    });
    // The refused relay logs ONE failed attempt with the coarse code — never the relay's text.
    expect(recordMailEventSpy).toHaveBeenCalledTimes(1);
    expect(recordMailEventSpy).toHaveBeenCalledWith("magic-link", "person@example.com", {
      outcome: "failed",
      code: "send_failed",
    });
  });
});

describe("escapeHtml", () => {
  it("escapes every HTML-active character in a user-entered value", async () => {
    const mail = await importTransport({});
    expect(mail.escapeHtml(`<img src="x" onerror='a&b'>`)).toBe(
      "&lt;img src=&quot;x&quot; onerror=&#39;a&amp;b&#39;&gt;",
    );
  });
});
