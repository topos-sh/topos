import type { Transporter } from "nodemailer";
import nodemailer from "nodemailer";
import { serverEnv } from "@/env.server";
import { type MailEventKind, recordMailEvent } from "@/lib/db/mail-log.server";

/**
 * The ONE outbound-mail transport — every mail the product sends (the invite notice, the
 * verification + reset mails, a composition's magic link) goes through [`sendMail`]; no other
 * module may hold an SMTP client. BRING YOUR OWN SMTP: the five `TOPOS_MAIL_SMTP_*` variables
 * arm it all-or-nothing; unarmed, [`mailDelivery`]`().canSend` is false and every calling flow
 * keeps its honest no-send behavior (and for a MULTI-USER install, armed mail is the identity
 * rung — inviting requires it, since the invited sign-up proves itself through the mailbox).
 *
 * Redaction: a mail body may carry a live credential (a verification link, a sign-in link), so
 * a send failure NEVER echoes the message, the recipient, or the relay's response — callers
 * get one coarse error, and each flow's own contract says what a lost mail means (an invite's
 * rows + the address stand, a magic-link request surfaces the provider's failure to the login
 * form).
 *
 * Every send ATTEMPT lands one metadata-only `mail_event` row (lib/db/mail-log.server.ts —
 * the DAL's one actor-less system write): kind, recipient, ok/failed, at most a coarse code.
 * The log follows the same redaction rule — no subject, body, or relay text ever lands in it.
 */

/** Which product flow produced a mail — rides the message, tags its log row + dev-outbox line.
 * ONE vocabulary end-to-end: the union is defined beside the log writer (whose table CHECK
 * carries the same values), so a caller cannot mint a kind the log would refuse. */
export type MailKind = MailEventKind;

export interface MailMessage {
  kind: MailKind;
  to: string;
  subject: string;
  text: string;
  html?: string;
}

export interface MailDelivery {
  /** Whether real outbound delivery is wired (all five `TOPOS_MAIL_SMTP_*` set). */
  canSend: boolean;
}

interface SmtpSettings {
  host: string;
  port: number;
  user: string;
  pass: string;
  from: string;
}

/** The five-or-none resolve: any missing variable means NO transport (never a partial one). */
function smtpSettings(): SmtpSettings | null {
  const env = serverEnv();
  const host = env.TOPOS_MAIL_SMTP_HOST;
  const port = env.TOPOS_MAIL_SMTP_PORT;
  const user = env.TOPOS_MAIL_SMTP_USER;
  const pass = env.TOPOS_MAIL_SMTP_PASS;
  const from = env.TOPOS_MAIL_SMTP_FROM;
  if (!host || port === undefined || !user || !pass || !from) {
    return null;
  }
  return { host, port, user, pass, from };
}

/** Whether real outbound delivery exists — the capability every honest `mailed` flag reads. */
export function mailDelivery(): MailDelivery {
  return { canSend: smtpSettings() !== null };
}

let cached: Transporter | undefined;

/** One lazy transporter per process (nodemailer pools the connection; 465 is implicit TLS). */
function transporter(settings: SmtpSettings): Transporter {
  cached ??= nodemailer.createTransport({
    host: settings.host,
    port: settings.port,
    secure: settings.port === 465,
    auth: { user: settings.user, pass: settings.pass },
  });
  return cached;
}

/**
 * Send one mail through the configured relay. Throws a COARSE error when the transport is
 * unconfigured or the relay refuses — never the message, the recipient, or the relay response
 * (a body can carry a live credential). Every attempt — landed or not — is logged to the
 * metadata-only `mail_event` table before this returns or throws.
 */
export async function sendMail(message: MailMessage): Promise<void> {
  const settings = smtpSettings();
  if (settings === null) {
    await recordMailEvent(message.kind, message.to, {
      outcome: "failed",
      code: "unconfigured",
    });
    throw new Error("mail transport is not configured");
  }
  try {
    await transporter(settings).sendMail({
      from: settings.from,
      to: message.to,
      subject: message.subject,
      text: message.text,
      ...(message.html === undefined ? {} : { html: message.html }),
    });
  } catch {
    // Deliberately CAUSE-LESS: a relay error can echo the envelope (recipient, body preview) and
    // the body may carry a live credential — the caller learns only that the send failed.
    await recordMailEvent(message.kind, message.to, { outcome: "failed", code: "send_failed" });
    throw new Error("mail send failed");
  }
  await recordMailEvent(message.kind, message.to, { outcome: "ok" });
}

/** Escape a user-entered value for an HTML mail body (the invite/magic-link mirrors). */
export function escapeHtml(value: string): string {
  return value
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}
