import type { Pool } from "pg";
import { digestsEqual, KEY_BYTES, workspaceKeyAad, type MasterKeyBackend } from "./crypto";
import type { Logger } from "./log";
import { KEY_BACKENDS, type KeyBackendName } from "./master-key";

/**
 * Moving a deployment's wrapped data keys from one master key backend to another — file → KMS
 * being the case this exists for.
 *
 * Only `gateway.workspace_key` changes. The data keys themselves are untouched, so every
 * `credential_secret` ciphertext stays exactly as it was: this rewrites the WRAP, never the
 * credential.
 *
 * The three properties that make it safe to run against a production database:
 *
 *  · IDEMPOTENT — a row the destination can already open is left alone. That test is a real
 *    unwrap, not a version-byte guess, so re-running after any interruption converts exactly the
 *    rows that still need it.
 *  · ATOMIC PER ROW — each row is locked, converted, and committed on its own. An interrupted run
 *    leaves every row either fully old or fully new; there is no half-written wrap.
 *  · VERIFIED BEFORE WRITE — the new envelope is unwrapped again through the destination and
 *    compared to the plaintext before the UPDATE. A backend that cannot read back what it just
 *    wrote fails the row instead of storing bytes nobody can open.
 *
 * A row that neither backend can open is reported and SKIPPED, never overwritten — that row is
 * evidence, and the operator needs it intact.
 *
 * NOTHING NEEDS ITS DATA-KEY CACHE CLEARED. This sweep runs as a one-off process that holds no
 * cache, against a stopped gateway; and even a running one could not serve a converted row from a
 * stale entry, because `EnvelopeCrypto` keys its cache by the WRAPPED BYTES as well as the
 * workspace — the row's new bytes are a new cache key.
 *
 * The run never logs key material: workspace ids and counts only.
 */

export interface RewrapPlan {
  /** The backend that wrote the rows today. */
  from: KeyBackendName;
  dryRun: boolean;
}

export const REWRAP_USAGE = `usage: bun scripts/rewrap-workspace-keys.ts --from <${KEY_BACKENDS.join(
  "|",
)}> [--dry-run]`;

/**
 * What a re-wrap run was asked to do, or a refusal that says why. The destination is never an
 * argument: it is `GATEWAY_KEY_BACKEND`, so a run can only move rows toward the backend the
 * deployment is about to boot with.
 */
export function planRewrap(argv: readonly string[], destination: KeyBackendName): RewrapPlan {
  const dryRun = argv.includes("--dry-run");
  const at = argv.indexOf("--from");
  const from = at === -1 ? undefined : argv[at + 1];
  for (const [index, arg] of argv.entries()) {
    if (arg !== "--dry-run" && arg !== "--from" && index !== at + 1) {
      throw new Error(`unrecognized argument: ${arg}\n\n${REWRAP_USAGE}`);
    }
  }
  if (from === undefined) {
    throw new Error(`--from is required: name the backend that wrote the rows today\n\n${REWRAP_USAGE}`);
  }
  if (!KEY_BACKENDS.includes(from as KeyBackendName)) {
    throw new Error(`--from must be one of ${KEY_BACKENDS.join(", ")}\n\n${REWRAP_USAGE}`);
  }
  if (from === destination) {
    // Re-keying WITHIN one backend (a new key file, a rotated KMS key) would need a second set of
    // variables to say which key is the old one. Nothing here can express that, so refuse rather
    // than convert every row to itself and report success.
    throw new Error(
      `--from ${from} matches GATEWAY_KEY_BACKEND; there is nothing to move ` +
        "(re-keying within one backend is not supported here)",
    );
  }
  return { from: from as KeyBackendName, dryRun };
}

export interface RewrapOptions {
  /** The backend that wrote the rows today. */
  from: MasterKeyBackend;
  /** The backend the deployment is moving to. */
  to: MasterKeyBackend;
  /** Do every step including the read-back verification, then roll back instead of committing. */
  dryRun: boolean;
  log: Logger;
}

export interface RewrapSummary {
  scanned: number;
  converted: number;
  alreadyThere: number;
  failed: number;
}

async function openWith(
  backend: MasterKeyBackend,
  wrapped: Buffer,
  workspaceId: string,
): Promise<Buffer | null> {
  try {
    const key = await backend.unwrapDataKey(wrapped, workspaceKeyAad(workspaceId));
    return key.length === KEY_BYTES ? key : null;
  } catch {
    return null;
  }
}

export async function rewrapWorkspaceKeys(
  pool: Pool,
  options: RewrapOptions,
): Promise<RewrapSummary> {
  const { from, to, dryRun, log } = options;
  const summary: RewrapSummary = { scanned: 0, converted: 0, alreadyThere: 0, failed: 0 };
  const listed = await pool.query<{ workspace_id: string }>(
    "SELECT workspace_id FROM gateway.workspace_key ORDER BY workspace_id",
  );
  log("info", "re-wrap starting", {
    rows: listed.rows.length,
    from: from.describe,
    to: to.describe,
    mode: dryRun ? "dry-run" : "write",
  });

  for (const { workspace_id: workspaceId } of listed.rows) {
    summary.scanned += 1;
    const client = await pool.connect();
    try {
      await client.query("BEGIN");
      // Re-read under the row lock: a parallel run (or a second replica) may already have
      // converted this row since the listing above.
      const locked = await client.query<{ wrapped_key: Buffer }>(
        "SELECT wrapped_key FROM gateway.workspace_key WHERE workspace_id = $1 FOR UPDATE",
        [workspaceId],
      );
      const standing = locked.rows[0]?.wrapped_key;
      if (standing === undefined) {
        // Deleted between the listing and the lock — nothing to convert, nothing wrong.
        await client.query("ROLLBACK");
        summary.scanned -= 1;
        continue;
      }
      if ((await openWith(to, standing, workspaceId)) !== null) {
        await client.query("ROLLBACK");
        summary.alreadyThere += 1;
        continue;
      }
      const plaintext = await openWith(from, standing, workspaceId);
      if (plaintext === null) {
        await client.query("ROLLBACK");
        summary.failed += 1;
        log("error", "workspace key opens under neither backend; left untouched", { workspaceId });
        continue;
      }
      const rewrapped = await to.wrapDataKey(plaintext, workspaceKeyAad(workspaceId));
      const readBack = await openWith(to, rewrapped, workspaceId);
      if (readBack === null || !digestsEqual(plaintext, readBack)) {
        await client.query("ROLLBACK");
        summary.failed += 1;
        log("error", "re-wrapped key failed its read-back; row left untouched", { workspaceId });
        continue;
      }
      await client.query(
        "UPDATE gateway.workspace_key SET wrapped_key = $2 WHERE workspace_id = $1",
        [workspaceId, rewrapped],
      );
      // The dry run has now proven the whole path — unwrap, re-wrap, read back — and throws the
      // write away. It is the honest rehearsal: everything a real run does except the commit.
      await client.query(dryRun ? "ROLLBACK" : "COMMIT");
      summary.converted += 1;
      log("info", dryRun ? "workspace key would be re-wrapped" : "workspace key re-wrapped", {
        workspaceId,
      });
    } catch (error) {
      await client.query("ROLLBACK").catch(() => {});
      summary.failed += 1;
      log("error", "re-wrap failed for a workspace; row left untouched", {
        workspaceId,
        detail: String(error).slice(0, 200),
      });
    } finally {
      client.release();
    }
  }

  log(summary.failed === 0 ? "info" : "error", "re-wrap finished", {
    ...summary,
    mode: dryRun ? "dry-run" : "write",
  });
  return summary;
}
