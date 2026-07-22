import { and, asc, eq, sql } from "drizzle-orm";
import type { MemberActor, OwnerActor } from "@/lib/auth/guards.server";
import { healDetachmentsInTx } from "@/lib/db/detach.server";
import { auditInTx } from "@/lib/db/identity.server";
import { getDb } from "@/lib/db/index.server";
import { bundle, bundleNameHint, channelBundle, notice, proposal } from "@/lib/db/schema.app";
import { deleteBundleBytes, purgeVersionBytes } from "@/lib/plane/custody.server";

/**
 * The bundle LIFECYCLE data access — the owner ceremonies (archive / unarchive / delete /
 * purge / rename) as app-tier row transactions over `web.bundle` (+ the name hints), PLUS the
 * custody byte calls where bytes actually drop (purge, delete). The OwnerActor brand is the
 * gate (the route re-guards as owner); every row write lands its audit in the same transaction.
 *
 * Lifecycle: active → archived → deleted. Archiving renames (`<name>-archived-<date>`,
 * counter on same-day repeats) FREEING the base name — id-keyed follows and references make a
 * reused name a NEW identity — records the pre-archive name so unarchive restores it EXACTLY
 * (no suffix parsing), unplaces the bundle from every channel, and auto-closes open proposals
 * with author notices. Deleting (archive-first) keeps the row as a tombstone so history
 * survives, and drops the bytes vault-side. Deletion cannot recall device copies — the fleet
 * page says so via the retained state rows.
 */

const BUNDLE_NAME = /^[a-z0-9][a-z0-9-]*$/;
const BUNDLE_NAME_MAX = 64;

/** One archived catalog row — the identity retired but still addressable in lifecycle. */
export interface ArchivedSkillRow {
  skillId: string;
  /** The current (archived) name — `<base>-archived-<date>`; the delete confirm types THIS. */
  name: string;
  /** The freed base name the bundle carried before archiving. */
  baseName: string | null;
  archivedAtMs: number | null;
}

/** The workspace's archived bundles, name order (member-visible; per-row acts re-guard). */
export async function archivedSkillsOf(actor: MemberActor): Promise<ArchivedSkillRow[]> {
  const rows = await getDb()
    .select({
      skillId: bundle.id,
      name: bundle.name,
      baseName: bundle.baseName,
      archivedAtMs: sql<string | null>`(extract(epoch from ${bundle.archivedAt}) * 1000)::bigint`,
    })
    .from(bundle)
    .where(and(eq(bundle.workspaceId, actor.workspaceId), eq(bundle.status, "archived")))
    .orderBy(asc(bundle.name));
  return rows.map((r) => ({
    ...r,
    archivedAtMs: r.archivedAtMs === null ? null : Number(r.archivedAtMs),
  }));
}

/** One archived row by its immutable id — the action's re-read (the typed-name anchor). */
export async function archivedSkillById(
  actor: MemberActor,
  bundleId: string,
): Promise<ArchivedSkillRow | undefined> {
  const rows = await getDb()
    .select({
      skillId: bundle.id,
      name: bundle.name,
      baseName: bundle.baseName,
      archivedAtMs: sql<string | null>`(extract(epoch from ${bundle.archivedAt}) * 1000)::bigint`,
    })
    .from(bundle)
    .where(
      and(
        eq(bundle.workspaceId, actor.workspaceId),
        eq(bundle.id, bundleId),
        eq(bundle.status, "archived"),
      ),
    )
    .limit(1);
  const row = rows[0];
  return row === undefined
    ? undefined
    : { ...row, archivedAtMs: row.archivedAtMs === null ? null : Number(row.archivedAtMs) };
}

type Tx = Parameters<Parameters<ReturnType<typeof getDb>["transaction"]>[0]>[0];

/**
 * Auto-close a bundle's OPEN proposals for a circumstantial cause (an archive, a purge) —
 * no human verdict: each closes as `withdrawn` (the row vocabulary's no-verdict terminal)
 * carrying the cause in `resolved_reason`, and the author gets a `proposal_closed` notice.
 */
async function closeOpenProposalsInTx(
  tx: Tx,
  ws: string,
  bundleId: string,
  actor: { userId: string; display: string },
  reason: string,
  onlyCandidate?: string,
): Promise<void> {
  const open = await tx
    .select({
      id: proposal.id,
      candidateVersionId: proposal.candidateVersionId,
      proposedBy: proposal.proposedBy,
    })
    .from(proposal)
    .where(
      and(
        eq(proposal.workspaceId, ws),
        eq(proposal.bundleId, bundleId),
        eq(proposal.status, "open"),
        ...(onlyCandidate === undefined ? [] : [eq(proposal.candidateVersionId, onlyCandidate)]),
      ),
    );
  for (const row of open) {
    await tx
      .update(proposal)
      .set({
        status: "withdrawn",
        resolvedBy: actor.userId,
        resolvedReason: reason,
        resolvedAt: new Date(),
      })
      .where(eq(proposal.id, row.id));
    if (row.proposedBy !== null) {
      await tx.insert(notice).values({
        userId: row.proposedBy,
        workspaceId: ws,
        kind: "proposal_closed",
        payload: {
          skill_id: bundleId,
          version_id: row.candidateVersionId,
          actor: actor.display,
          outcome: "closed",
          reason,
        },
      });
    }
  }
}

