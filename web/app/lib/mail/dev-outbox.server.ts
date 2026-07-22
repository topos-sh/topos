import fs from "node:fs/promises";
import path from "node:path";
import type { MailMessage } from "@/lib/mail/transport.server";

/**
 * The ONE accumulating dev outbox — `.outbox.jsonl` in the server's working directory. Outside
 * production every product mail lands here as one JSON line (`{at, kind, to, subject, text,
 * html?}`): the exact message production would hand the transport, so a human tester watches a
 * single file to see everything the app "sent". The message's own `kind` (the transport's
 * MailKind) tags the line — a display/filter tag, never branched on. The per-flow credential
 * files (`.magic-links.jsonl`, `.invite-emails.jsonl`) stand unchanged — each flow's reader
 * parses its OWN file; this one is the superset view, read by humans, never parsed back into a
 * flow.
 */

export const DEV_OUTBOX_FILE = ".outbox.jsonl";

/** Append one full rendered mail to the accumulating dev outbox (non-production callers only). */
export async function recordDevMail(message: MailMessage): Promise<void> {
  const entry = { at: new Date().toISOString(), ...message };
  const line = `${JSON.stringify(entry)}\n`;
  await fs.appendFile(path.join(process.cwd(), DEV_OUTBOX_FILE), line, "utf8");
}
