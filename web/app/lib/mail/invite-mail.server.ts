import fs from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";
import { recordDevMail } from "@/lib/mail/dev-outbox.server";
import { escapeHtml, mailDelivery, sendMail } from "@/lib/mail/transport.server";

/**
 * The invitation-mail seam. The mail is where the single-use INVITE LINK travels — worth one
 * invitation, never an account or a credential (only the token's hash is stored server-side;
 * the inviter's own surfaces never show it). Three ways in, in this order: the browser link
 * (accept in one click), the agent paste-block, and the terminal line — the same tokened URL
 * all three times, so whichever the recipient grabs redeems the same invitation.
 *
 * Delivery rides the ONE mail transport (`transport.server.ts`): with the five
 * `TOPOS_MAIL_SMTP_*` set, `inviteMailDelivery().canSend` is true and production really sends;
 * unset, production is a deliberate no-op and the honest `mailed` flag stays false. Outside
 * production the notice is recorded to its OWN file, `.invite-emails.jsonl` (NEVER
 * `.magic-links.jsonl`, whose reader parses every line and would hand a sign-in flow the wrong
 * thing), the full mail to the accumulating dev outbox, plus the dev-server terminal — the
 * sanctioned non-email surfaces. A missing or failed send never loses the invite: the
 * invitation row stands, and re-inviting the address mints a fresh link.
 */

export interface InviteEmailInput {
  to: string;
  /** The workspace's human-readable name (shown in the notice; user-entered). */
  workspaceDisplayName: string;
  /** The tokened invitation URL — the door all three CTAs share. */
  inviteUrl: string;
  /** The deployment's agent-onboarding doc (`<origin>/agent`) — the agent paste-block fetches it. */
  agentUrl: string;
  /** The inviter's email (attribution). */
  invitedBy: string;
  /** The optional first-destination hint the invitation leads with (`kind` is the bundle
   * catalog's tag — 'skill' today — or 'channel'). */
  hint?: { kind: string; name: string };
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
 * only in the HTML mirror). The hint leads: a hinted invitation names its first destination in
 * the subject and the opening line, because that is what the invitee was invited FOR. */
function inviteLines({
  workspaceDisplayName,
  inviteUrl,
  agentUrl,
  invitedBy,
  hint,
}: InviteEmailInput): {
  subject: string;
  text: string;
  html: string;
} {
  const hintLead = hint === undefined ? "" : ` — starting with the ${hint.name} ${hint.kind}`;
  const subject = `You're invited to ${workspaceDisplayName} on Topos${hintLead}`;
  const intro =
    `${invitedBy} invited you to ${workspaceDisplayName} on Topos — shared skills for your AI agents.` +
    (hint === undefined ? "" : ` First up: the ${hint.name} ${hint.kind}.`);
  const agentPaste = `Set up Topos for us: fetch ${agentUrl} and follow it. Our invite: ${inviteUrl}`;
  const text =
    `${intro}\n\n` +
    `Accept in your browser:\n\n` +
    `  ${inviteUrl}\n\n` +
    `Or ask your agent to join — paste this to it:\n\n` +
    `  ${agentPaste}\n\n` +
    `Or from a terminal: topos follow ${inviteUrl}\n\n` +
    `This link is for you alone and lapses after a while. If you weren't expecting it, you can ignore this email.\n`;
  const html =
    `<p>${escapeHtml(invitedBy)} invited you to <strong>${escapeHtml(workspaceDisplayName)}</strong> on Topos — shared skills for your AI agents.${
      hint === undefined
        ? ""
        : ` First up: the <strong>${escapeHtml(hint.name)}</strong> ${escapeHtml(hint.kind)}.`
    }</p>` +
    `<p><a href="${escapeHtml(inviteUrl)}">Accept in your browser</a></p>` +
    `<p>Or ask your agent to join — paste this to it:</p>` +
    `<p><code>${escapeHtml(agentPaste)}</code></p>` +
    `<p>Or from a terminal: <code>topos follow ${escapeHtml(inviteUrl)}</code></p>` +
    `<p>This link is for you alone and lapses after a while. If you weren't expecting it, you can ignore this email.</p>`;
  return { subject, text, html };
}

/**
 * Send (or record, outside production) an invitation notice carrying the tokened invite link.
 * Callers may treat a throw as "notice not sent" — the invitation row stands regardless, and
 * re-inviting mints a fresh link.
 */
export async function sendInviteEmail(input: InviteEmailInput): Promise<void> {
  const appEnv = serverEnv().APP_ENV;
  if (appEnv === "production") {
    if (!mailDelivery().canSend) {
      // No transport wired — the deliberate no-op posture: the invitation row is durable and
      // the honest `mailed` flag already said nothing was delivered.
      return;
    }
    const { subject, text, html } = inviteLines(input);
    await sendMail({ to: input.to, subject, text, html });
    return;
  }
  // Dev/test: record the notice to its OWN file so a flow can assert it (never a send, never the
  // magic-link file), plus the full mail to the accumulating dev outbox. The recorded fields
  // carry the tokened invite URL — the dev-mode stand-in for the recipient's mailbox.
  const { to, workspaceDisplayName, inviteUrl, invitedBy, hint } = input;
  const line = `${JSON.stringify({ to, workspaceDisplayName, inviteUrl, invitedBy, ...(hint === undefined ? {} : { hint }) })}\n`;
  await fs.appendFile(path.join(process.cwd(), ".invite-emails.jsonl"), line, "utf8");
  const { subject, text, html } = inviteLines(input);
  await recordDevMail("invite", { to, subject, text, html });
  if (appEnv === "development") {
    // biome-ignore lint/suspicious/noConsole: the deliberate dev-only invite surface.
    console.log(`\n  Invite for ${to} (dev — mail suppressed): ${inviteUrl}\n`);
  }
}
