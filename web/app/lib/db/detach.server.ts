import { sql } from "drizzle-orm";
import { entitledBundlesSql } from "@/lib/db/identity.server";
import type { Db } from "@/lib/db/index.server";

/**
 * The lapse-detach / re-attach reconciles — the who-acts bookkeeping behind every
 * entitlement-losing PERSON event (unfollow, a channel leave, the default-channel opt-out,
 * seat removal) and its inverse. A detach record means "this person's own act stopped
 * delivering this bundle to them — every device freezes the copy in place"; it is a RECORD of
 * a lapse, never a mask: any re-entitlement (a follow, a join, a curator re-placing) clears it
 * (entitlement always wins over a stale record).
 *
 * Every helper runs INSIDE the caller's transaction, so the snapshot the lapse is computed
 * from and the mutation it reconciles commit together. Records are written only for bundles
 * holding a `current` pointer — a never-published bundle has nothing on any device to freeze.
 */

type Tx = Parameters<Parameters<Db["transaction"]>[0]>[0];

export type DetachCause = "unfollow" | "channel_leave" | "membership_removed";

/**
 * A Postgres text[] literal for a bound parameter — the driver does not serialize a plain JS
 * array through drizzle's sql template, so array params bind as one quoted literal.
 */
export function pgTextArray(values: string[]): string {
  return `{${values.map((v) => `"${v.replaceAll('"', "")}"`).join(",")}}`;
}

/** The person's currently-entitled bundle ids — the before/after snapshot both sides take. */
export async function entitledIdsInTx(tx: Tx, ws: string, userId: string): Promise<string[]> {
  const rows = await tx.execute(sql`SELECT bundle_id FROM (${entitledBundlesSql(userId, ws)}) e`);
  return (rows.rows as { bundle_id: string }[]).map((r) => r.bundle_id).sort();
}

/**
 * Write detach records for EXACTLY the bundles one event lapsed (the caller's before − after),
 * so an unrelated bundle the person still receives — or one an UPSTREAM act removed — is never
 * mislabelled a person detach. Already-detached rows keep their original cause.
 */
export async function detachExactInTx(
  tx: Tx,
  ws: string,
  userId: string,
  lapsed: string[],
  cause: DetachCause,
): Promise<void> {
  if (lapsed.length === 0) {
    return;
  }
  await tx.execute(sql`
    INSERT INTO web.bundle_detachment (user_id, workspace_id, bundle_id, cause)
    SELECT ${userId}, ${ws}, s, ${cause} FROM unnest(${pgTextArray(lapsed)}::text[]) AS s
    WHERE EXISTS (
      SELECT 1 FROM plane.current_pointer cp
      WHERE cp.workspace_id = ${ws} AND cp.bundle_id = s
    )
    ON CONFLICT (user_id, bundle_id) DO NOTHING
  `);
}

/**
 * RE-ATTACH — the self-heal: drop this person's detach records for any bundle they are
 * entitled to again (their own follow/join, but equally a curator re-placing it or the
 * default-channel opt-in). No record can strand a live subscription.
 */
export async function reattachInTx(tx: Tx, ws: string, userId: string): Promise<void> {
  await tx.execute(sql`
    DELETE FROM web.bundle_detachment d
    WHERE d.workspace_id = ${ws} AND d.user_id = ${userId}
      AND d.bundle_id IN (${entitledBundlesSql(userId, ws)})
  `);
}

/**
 * The bundle-scoped self-heal — the inverse of a lapse for ONE bundle, across everyone who had
 * detached it: any person now entitled again (a curator re-placed it, an owner unarchived it)
 * has the record dropped. Bounded by the people who had actually detached THIS bundle.
 */
export async function healDetachmentsInTx(tx: Tx, ws: string, bundleId: string): Promise<void> {
  const rows = await tx.execute(sql`
    SELECT user_id FROM web.bundle_detachment
    WHERE workspace_id = ${ws} AND bundle_id = ${bundleId}
  `);
  for (const raw of rows.rows as { user_id: string }[]) {
    await tx.execute(sql`
      DELETE FROM web.bundle_detachment d
      WHERE d.workspace_id = ${ws} AND d.user_id = ${raw.user_id} AND d.bundle_id = ${bundleId}
        AND EXISTS (
          SELECT 1 FROM (${entitledBundlesSql(raw.user_id, ws)}) e WHERE e.bundle_id = ${bundleId}
        )
    `);
  }
}
