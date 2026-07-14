import fs from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";
import { recordDevMail } from "@/lib/mail/dev-outbox.server";
import type { MailMessage } from "@/lib/mail/transport.server";
import { mailDelivery, sendMail } from "@/lib/mail/transport.server";

/**
 * Passcode delivery over the ONE mail transport — the send half of the enrollment second factor
 * (the vault MINTS the code over its internal lane; this tier mails it and answers the constant
 * ack — see `routes/api.v1.enroll.passcode.ts`).
 *
 * The code is a LIVE CREDENTIAL with its own short clock: production without a transport drops it
 * silently (the old vault no-op posture — the ack stays constant-shaped, and a deployment that
 * advertises the passcode method is expected to arm SMTP); a relay failure is the caller's
 * fire-and-forget to swallow. It never lands in a production log or an error. Outside production
 * the code is recorded to its OWN `.passcode-emails.jsonl` (never the magic-link file), the full
 * mail to the accumulating dev outbox, and the dev terminal, so the flow is drivable with mail
 * suppressed.
 */
export interface PasscodeEmailInput {
  to: string;
  /** The 6-digit code — plaintext exactly once, from the mint response into this mail. */
  code: string;
  /** The workspace display name the mint disclosed (empty for a workspace-less login session). */
  workspaceDisplayName: string;
  /** The HUMAN-facing base the `{base}/verify` line points at (the app's own public origin). */
  verifyBaseUrl: string;
}

export async function sendPasscodeEmail(input: PasscodeEmailInput): Promise<void> {
  const { to, code, workspaceDisplayName, verifyBaseUrl } = input;
  // TEXT-ONLY on purpose (the old vault mail's shape, kept byte-similar).
  const message: MailMessage = {
    to,
    subject: "Your Topos verification code",
    text:
      `Your Topos verification code for ${workspaceDisplayName} is ${code}.\n\n` +
      `Enter it at ${verifyBaseUrl}/verify to finish connecting your agent.\n` +
      `If you didn't request this, you can ignore this email.\n`,
  };
  const appEnv = serverEnv().APP_ENV;
  if (appEnv !== "production") {
    const line = `${JSON.stringify({ to, code, workspaceDisplayName })}\n`;
    await fs.appendFile(path.join(process.cwd(), ".passcode-emails.jsonl"), line, "utf8");
    await recordDevMail("passcode", message);
    if (appEnv === "development") {
      // biome-ignore lint/suspicious/noConsole: the deliberate dev-only enrollment surface.
      console.log(`\n  Passcode for ${to} (dev — mail suppressed): ${code}\n`);
    }
    return;
  }
  if (!mailDelivery().canSend) {
    // No transport wired — the old vault NoopMailer posture, kept: silent, so the public ack's
    // constant shape holds; the code expires on its own clock.
    return;
  }
  await sendMail(message);
}
