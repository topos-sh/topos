import { serverEnv } from "@/env.server";
import { recordDevMail } from "./dev-outbox.server";
import { escapeHtml, type MailMessage, mailDelivery, sendMail } from "./transport.server";

/**
 * The auth rungs' two mails — address verification (the invited sign-up's identity rung) and
 * the password reset — riding the app's ONE transport. Armed-only like every product mail;
 * outside production each rendered message also lands in the accumulating dev outbox so the
 * e2e mail-sink suites read what "sent" means. Coarse errors only: a body carries a live
 * token, so no failure ever echoes the message or the recipient.
 */

function verificationMessage(to: string, url: string): MailMessage {
  return {
    to,
    subject: "Verify your email",
    text: `Confirm this address to finish joining: ${url}\n\nIf you didn't request this, ignore this mail.`,
    html: `<p>Confirm this address to finish joining:</p><p><a href="${escapeHtml(url)}">${escapeHtml(url)}</a></p><p>If you didn't request this, ignore this mail.</p>`,
  };
}

function resetMessage(to: string, url: string): MailMessage {
  return {
    to,
    subject: "Reset your password",
    text: `Reset your password: ${url}\n\nIf you didn't request this, ignore this mail.`,
    html: `<p>Reset your password:</p><p><a href="${escapeHtml(url)}">${escapeHtml(url)}</a></p><p>If you didn't request this, ignore this mail.</p>`,
  };
}

export async function sendVerificationMail(to: string, url: string): Promise<void> {
  const message = verificationMessage(to, url);
  if (serverEnv().APP_ENV !== "production") {
    await recordDevMail("auth-verify", message);
  }
  if (mailDelivery().canSend) {
    await sendMail(message);
  }
}

export async function sendResetMail(to: string, url: string): Promise<void> {
  const message = resetMessage(to, url);
  if (serverEnv().APP_ENV !== "production") {
    await recordDevMail("auth-reset", message);
  }
  if (mailDelivery().canSend) {
    await sendMail(message);
  }
}
