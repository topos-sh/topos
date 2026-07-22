import { getDb } from "@/lib/db/index.server";
import { mailEvent } from "@/lib/db/schema.app";

/**
 * The mail send log's ONE writer — called by the one transport (lib/mail/transport.server.ts)
 * on every send attempt, success and failure alike. THE DAL'S ONE ACTOR-LESS EXCEPTION: every
 * other DAL function requires a branded actor, but a mail send is a SYSTEM act (the transport
 * fires inside auth rungs and invite flows where no workspace actor exists), so this write
 * carries none — it records that the server tried to hand a message to the relay, nothing more.
 *
 * METADATA-ONLY by design: kind, recipient, outcome, and at most a coarse machine code. Never
 * the subject, body, token, or relay response — a mail body can carry a live credential, and
 * the transport's coarse-failure posture extends to its log (the table has no column a body
 * COULD land in). Best-effort like the ceremony audit writer: a log fault must never mask —
 * or fail — the send itself.
 */

export type MailEventOutcome =
  | { outcome: "ok" }
  | { outcome: "failed"; code: "unconfigured" | "send_failed" };

export async function recordMailEvent(
  kind: string,
  recipient: string,
  result: MailEventOutcome,
): Promise<void> {
  try {
    await getDb()
      .insert(mailEvent)
      .values({
        kind,
        recipient,
        outcome: result.outcome,
        code: result.outcome === "failed" ? result.code : null,
      });
  } catch (error) {
    console.error("mail_event insert failed", error);
  }
}
