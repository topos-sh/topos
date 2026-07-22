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
const RECOVERY_TTL_MS = 15 * 60 * 1000;

/**
 * Mint + store the code for an email's user; null when no such user exists (script prints a
 * miss). Mirrors scripts/mint-recovery-code.mjs — keep the SQL in lockstep.
 */
export async function mintRecoveryCode(
  email: string,
): Promise<{ code: string; expiresInMinutes: number } | null> {
  const db = getDb();
  const lowered = email.trim().toLowerCase();
  const users = await db.execute(sql`SELECT id FROM web."user" WHERE email = ${lowered}`);
  const userId = (users.rows[0] as { id: string } | undefined)?.id;
  if (!userId) {
    return null;
  }
  const code = randomBytes(16).toString("base64url");
  const identifier = `${RECOVERY_IDENTIFIER_PREFIX}${userId}`;
  const expiresAt = new Date(Date.now() + RECOVERY_TTL_MS);
  await db.execute(sql`DELETE FROM web.verification WHERE identifier = ${identifier}`);
  await db.execute(
    sql`INSERT INTO web.verification (id, identifier, value, expires_at, created_at, updated_at)
        VALUES (${`rec_${randomBytes(12).toString("hex")}`}, ${identifier},
                encode(sha256(convert_to(${code}, 'UTF8')), 'hex'), ${expiresAt}, now(), now())`,
  );
  return { code, expiresInMinutes: RECOVERY_TTL_MS / 60_000 };
}

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
    await tx.execute(
      sql`INSERT INTO web.audit_event (workspace_id, actor_user_id, actor_display, kind, outcome)
          SELECT w.id, ${userId}, 'recovery', 'password_recovered', 'ok' FROM web.workspace w LIMIT 1`,
    );
    return { userId };
  });
}
