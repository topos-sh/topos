import fs from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";
import { recordDevMail } from "@/lib/mail/dev-outbox.server";
import type { MailMessage } from "@/lib/mail/transport.server";
import { escapeHtml, sendMail } from "@/lib/mail/transport.server";

/**
 * Magic-link delivery over the ONE mail transport — the sender a composition wires into the auth
 * seam (`AuthProviderConfig.magicLink`). The OSS default deliberately does NOT register the rung
 * (email+password needs no delivery); the sender lives here so any composition that turns the
 * rung on delivers through the same transport as every other product mail.
 *
 * The URL is a LIVE CREDENTIAL: production sends it or throws coarse (better-auth surfaces the
 * failure to the login form — a silently-dropped link would read as a broken sign-in); it never
 * lands in a log or an error. Outside production the link is recorded to `.magic-links.jsonl`
 * (its OWN file — the invite notice must never land here, this file's reader hands sign-in flows
 * their link), the full mail to the accumulating dev outbox, and the dev terminal, so the real
 * sign-in flow works with mail suppressed.
 */
export async function sendMagicLinkEmail(args: { email: string; url: string }): Promise<void> {
  const { email, url } = args;
  const message: MailMessage = {
    kind: "magic-link",
    to: email,
    subject: "Sign in to Topos",
    text:
      `Sign in to Topos by opening this link:\n\n${url}\n\n` +
      `The link works for a few minutes and can be used once.\n` +
      `If you didn't request it, you can ignore this email.\n`,
    html:
      `<p>Sign in to Topos by opening this link:</p>` +
      `<p><a href="${escapeHtml(url)}">Sign in to Topos</a></p>` +
      `<p>The link works for a few minutes and can be used once. ` +
      `If you didn't request it, you can ignore this email.</p>`,
  };
  const appEnv = serverEnv().APP_ENV;
  if (appEnv !== "production") {
    const line = `${JSON.stringify({ email, url })}\n`;
    await fs.appendFile(path.join(process.cwd(), ".magic-links.jsonl"), line, "utf8");
    await recordDevMail(message);
    if (appEnv === "development") {
      // biome-ignore lint/suspicious/noConsole: the deliberate dev-only sign-in surface.
      console.log(`\n  Magic link for ${email} (dev — mail suppressed): ${url}\n`);
    }
    return;
  }
  await sendMail(message);
}
