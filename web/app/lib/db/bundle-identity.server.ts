import { sql } from "drizzle-orm";
import { BUNDLE_KINDS } from "@/lib/bundle-base";
import { getDb } from "@/lib/db/index.server";
import { serverDocumentOf } from "@/lib/mcp/catalog.server";

/**
 * A BUNDLE'S SECOND NAME, AS ROWS — the read/write half of `web.bundle_identity`.
 *
 * Some kinds carry a name their machine consumers resolve on top of the catalog name people
 * type: an MCP server's `server.json` declares a registry name, and this workspace's registry
 * lane answers `…/servers/{name}` with it. Two bundles declaring one name would make an agent's
 * lookup a coin flip, so the name is claimed — and the claim is a ROW whose primary key refuses
 * the second claimant.
 *
 * THAT REFUSAL IS THE WHOLE POINT. The uniqueness rule reads BYTES, which no query reaches, so
 * it used to be enforced by scanning every published document under a per-workspace advisory
 * lock and comparing. A lock plus a bounded scan is an index written the long way: it costs a
 * vault round-trip per bundle on every publish, it caps out (a catalog past the cap could only
 * answer "cannot confirm"), and being a check-then-write it is only atomic while the lock is
 * held. The row is none of those things — the database refuses the duplicate, so the refusal is
 * a conflict rather than a comparison.
 *
 * Claims are written in the SAME transaction as the pointer move that makes them true, and
 * released when the bundle archives or is deleted (both free the name for someone else).
 */

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/** The one client a read runs on: a caller inside a transaction passes it, so the read sees that
 *  transaction's own uncommitted writes. Absent, the shared pool answers. */
export type IdentityDbClient = ReturnType<typeof getDb> | Tx;

/** Who holds an identity here: the holding bundle's id and catalog NAME (what a refusal says). */
export interface IdentityHolder {
  bundleId: string;
  name: string;
}

/**
 * CLAIM `identity` for this bundle — the insert whose conflict IS the refusal.
 *
 * One statement, and it cannot raise: a free name inserts; a name THIS bundle already holds
 * updates itself (re-publishing the same server is not a collision with itself); a name ANOTHER
 * bundle holds matches the conflict target but fails the `WHERE`, so the `DO UPDATE` is skipped
 * and nothing comes back. That last case must not raise, because it is an ANSWER the caller
 * words for a person — a unique-violation would poison the caller's transaction instead, taking
 * the whole ceremony down with what is really a routine refusal.
 *
 * A concurrent claimer is serialized by the index itself: the second insert waits on the first's
 * row lock, then sees the committed row and takes the refusal branch.
 *
 * On success the bundle's OTHER identity rows are dropped — a re-publish that RENAMES the server
 * releases the old name in the same breath it takes the new one. On refusal nothing is written
 * at all, so a caller that returns the refusal may safely commit.
 */
export async function claimBundleIdentityInTx(
  tx: Tx,
  workspaceId: string,
  bundleId: string,
  kind: string,
  identity: string,
): Promise<{ ok: true } | { ok: false; heldBy: IdentityHolder | null }> {
  const claimed = await tx.execute(sql`
    INSERT INTO web.bundle_identity (workspace_id, bundle_id, kind, identity)
    VALUES (${workspaceId}, ${bundleId}, ${kind}, ${identity})
    ON CONFLICT (workspace_id, kind, identity) DO UPDATE
      SET bundle_id = excluded.bundle_id
      WHERE web.bundle_identity.bundle_id = excluded.bundle_id
    RETURNING bundle_id
  `);
  if (claimed.rows.length === 0) {
    return { ok: false, heldBy: await identityHolder(workspaceId, kind, identity, bundleId, tx) };
  }
  // The rename case: this bundle now claims exactly one name in this kind's namespace.
  await tx.execute(sql`
    DELETE FROM web.bundle_identity
    WHERE workspace_id = ${workspaceId} AND bundle_id = ${bundleId}
      AND NOT (kind = ${kind} AND identity = ${identity})
  `);
  return { ok: true };
}

/**
 * RELEASE every identity this bundle holds. Archiving frees the name (the archive rename frees
 * the catalog name for the same reason), and deleting frees it for good. Called inside the
 * ceremony's own transaction, so the name is free exactly when the status says it is.
 */
export async function releaseBundleIdentityInTx(
  tx: Tx,
  workspaceId: string,
  bundleId: string,
): Promise<void> {
  await tx.execute(sql`
    DELETE FROM web.bundle_identity
    WHERE workspace_id = ${workspaceId} AND bundle_id = ${bundleId}
  `);
}

/**
 * Who holds `identity` in this workspace, ignoring `exceptBundleId` (the bundle asking about its
 * own name). `null` is free.
 *
 * This is the PRE-CHECK the publish doors run before any custody call, so a refused publish
 * leaves no ingested bytes behind — and, being a read, it is never the last word: the claim
 * above is. It is also how a refusal learns the catalog NAME to point a reader at.
 */
export async function identityHolder(
  workspaceId: string,
  kind: string,
  identity: string,
  exceptBundleId: string | null,
  db: IdentityDbClient = getDb(),
): Promise<IdentityHolder | null> {
  const rows = await db.execute(sql`
    SELECT i.bundle_id, b.name
    FROM web.bundle_identity i
    JOIN web.bundle b ON b.id = i.bundle_id AND b.workspace_id = i.workspace_id
    WHERE i.workspace_id = ${workspaceId} AND i.kind = ${kind} AND i.identity = ${identity}
      AND i.bundle_id IS DISTINCT FROM ${exceptBundleId}
    LIMIT 1
  `);
  const row = rows.rows[0] as { bundle_id: string; name: string } | undefined;
  return row === undefined ? null : { bundleId: row.bundle_id, name: row.name };
}