export type ArchiveOutcome =
  | { outcome: "archived"; archivedName: string }
  | { outcome: "not_active" }
  | { outcome: "unknown_skill" };

/** Archive — out of circulation, not out of history. */
export async function archiveBundle(actor: OwnerActor, bundleId: string): Promise<ArchiveOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ name: bundle.name, status: bundle.status })
      .from(bundle)
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)))
      .limit(1);
    const row = rows[0];
    if (row === undefined) {
      return { outcome: "unknown_skill" } as const;
    }
    if (row.status !== "active") {
      return { outcome: "not_active" } as const;
    }
    const date = new Date().toISOString().slice(0, 10);
    let archivedName = `${row.name}-archived-${date}`;
    for (let counter = 2; ; counter++) {
      const taken = await tx
        .select({ id: bundle.id })
        .from(bundle)
        .where(and(eq(bundle.workspaceId, ws), eq(bundle.name, archivedName)))
        .limit(1);
      if (taken.length === 0) {
        break;
      }
      archivedName = `${row.name}-archived-${date}-${counter}`;
    }
    await tx
      .update(bundle)
      .set({ status: "archived", name: archivedName, baseName: row.name, archivedAt: new Date() })
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)));
    // Archive UNPLACES: the bundle leaves every channel (an upstream withdrawal, no detach).
    await tx
      .delete(channelBundle)
      .where(and(eq(channelBundle.workspaceId, ws), eq(channelBundle.bundleId, bundleId)));
    await closeOpenProposalsInTx(
      tx,
      ws,
      bundleId,
      { userId: actor.userId, display: actor.display },
      "skill archived",
    );
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "skill_archived",
      subject: bundleId,
      outcome: "ok",
      details: { from: row.name, to: archivedName },
    });
    return { outcome: "archived", archivedName } as const;
  });
}

export type UnarchiveOutcome =
  | { outcome: "unarchived"; name: string }
  | { outcome: "name_taken" }
  | { outcome: "not_archived" }
  | { outcome: "unknown_skill" };

/** Unarchive — renames back if the base name is still free, else a typed refusal. Channel
 * placements are NOT restored (curation moved on); re-entitled detach records heal. */
export async function unarchiveBundle(
  actor: OwnerActor,
  bundleId: string,
): Promise<UnarchiveOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ status: bundle.status, baseName: bundle.baseName })
      .from(bundle)
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)))
      .limit(1);
    const row = rows[0];
    if (row === undefined) {
      return { outcome: "unknown_skill" } as const;
    }
    if (row.status !== "archived" || row.baseName === null) {
      return { outcome: "not_archived" } as const;
    }
    const taken = await tx
      .select({ id: bundle.id })
      .from(bundle)
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.name, row.baseName)))
      .limit(1);
    if (taken.length > 0) {
      return { outcome: "name_taken" } as const;
    }
    await tx
      .update(bundle)
      .set({ status: "active", name: row.baseName, baseName: null, archivedAt: null })
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)));
    await healDetachmentsInTx(tx, ws, bundleId);
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "skill_unarchived",
      subject: bundleId,
      outcome: "ok",
      details: { name: row.baseName },
    });
    return { outcome: "unarchived", name: row.baseName } as const;
  });
}

export type DeleteBundleOutcome =
  | { outcome: "deleted"; bytesDropped: boolean }
  | { outcome: "not_archived" }
  | { outcome: "unknown_skill" };

/**
 * Delete — archive-first required; the row becomes a tombstone under its archived name (the
 * base name stays free), then the vault drops the bundle's whole custody. A custody fault
 * leaves the tombstone standing and answers `bytesDropped: false` honestly (the operator can
 * re-run; the row op is idempotent-safe).
 */
export async function deleteBundle(
  actor: OwnerActor,
  bundleId: string,
): Promise<DeleteBundleOutcome> {
  const ws = actor.workspaceId;
  const tombstoned = await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ status: bundle.status })
      .from(bundle)
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)))
      .limit(1);
    const row = rows[0];
    if (row === undefined) {
      return "unknown_skill" as const;
    }
    if (row.status !== "archived") {
      return "not_archived" as const;
    }
    await tx
      .update(bundle)
      .set({ status: "deleted", deletedAt: new Date() })
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)));
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "skill_deleted",
      subject: bundleId,
      outcome: "ok",
    });
    return "deleted" as const;
  });
  if (tombstoned !== "deleted") {
    return { outcome: tombstoned };
  }
  // The byte half runs AFTER the row commit: a vault fault must not resurrect the identity.
  const bytesDropped = await deleteBundleBytes(ws, bundleId);
  return { outcome: "deleted", bytesDropped };
}

