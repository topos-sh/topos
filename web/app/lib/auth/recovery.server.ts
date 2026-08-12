import { randomBytes } from "node:crypto";
import { hashPassword } from "better-auth/crypto";
import { sql } from "drizzle-orm";
import { getDb } from "@/lib/db/index.server";

/**
 * The mail-less recovery hatch: a solo owner on an SMTP-less install who forgets their
 * password proves MACHINE CONTROL instead of mailbox control — the same primitive as setup.
 * A one-shot command on the box (`node scripts/mint-recovery-code.mjs <email>`, run inside
 * the web container) mints a short-lived code, stores ONLY its SHA-256 in the Better Auth
 * `verification` table (identifier `topos-recovery:<user id>`), and prints the plaintext to
 * the operator's terminal. The public /recovery form accepts the code + a new password;
 * consuming it re-hashes the password with Better Auth's own hasher (the one sign-in
 * verifies against — no second implementation) and deletes the row. With SMTP armed,
 * the standard reset-mail rung exists too; this hatch stays available either way.
 */

export const RECOVERY_IDENTIFIER_PREFIX = "topos-recovery:";

/**
 * Consume a presented recovery code: match by hash, re-hash the new password with the
 * product's one hasher, upsert the credential account row, delete the code. One shot: a
 * second submit of the same code is the uniform miss.
 */
export async function consumeRecoveryCode(
  code: string,
  newPassword: string,
): Promise<{ userId: string } | null> {
  const db = getDb();
  const passwordHash = await hashPassword(newPassword);
  return await db.transaction(async (tx) => {
    const rows = await tx.execute(
      sql`DELETE FROM web.verification
          WHERE value = encode(sha256(convert_to(${code}, 'UTF8')), 'hex')
            AND identifier LIKE ${`${RECOVERY_IDENTIFIER_PREFIX}%`}
            AND expires_at > now()
          RETURNING identifier`,
    );
    const identifier = (rows.rows[0] as { identifier: string } | undefined)?.identifier;
    if (!identifier) {
      return null;
    }
    const userId = identifier.slice(RECOVERY_IDENTIFIER_PREFIX.length);
    const updated = await tx.execute(
      sql`UPDATE web.account SET password = ${passwordHash}, updated_at = now()
          WHERE user_id = ${userId} AND provider_id = 'credential'
          RETURNING id`,
    );
    if (updated.rows.length === 0) {
      await tx.execute(
        sql`INSERT INTO web.account (id, account_id, provider_id, user_id, password, created_at, updated_at)
            VALUES (${`acc_${randomBytes(12).toString("hex")}`}, ${userId}, 'credential', ${userId},
                    ${passwordHash}, now(), now())`,
      );
    }
    // Every credential the old password could have opened dies with it — browser sessions and
    // the CLI bearers alike. The mailed reset gets this from the auth tier's own hooks; this
    // hatch writes the password itself, so it owns the teardown too.
    await tx.execute(sql`DELETE FROM web.session WHERE user_id = ${userId}`);
    const endedSessions = await tx.execute(
      sql`DELETE FROM web.cli_session WHERE user_id = ${userId} RETURNING id, workspace_id`,
    );
    // The recovery is a PERSON-scoped act, so its trail lands in each workspace the person is
    // seated in — never on an arbitrary `LIMIT 1` workspace, which on a multi-tenant install
    // would file one tenant's ceremony in an unrelated tenant's history (and on a virgin
    // database recorded nothing at all).
    await tx.execute(
      sql`INSERT INTO web.audit_event (workspace_id, actor_user_id, actor_display, kind, outcome)
          SELECT s.workspace_id, ${userId}, 'recovery', 'password_recovered', 'ok'
          FROM web.seat s WHERE s.user_id = ${userId}`,
    );
    for (const row of endedSessions.rows as { id: string; workspace_id: string }[]) {
      await tx.execute(
        sql`INSERT INTO web.audit_event
              (workspace_id, actor_user_id, actor_display, kind, subject, outcome, details)
            VALUES (${row.workspace_id}, ${userId}, 'recovery', 'session_ended', ${row.id}, 'ok',
                    ${JSON.stringify({ cause: "self" })}::jsonb)`,
      );
    }
    return { userId };
  });
}