/** Every identity this workspace's bundles hold, by bundle id — one indexed read, no vault. */
export async function identitiesByBundle(
  workspaceId: string,
  kind: string,
): Promise<Map<string, string>> {
  const rows = await getDb().execute(sql`
    SELECT bundle_id, identity FROM web.bundle_identity
    WHERE workspace_id = ${workspaceId} AND kind = ${kind}
  `);
  return new Map(
    (rows.rows as { bundle_id: string; identity: string }[]).map((r) => [r.bundle_id, r.identity]),
  );
}

/** What one backfill pass did — the operator's account of it. */
export interface IdentityBackfillReport {
  claimed: number;
  /** Bundles whose current document this tier could not read, named so an operator can look. */
  unreadable: string[];
  /** Bundles whose name was already held by another bundle — a duplicate that predates the key. */
  contested: string[];
}

/**
 * THE ONE-TIME BACKFILL, run at boot right after the migrations.
 *
 * It is not part of the migration, and cannot be: the names being recorded live in `server.json`
 * inside the vault's object store, which holds bytes behind an HTTP lane — a SQL migration has
 * no way to read one. So the step that needs bytes runs where bytes are reachable, immediately
 * after the DDL lands and BEFORE the first request, which is the ordering that matters: no
 * publish may claim a name that an already-published server still serves.
 *
 * Idempotent and self-healing by construction. It considers only bundles that have a `current`
 * and no row yet, so a settled database does one indexed query and stops. A bundle whose
 * document cannot be read right now gets NO ROW and is NAMED in the report — never a silent
 * skip — and is reconsidered on the next boot, because unreadable today (a vault still starting
 * up) is not unreadable forever. It never throws: a vault that is down must not stop the app
 * from booting, and the next boot picks the work up where this one left it.
 */
export async function backfillBundleIdentities(): Promise<IdentityBackfillReport> {
  const report: IdentityBackfillReport = { claimed: 0, unreadable: [], contested: [] };
  for (const record of BUNDLE_KINDS) {
    if (!record.hasContentGate) {
      continue;
    }
    // Active, published, unclaimed — the whole work list, in one indexed read.
    const pending = await getDb().execute(sql`
      SELECT b.workspace_id, b.id AS bundle_id, b.name, cp.version_id
      FROM web.bundle b
      JOIN plane.current_pointer cp
        ON cp.workspace_id = b.workspace_id AND cp.bundle_id = b.id
      WHERE b.kind = ${record.kind} AND b.status = 'active'
        AND NOT EXISTS (
          SELECT 1 FROM web.bundle_identity i
          WHERE i.workspace_id = b.workspace_id AND i.bundle_id = b.id
        )
      ORDER BY b.workspace_id, b.name
    `);
    for (const row of pending.rows as {
      workspace_id: string;
      bundle_id: string;
      name: string;
      version_id: string;
    }[]) {
      const document = await serverDocumentOf(row.workspace_id, row.bundle_id, row.version_id);
      const identity = typeof document?.name === "string" ? document.name : null;
      if (identity === null) {
        report.unreadable.push(`${row.workspace_id}/${row.name}`);
        continue;
      }
      const claimed = await getDb().transaction((tx) =>
        claimBundleIdentityInTx(tx, row.workspace_id, row.bundle_id, record.kind, identity),
      );
      if (claimed.ok) {
        report.claimed += 1;
      } else {
        // Two bundles were already serving one name when the key arrived. The first one read
        // keeps it; this one is named so an operator can settle it by hand. Nothing is rewritten
        // here — which of two live servers owns a name is not this step's call to make.
        report.contested.push(`${row.workspace_id}/${row.name}`);
      }
    }
  }
  return report;
}

/**
 * Run the backfill and SAY what it could not do. Postgres' own `RAISE NOTICE` would be the
 * natural channel for a migration step, but nothing in this tier listens for notices, so a
 * notice is exactly the silent skip this must not be — the process log is the channel an
 * operator actually has.
 *
 * SILENT WHEN IT WORKED, loud and naming names when a bundle was left out: recording what a
 * server already calls itself is bookkeeping nobody needs told, while a name that went
 * unrecorded is a name another bundle can still take, which somebody does.
 */
export async function backfillBundleIdentitiesAtBoot(): Promise<void> {
  let report: IdentityBackfillReport;
  try {
    report = await backfillBundleIdentities();
  } catch (error) {
    console.warn(
      "[bundle-identity] backfill did not complete; it will run again on the next boot:",
      error instanceof Error ? error.message : error,
    );
    return;
  }
  for (const name of report.unreadable) {
    console.warn(
      `[bundle-identity] ${name}: its published server.json could not be read here, so the ` +
        "registry name it serves is NOT recorded and another bundle could still claim it. " +
        "Retried on the next boot.",
    );
  }
  for (const name of report.contested) {
    console.warn(
      `[bundle-identity] ${name}: the registry name it serves is already recorded for another ` +
        "bundle in the same workspace. Both are live; settle it by re-publishing one under a " +
        "different name.",
    );
  }
}
