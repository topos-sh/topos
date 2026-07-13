import fs from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";
import { escapeHtml, mailDelivery, sendMail } from "@/lib/mail/transport.server";

/**
 * The invitation-mail seam. Invitation is already durable WITHOUT any email: the vault seats the
 * invited address (a roster row) and the workspace's ADDRESS is the shareable door — so this seam
 * carries a courtesy notice, never a credential. There is no tokened link anywhere in it: a
 * recipient joins by asking their agent to `follow <address>` and proving the invited email.
 *
 * Delivery rides the ONE mail transport (`transport.server.ts`): with the five
 * `TOPOS_MAIL_SMTP_*` set, `inviteMailDelivery().canSend` is true and production really sends;
 * unset, production is a deliberate no-op and the honest `mailed` flag stays false. Outside
 * production the notice is recorded to its OWN file, `.invite-emails.jsonl` (NEVER
 * `.magic-links.jsonl`, whose reader parses every line and would hand a sign-in flow the wrong
 * thing), plus the dev-server terminal — the two sanctioned non-email surfaces. A missing or
 * failed send never loses the invite, because the seat + the address already stand.
 */

export interface InviteEmailInput {
  to: string;
  /** The workspace's human-readable name (shown in the notice; user-entered). */
  workspaceDisplayName: string;
  /** The workspace ADDRESS slug — the door: `follow <address>`. Not a credential. */
  address: string;
  /** The inviter's email (attribution). */
  invitedBy: string;
}

export interface InviteMailDelivery {
  /** Whether real outbound delivery is wired — the transport's capability, read per call. */
  canSend: boolean;
}

/** Describes whether real invitation delivery exists — `{ canSend: true }` once SMTP is armed. */
export function inviteMailDelivery(): InviteMailDelivery {
  return { canSend: mailDelivery().canSend };
}

/** The notice body — shared by the text mail and the dev recording (user-entered fields escaped
 * only in the HTML mirror). The terminal line stays `topos follow <server>/<address>`-shaped when
 * the public origin is known; the address alone is the door either way. */
function inviteLines({ workspaceDisplayName, address, invitedBy }: InviteEmailInput): {
  subject: string;
  text: string;
  html: string;
} {
  const publicUrl = serverEnv().TOPOS_PUBLIC_URL;
  const host = publicUrl === undefined ? undefined : new URL(publicUrl).host;
  const terminalLine =
    host === undefined ? "" : `Or from a terminal: topos follow ${host}/${address}\n\n`;
  const subject = `You've been invited to ${workspaceDisplayName} on Topos`;
  const text =
    `${invitedBy} invited you to ${workspaceDisplayName} on Topos — shared skills for your AI agents.\n\n` +
    `Ask your agent to join: have it follow ${address}\n\n` +
    terminalLine +
    `If you weren't expecting this, you can ignore this email.\n`;
  const htmlTerminal =
    host === undefined
      ? ""
      : `<p>Or from a terminal: <code>topos follow ${escapeHtml(host)}/${escapeHtml(address)}</code></p>`;
  const html =
    `<p>${escapeHtml(invitedBy)} invited you to <strong>${escapeHtml(workspaceDisplayName)}</strong> on Topos — shared skills for your AI agents.</p>` +
    `<p>Ask your agent to join: have it follow <code>${escapeHtml(address)}</code></p>` +
    htmlTerminal +
    `<p>If you weren't expecting this, you can ignore this email.</p>`;
  return { subject, text, html };
}

/**
 * Send (or record, outside production) an invitation notice carrying the workspace display name +
 * ADDRESS. Callers may treat a throw as "notice not sent" and keep the seat + the address
 * standing — a mail-seam fault never fails an invite.
 */
export async function sendInviteEmail(input: InviteEmailInput): Promise<void> {
  const appEnv = serverEnv().APP_ENV;
  if (appEnv === "production") {
    if (!mailDelivery().canSend) {
      // No transport wired — the deliberate no-op posture: the invite is durable regardless
      // (the roster seat + the shareable address already stand) and `mailed` honestly said so.
      return;
    }
    const { subject, text, html } = inviteLines(input);
    await sendMail({ to: input.to, subject, text, html });
    return;
  }
  // Dev/test: record the notice to its OWN file so a flow can assert it (never a send, never the
  // magic-link file). The recorded fields carry the address — a plain slug, not a token.
  const { to, workspaceDisplayName, address, invitedBy } = input;
  const line = `${JSON.stringify({ to, workspaceDisplayName, address, invitedBy })}\n`;
  await fs.appendFile(path.join(process.cwd(), ".invite-emails.jsonl"), line, "utf8");
  if (appEnv === "development") {
    // biome-ignore lint/suspicious/noConsole: the deliberate dev-only invite surface.
    console.log(`\n  Invite for ${to} (dev — mail suppressed): follow ${address}\n`);
  }
}
