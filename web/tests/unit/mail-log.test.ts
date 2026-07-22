import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import { createScratchDb, type ScratchDb } from "./helpers/scratch-db";

/**
 * The metadata-only mail send log against a REAL scratch Postgres: every attempt through the
 * ONE transport lands one `web.mail_event` row — ok on a landed send, failed + a coarse code on
 * a refused one — and the table CANNOT carry a message even by mistake: there is no subject or
 * body column at all (a mail body can carry a live credential; the transport's coarse-failure
 * posture extends to its log by construction). The relay itself is mocked; the rows are real.
 */

const { sendMailSpy, createTransportSpy } = vi.hoisted(() => {
  const sendMailSpy = vi.fn(async () => ({}));
  const createTransportSpy = vi.fn(() => ({ sendMail: sendMailSpy }));
  return { sendMailSpy, createTransportSpy };
});
vi.mock("nodemailer", () => ({ default: { createTransport: createTransportSpy } }));

const SMTP_ENV: Record<string, string> = {
  TOPOS_MAIL_SMTP_HOST: "smtp.example.com",
  TOPOS_MAIL_SMTP_PORT: "465",
  TOPOS_MAIL_SMTP_USER: "api_token",
  TOPOS_MAIL_SMTP_PASS: "relay-secret",
  TOPOS_MAIL_SMTP_FROM: "Topos <no-reply@example.com>",
};

let db: ScratchDb;

beforeAll(async () => {
  db = await createScratchDb("web_mail_log", SMTP_ENV);
}, 60_000);

afterAll(async () => {
  await db.drop();
});

async function rows(): Promise<
  { kind: string; recipient: string; outcome: string; code: string | null }[]
> {
  return db.q(`SELECT kind, recipient, outcome, code FROM web.mail_event ORDER BY id`);
}

describe("the mail_event send log", () => {
  it("lands ONE ok row on a landed send and ONE failed row (coarse code) on a refused one", async () => {
    const { sendMail } = await import("@/lib/mail/transport.server");

    await sendMail({ kind: "invite", to: "invitee@example.com", subject: "s", text: "t" });
    expect(await rows()).toEqual([
      { kind: "invite", recipient: "invitee@example.com", outcome: "ok", code: null },
    ]);

    sendMailSpy.mockRejectedValueOnce(new Error("550 relay text with a body preview"));
    await expect(
      sendMail({ kind: "auth-reset", to: "person@example.com", subject: "s", text: "secret" }),
    ).rejects.toThrow("mail send failed");
    expect(await rows()).toEqual([
      { kind: "invite", recipient: "invitee@example.com", outcome: "ok", code: null },
      {
        kind: "auth-reset",
        recipient: "person@example.com",
        outcome: "failed",
        code: "send_failed",
      },
    ]);
  });

  it("records the REAL no-transport refusal — sendMail with SMTP unset lands 'unconfigured'", async () => {
    // Disarm the transport for real: end the current pool (no leaked handles), drop the five
    // SMTP variables, and re-import the module graph so the memoized env re-parses.
    // DATABASE_URL still points at the scratch database, so the fresh pool the fresh DAL
    // opens writes the same real table.
    const { getPool } = await import("@/lib/db/index.server");
    await getPool().end();
    vi.resetModules();
    for (const key of Object.keys(SMTP_ENV)) {
      delete process.env[key];
    }
    const { sendMail } = await import("@/lib/mail/transport.server");
    await expect(
      sendMail({ kind: "magic-link", to: "person@example.com", subject: "s", text: "t" }),
    ).rejects.toThrow("mail transport is not configured");
    const last = (await rows()).at(-1);
    expect(last).toEqual({
      kind: "magic-link",
      recipient: "person@example.com",
      outcome: "failed",
      code: "unconfigured",
    });
  });

  it("has NO column a message could land in — metadata only, by construction", async () => {
    const columns = await db.q<{ column_name: string }>(
      `SELECT column_name FROM information_schema.columns
       WHERE table_schema = 'web' AND table_name = 'mail_event' ORDER BY column_name`,
    );
    expect(columns.map((c) => c.column_name)).toEqual([
      "code",
      "created_at",
      "id",
      "kind",
      "outcome",
      "recipient",
    ]);
  });
});
