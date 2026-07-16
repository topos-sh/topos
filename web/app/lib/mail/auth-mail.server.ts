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

/**
 * The step-up confirmation link a password-less account confirms an admin ceremony with — the
 * mail rung of the step-up gate (step-up.server's `beginStepUpConfirmation`). The URL is a LIVE,
 * single-use, short-TTL credential returning to the exact ceremony page, so it is treated like a
 * magic link: coarse errors only, never echoed in a log.
 */
function stepUpMessage(to: string, url: string): MailMessage {
  return {
    to,
    subject: "Confirm an action on Topos",
    text:
      `Confirm the action you started on Topos by opening this link:\n\n${url}\n\n` +
      `The link works for a few minutes and can be used once.\n` +
      `If you didn't start it, you can ignore this email — nothing changes.\n`,
    html:
      `<p>Confirm the action you started on Topos by opening this link:</p>` +
      `<p><a href="${escapeHtml(url)}">Confirm the action</a></p>` +
      `<p>The link works for a few minutes and can be used once. ` +
      `If you didn't start it, you can ignore this email — nothing changes.</p>`,
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

/**
 * Send the step-up confirmation link over the ONE transport — the rung a password-less account
 * confirms an admin ceremony with (the caller reaches here only after `mailDelivery().canSend`).
 * Mirrors the magic link: outside production the full mail lands in the dev outbox and nothing is
 * relayed (dev/e2e reads the captured link); production sends through the armed transport or
 * throws COARSE, which the caller turns into the constant refusal — never a silent dead end.
 */
export async function sendStepUpMail(to: string, url: string): Promise<void> {
  const message = stepUpMessage(to, url);
  if (serverEnv().APP_ENV !== "production") {
    await recordDevMail("step-up", message);
    return;
  }
  await sendMail(message);
}