export type PurgeOutcome =
  | { outcome: "purged" }
  | { outcome: "is_current" }
  | { outcome: "unknown_version" }
  | { outcome: "fault" };

/**
 * Byte-purge ONE version — the leak tool: the vault tombstones the bytes (refusing typed while
 * the version is pointed-at); this tier then auto-closes any open proposal carrying the purged
 * candidate, with author notices. The hash stays in history with who/when (the audit row);
 * re-purging is idempotent vault-side.
 */
export async function purgeVersion(
  actor: OwnerActor,
  bundleId: string,
  versionId: string,
): Promise<PurgeOutcome> {
  const ws = actor.workspaceId;
  const custody = await purgeVersionBytes(ws, bundleId, versionId, actor.display);
  if (custody.kind === "pointed_at") {
    return { outcome: "is_current" };
  }
  if (custody.kind === "not_found") {
    return { outcome: "unknown_version" };
  }
  if (custody.kind !== "ok") {
    return { outcome: "fault" };
  }
  await getDb().transaction(async (tx) => {
    await closeOpenProposalsInTx(
      tx,
      ws,
      bundleId,
      { userId: actor.userId, display: actor.display },
      "a version it rests on was purged",
      versionId,
    );
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "version_purged",
      subject: bundleId,
      outcome: "ok",
      details: { versionId },
    });
  });
  return { outcome: "purged" };
}

export type ProtectionOutcome = { outcome: "set" } | { outcome: "unknown_skill" };

/**
 * Pin (or unpin) ONE bundle's protection: 'open'/'reviewed' overrides the workspace default,
 * null returns the bundle to inheriting it. The OwnerActor brand is the gate (the route re-guards
 * as owner); the publish gate and the review four-eyes check both read the resolved cascade.
 */
export async function setBundleProtection(
  actor: OwnerActor,
  bundleId: string,
  protection: "open" | "reviewed" | null,
): Promise<ProtectionOutcome> {
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const rows = await tx
      .update(bundle)
      .set({ protection })
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)))
      .returning({ id: bundle.id });
    if (rows.length === 0) {
      return { outcome: "unknown_skill" } as const;
    }
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "skill_protection",
      subject: bundleId,
      outcome: "ok",
      details: { protection: protection ?? "inherit" },
    });
    return { outcome: "set" } as const;
  });
}

export type RenameOutcome =
  | { outcome: "renamed"; name: string }
  | { outcome: "name_taken" }
  | { outcome: "bad_name" }
  | { outcome: "not_active" }
  | { outcome: "unknown_skill" };

/**
 * Rename an ACTIVE bundle (id-keyed, so a concurrent rename can never retarget the act). The
 * old name becomes a resolving hint (latest rename wins the hint slot); any hint squatting the
 * NEW name is cleared. The `-archived-` pattern is refused — that namespace belongs to the
 * archive rename.
 */
export async function renameBundle(
  actor: OwnerActor,
  bundleId: string,
  newName: string,
): Promise<RenameOutcome> {
  if (
    !BUNDLE_NAME.test(newName) ||
    newName.length > BUNDLE_NAME_MAX ||
    newName.includes("-archived-")
  ) {
    return { outcome: "bad_name" };
  }
  const ws = actor.workspaceId;
  return await getDb().transaction(async (tx) => {
    const rows = await tx
      .select({ name: bundle.name, status: bundle.status })
      .from(bundle)
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)))
      .limit(1);
    const row = rows[0];
    if (row === undefined) {
      return { outcome: "unknown_skill" } as const;
    }
    if (row.status !== "active") {
      return { outcome: "not_active" } as const;
    }
    if (row.name === newName) {
      return { outcome: "renamed", name: newName } as const;
    }
    const taken = await tx
      .select({ id: bundle.id })
      .from(bundle)
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.name, newName)))
      .limit(1);
    if (taken.length > 0) {
      return { outcome: "name_taken" } as const;
    }
    // The old name keeps resolving: latest rename wins the hint slot.
    await tx
      .insert(bundleNameHint)
      .values({ workspaceId: ws, oldName: row.name, bundleId, renamedBy: actor.userId })
      .onConflictDoUpdate({
        target: [bundleNameHint.workspaceId, bundleNameHint.oldName],
        set: { bundleId, renamedBy: actor.userId, renamedAt: new Date() },
      });
    await tx
      .delete(bundleNameHint)
      .where(and(eq(bundleNameHint.workspaceId, ws), eq(bundleNameHint.oldName, newName)));
    await tx
      .update(bundle)
      .set({ name: newName })
      .where(and(eq(bundle.workspaceId, ws), eq(bundle.id, bundleId)));
    await auditInTx(tx, {
      workspaceId: ws,
      actor: { userId: actor.userId, display: actor.display },
      kind: "skill_renamed",
      subject: bundleId,
      outcome: "ok",
      details: { from: row.name, to: newName },
    });
    return { outcome: "renamed", name: newName } as const;
  });
}
