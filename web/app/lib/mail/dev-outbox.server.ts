import fs from "node:fs/promises";
import path from "node:path";
import type { MailMessage } from "@/lib/mail/transport.server";

/**
 * The ONE accumulating dev outbox — `.outbox.jsonl` in the server's working directory. Outside
 * production every product mail lands here as one JSON line (`{at, kind, to, subject, text,
 * html?}`): the exact message production would hand the transport, so a human tester watches a
 * single file to see everything the app "sent". The per-flow credential files
 * (`.magic-links.jsonl`, `.invite-emails.jsonl`) stand unchanged — each flow's reader parses
 * its OWN file; this one is the superset view, read by humans, never parsed back into a flow.
 */

export const DEV_OUTBOX_FILE = ".outbox.jsonl";

/** Which product flow produced the mail — a display/filter tag, never branched on. */
export type DevMailKind = "magic-link" | "invite" | "auth-verify" | "auth-reset";

/** Append one full rendered mail to the accumulating dev outbox (non-production callers only). */
export async function recordDevMail(kind: DevMailKind, message: MailMessage): Promise<void> {
  const entry = { at: new Date().toISOString(), kind, ...message };
  const line = `${JSON.stringify(entry)}\n`;
  await fs.appendFile(path.join(process.cwd(), DEV_OUTBOX_FILE), line, "utf8");
}
