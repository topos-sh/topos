import fs from "node:fs/promises";
import path from "node:path";
import { serverEnv } from "@/env.server";

/**
 * The invitation-mail seam. Invitation is already durable WITHOUT any email: the vault seats the
 * invited address (a roster row) and the workspace's ADDRESS is the shareable door — so this seam
 * carries a courtesy notice, never a credential. There is no tokened link anywhere in it: a
 * recipient joins by asking their agent to `follow <address>` and proving the invited email.
 *
 * Honest posture (the OSS default): NO outbound delivery is wired — `inviteMailDelivery().canSend`
 * is always false, and production is a deliberate no-op. Outside production the notice is recorded
 * to its OWN file, `.invite-emails.jsonl` (NEVER `.magic-links.jsonl`, whose reader parses every
 * line and would hand a sign-in flow the wrong thing), plus the dev-server terminal — the two
 * sanctioned non-email surfaces. A downstream/superset build layers a real transport over this
 * seam; here a missing send never loses the invite, because the seat + the address already stand.
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
  /** Whether real outbound delivery is wired. Always false in the OSS default. */
  canSend: boolean;
}

/** Describes whether real invitation delivery exists — always `{ canSend: false }` here. */
export function inviteMailDelivery(): InviteMailDelivery {
  return { canSend: false };
}

/**
 * Record (never actually send, in the OSS default) an invitation notice carrying the workspace
 * display name + ADDRESS. Callers may treat a throw as "notice not recorded" and keep the seat +
 * the address standing — a mail-seam fault never fails an invite.
 */
export async function sendInviteEmail({
  to,
  workspaceDisplayName,
  address,
  invitedBy,
}: InviteEmailInput): Promise<void> {
  const appEnv = serverEnv().APP_ENV;
  if (appEnv === "production") {
    // No outbound transport is wired in the OSS default (inviteMailDelivery().canSend === false).
    // The invite is durable regardless — the roster seat + the shareable address already stand.
    return;
  }
  // Dev/test: record the notice to its OWN file so a flow can assert it (never a send, never the
  // magic-link file). The recorded fields carry the address — a plain slug, not a token.
  const line = `${JSON.stringify({ to, workspaceDisplayName, address, invitedBy })}\n`;
  await fs.appendFile(path.join(process.cwd(), ".invite-emails.jsonl"), line, "utf8");
  if (appEnv === "development") {
    // biome-ignore lint/suspicious/noConsole: the deliberate dev-only invite surface.
    console.log(`\n  Invite for ${to} (dev — mail suppressed): follow ${address}\n`);
  }
}
