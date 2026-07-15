#!/usr/bin/env node
import { randomBytes } from "node:crypto";
/**
 * mint-recovery-code.mjs — the box-side half of the mail-less recovery hatch: mint a one-shot
 * recovery code for an account and print the plaintext to the operator's terminal. Run where
 * the server runs (machine control is the proof, the same primitive as setup):
 *
 *   DATABASE_URL=postgres://… node scripts/mint-recovery-code.mjs someone@example.com
 *
 * SAME SQL as app/lib/auth/recovery.server.ts's mintRecoveryCode — keep the SQL in lockstep.
 * Only the code's sha256 is stored (identifier `topos-recovery:<user id>`, 15-minute expiry,
 * any older code replaced); the public /recovery form consumes it. An unknown email prints a
 * miss and exits 1 — this is an operator-only surface on the box's own terminal, so honesty
 * beats a non-enumeration oracle here.
 */
import { Pool } from "pg";

const RECOVERY_IDENTIFIER_PREFIX = "topos-recovery:";
const RECOVERY_TTL_MS = 15 * 60 * 1000;

const email = process.argv[2];
if (!email) {
  console.error("usage: node scripts/mint-recovery-code.mjs <email>");
  process.exit(1);
}
const url = process.env.DATABASE_URL;
if (!url) {
  console.error("mint-recovery-code.mjs: DATABASE_URL is required");
  process.exit(1);
}

const pool = new Pool({ connectionString: url });
try {
  const lowered = email.trim().toLowerCase();
  const users = await pool.query('SELECT id FROM web."user" WHERE email = $1', [lowered]);
  const userId = users.rows[0]?.id;
  if (!userId) {
    console.error("no such account");
    process.exitCode = 1;
  } else {
    const code = randomBytes(16).toString("base64url");
    const identifier = `${RECOVERY_IDENTIFIER_PREFIX}${userId}`;
    const expiresAt = new Date(Date.now() + RECOVERY_TTL_MS);
    await pool.query("DELETE FROM web.verification WHERE identifier = $1", [identifier]);
    await pool.query(
      `INSERT INTO web.verification (id, identifier, value, expires_at, created_at, updated_at)
       VALUES ($1, $2, encode(sha256(convert_to($3, 'UTF8')), 'hex'), $4, now(), now())`,
      [`rec_${randomBytes(12).toString("hex")}`, identifier, code, expiresAt],
    );
    process.stdout.write(`recovery code: ${code}\n`);
    process.stdout.write(`expires: ${expiresAt.toISOString()} — single-use, redeem at /recovery\n`);
  }
} finally {
  await pool.end();
}
