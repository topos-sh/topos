import type { Transporter } from "nodemailer";
import nodemailer from "nodemailer";
import { serverEnv } from "@/env.server";

/**
 * The ONE outbound-mail transport — every mail the product sends (the invite notice, the
 * enrollment passcode, a composition's magic link) goes through [`sendMail`]; no other module may
 * hold an SMTP client. BRING YOUR OWN SMTP: the five `TOPOS_MAIL_SMTP_*` variables arm it
 * all-or-nothing (the vault's old five-flag rule, moved app-side with the mail unification —
 * the vault holds no mail transport at all now); unarmed, [`mailDelivery`]`().canSend` is false
 * and every calling flow keeps its honest no-send behavior.
 *
 * Redaction: a mail body may carry a live credential (a passcode, a sign-in link), so a send
 * failure NEVER echoes the message, the recipient, or the relay's response — callers get one
 * coarse error, and each flow's own contract says what a lost mail means (an invite's seat +
 * address stand, a passcode ack stays constant-shaped, a magic-link request surfaces the
 * provider's failure to the login form).
 */

export interface MailMessage {
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
 * (a body can carry a live credential).
 */
export async function sendMail(message: MailMessage): Promise<void> {
  const settings = smtpSettings();
  if (settings === null) {
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
    throw new Error("mail send failed");
  }
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
