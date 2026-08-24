//! The write orchestration — commit / publish / pointer move / revert / purge / the reclaims.
//!
//! Every byte-introducing write is the same three-phase story: **ingest** (ephemeral staging +
//! server rehash), **migrate** (lease → store put → skeleton put), then **one serializable
//! transaction** that records the version rows — including the git commit locator + message —
//! and, when asked, runs the generation-fenced pointer CAS. The candidate's committed lease is
//! released only after that transaction resolves, so the GC keep-set covers the objects
//! continuously across the lease→edge handoff. A refused transaction (CONFLICT, a lineage
//! violation) rolls back its version rows; the released lease then lets ordinary GC reclaim the
//! unique staged bytes (the orphan skeleton objects are accepted debris, reclaimed with the
//! workspace).
//!
//! Idempotency is CONTENT-shaped, not receipt-shaped: an identical candidate re-committed converges
//! on the same version row (`deduped`), and every pointer mover honors the idempotent-CAS rule (see
//! `db/custody/pointer.rs`), so an app-side retry after a crash re-lands on success without any
//! vault-side receipt machinery.

use crate::authority::Authority;
use crate::error::{AuthorityError, LivePointer, Result};
use crate::id::{BundleId, CommitId, ObjectId, OpId, WorkspaceId, validate_attribution};
use crate::lifecycle::{self, StagedCandidate};
use crate::upload::CandidateUpload;

use crate::db::custody::pointer::{PointerAction, VersionFacts};

/// A committed version's identity facts, as the ingest paths answer them.
#[derive(Debug, Clone)]
pub struct CommittedVersion {
    /// The version's id (= the kernel commit id over the candidate frame).
    pub version_id: CommitId,
    /// The byte-exact consent digest over the candidate's file tree.
    pub bundle_digest: [u8; 32],
    /// Whether the version row already existed (an identical candidate re-committed).
    pub deduped: bool,
}

/// The pointer state a successful move answers with.
#[derive(Debug, Clone)]
pub struct PointerState {
    /// The version the pointer now names.
    pub version_id: CommitId,
    /// The pointer's generation after the move.
    pub generation: u64,
    /// When the pointer last moved (epoch milliseconds).
    pub moved_at_ms: i64,
    /// The attribution recorded on the move.
    pub moved_by: String,
    /// Whether this request resolved through the idempotent-CAS carve-out (the exact move had
    /// already landed — an app-side retry after a crash).
    pub replayed: bool,
}

/// What a purge did: how many blobs were denylisted (unique to the purged version) and how many of
/// those had their bytes reclaimed inline.
#[derive(Debug, Clone, Copy)]
pub struct PurgeReport {
    pub tombstoned: usize,
    pub reclaimed: usize,
}

/// What a bundle deletion reclaimed.
#[derive(Debug, Clone, Copy)]
pub struct BundleDeleteReport {
    /// Version rows dropped.
    pub versions_dropped: u64,
    /// Objects reclaimed by the inline GC pass that followed the row drop.
    pub objects_reclaimed: usize,
}

/// Mint a fresh op id (16 random bytes as lowercase hex) — the staging/lease/audit key of one
/// ingest. Vault-minted, never caller-supplied, so an op id is never reused across requests.
fn mint_op_id() -> Result<OpId> {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).map_err(AuthorityError::internal)?;
    let mut s = String::with_capacity(32);
    for b in buf {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    OpId::parse(&s).map_err(AuthorityError::internal)
}

/// Ingest + commit a candidate WITHOUT moving the pointer (the propose path), or WITH the CAS (the
/// publish path) when `expected_generation` is `Some(action)`. The shared body of
/// [`Authority::commit_version`] and [`Authority::publish`].
async fn ingest_and_commit(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    candidate: CandidateUpload,
    action: PointerAction,
    now: i64,
) -> Result<(CommittedVersion, Option<PointerState>)> {
    validate_attribution(&candidate.attribution)?;
    let op_id = mint_op_id()?;

    // Ingest + migrate ALWAYS run in full — the install path dedups per object (and re-materializes
    // crash-lost bytes), and the commit transaction dedups the version row, so re-committing an
    // identical candidate is safe, idempotent, and self-healing.
    let staged = lifecycle::ingest(authority, ws, bundle, &op_id, candidate, now).await?;
    let git_commit_oid = match lifecycle::migrate(authority, ws, bundle, &staged, now).await {
        Ok(oid) => oid,
        Err(e) => {
            // A refused/failed migrate: the audit row goes terminal and the lease (if any) is released
            // so the staged bytes become ordinary garbage.
            let _ = authority.db().mark_upload_aborted(&op_id).await;
            let _ = authority.db().release_lease(ws, &op_id).await;
            return Err(e);
        }
    };

    let outcome = commit_staged(authority, ws, bundle, &staged, git_commit_oid, action, now).await;
    match outcome {
        Ok(txn) => {
            // The version rows are durable — they root the objects now; the lease may go.
            authority.db().release_lease(ws, &op_id).await?;
            authority.db().mark_upload_committed(&op_id).await?;
            Ok((
                CommittedVersion {
                    version_id: staged.version_id,
                    bundle_digest: staged.bundle_digest,
                    deduped: txn.deduped,
                },
                txn.pointer.map(|p| PointerState {
                    version_id: p.version_id,
                    generation: p.generation,
                    moved_at_ms: p.moved_at_ms,
                    moved_by: p.moved_by,
                    replayed: txn.replayed,
                }),
            ))
        }
        Err(e) => {
            // A refused commit rolled its rows back; release the lease so GC can reclaim the unique
            // staged bytes, and close the audit row.
            let _ = authority.db().mark_upload_aborted(&op_id).await;
            let _ = authority.db().release_lease(ws, &op_id).await;
            Err(e)
        }
    }
}

/// The one commit-transaction call, shared by the ingest paths and the revert.
async fn commit_staged(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    staged: &StagedCandidate,
    git_commit_oid: [u8; 20],
    action: PointerAction,
    now: i64,
) -> Result<crate::db::custody::pointer::CommitTxnOutcome> {
    let object_ids = lifecycle::distinct_object_ids(&staged.entries);
    let digest_hex = crate::id::hex32(&staged.bundle_digest);
    authority
        .db()
        .commit_version(
            ws,
            bundle,
            &VersionFacts {
                version_id: staged.version_id,
                parent: staged.parent,
                attribution: &staged.attribution,
                bundle_digest_hex: &digest_hex,
                object_ids: &object_ids,
                op_id: &staged.op_id,
                git_commit_oid: &git_commit_oid,
                message: &staged.message,
            },
            action,
            now,
        )
        .await
}

/// Ingest + commit WITHOUT moving the pointer — the propose path. Committing an identical candidate
/// twice returns the same ids (success, `deduped`).
pub(crate) async fn commit_version(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    candidate: CandidateUpload,
    now: i64,
) -> Result<CommittedVersion> {
    let (version, _) =
        ingest_and_commit(authority, ws, bundle, candidate, PointerAction::None, now).await?;
    Ok(version)
}

/// Ingest + commit + CAS pointer move, one flow — the direct publish path. `expected_generation`
/// `None` = genesis (creates the pointer at generation 1); `Some(g)` = the CAS, with the same-bundle
/// lineage fence (the candidate's first parent must be the currently pointed version).
pub(crate) async fn publish(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    candidate: CandidateUpload,
    expected_generation: Option<u64>,
    now: i64,
) -> Result<(CommittedVersion, PointerState)> {
    let (version, pointer) = ingest_and_commit(
        authority,
        ws,
        bundle,
        candidate,
        PointerAction::Cas(expected_generation),
        now,
    )
    .await?;
    let pointer = pointer.ok_or_else(|| AuthorityError::internal(MissingPointerState))?;
    Ok((version, pointer))
}

/// CAS the pointer to an EXISTING version (the approve path). No bytes move; the target must be a
/// live (un-purged) version of this bundle whose objects are all present.
pub(crate) async fn move_pointer(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    version: CommitId,
    expected_generation: Option<u64>,
    attribution: &str,
    now: i64,
) -> Result<PointerState> {
    validate_attribution(attribution)?;
    let txn = authority
        .db()
        .move_pointer(ws, bundle, version, expected_generation, attribution, now)
        .await?;
    let pointer = txn
        .pointer
        .ok_or_else(|| AuthorityError::internal(MissingPointerState))?;
    Ok(PointerState {
        version_id: pointer.version_id,
        generation: pointer.generation,
        moved_at_ms: pointer.moved_at_ms,
        moved_by: pointer.moved_by,
        replayed: txn.replayed,
    })
}

/// The revert: a FORWARD commit `{tree: target.tree, parents: [current]}` + the CAS — the pointer
/// never moves backward. A purged target is refused typed; a crashed caller's retry (the pointer
/// already advanced to a version carrying the target's exact bytes) answers success idempotently.
///
/// The forward commit REUSES the target's root tree object (the skeleton is content-addressed and
/// retained), so only one new commit object is put. Both locators — the target's tree and the
/// current version's commit — come from version rows; a pre-import row fails closed naming
/// `import-local`.
///
/// `message` is the CALLER's forward-commit message, recorded verbatim into the commit frame: the
/// public device wire lets a client pre-derive the forward commit id from `(parents, tree, author,
/// message)` and verify the move landed on exactly that version, so the frame's inputs must be the
/// wire's — a server-synthesized message would break that parity. Retries stay deterministic
/// because a replay re-sends the identical message.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn revert(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    to_version: CommitId,
    expected_generation: u64,
    attribution: &str,
    message: &str,
    now: i64,
) -> Result<(CommittedVersion, PointerState)> {
    validate_attribution(attribution)?;

    // The target must be a live version of this bundle — typed refusals BEFORE any staging.
    match authority
        .db()
        .read_version_row(ws, bundle, to_version)
        .await?
    {
        None => return Err(AuthorityError::NotFound),
        Some(row) if row.purged_at_ms.is_some() => return Err(AuthorityError::TargetPurged),
        Some(_) => {}
    }
    let target_digest = authority
        .db()
        .read_bundle_digest(ws, bundle, to_version)
        .await?
        .ok_or_else(|| AuthorityError::integrity(MissingVersionDigest))?;

    // The pre-stage pointer read: the parent of the forward commit is the version pointed at the
    // expected generation. A retry whose move already landed is answered here (the pointer sits one
    // past `expected` and carries the target's exact bytes) — nothing is staged twice.
    let Some(pointer) = authority.db().read_pointer(ws, bundle).await? else {
        return Err(AuthorityError::Conflict(None));
    };
    if pointer.generation == expected_generation + 1 {
        let pointed_digest = authority
            .db()
            .read_bundle_digest(ws, bundle, pointer.version_id)
            .await?
            .ok_or_else(|| AuthorityError::integrity(MissingVersionDigest))?;
        if pointed_digest == target_digest {
            return Ok((
                CommittedVersion {
                    version_id: pointer.version_id,
                    bundle_digest: target_digest,
                    deduped: true,
                },
                PointerState {
                    version_id: pointer.version_id,
                    generation: pointer.generation,
                    moved_at_ms: pointer.moved_at_ms,
                    moved_by: pointer.moved_by,
                    replayed: true,
                },
            ));
        }
    }
    if pointer.generation != expected_generation {
        return Err(AuthorityError::Conflict(Some(LivePointer {
            generation: pointer.generation,
            version_id: pointer.version_id,
        })));
    }
    let parent = pointer.version_id;

    // The forward commit's content: the TARGET's tree (its root tree OID, read from the target's
    // commit object), the object set from the reachability rows, the caller's message
    // (deterministic across retries — a replay re-sends the identical one), the caller's
    // attribution. Both locators come from version rows and fail closed on a pre-import NULL.
    let target_commit_oid = require_commit_oid(authority, ws, bundle, to_version).await?;
    let parent_commit_oid = require_commit_oid(authority, ws, bundle, parent).await?;
    let target_tree_oid = crate::read::read_commit(authority, ws, target_commit_oid)
        .await?
        .tree_oid;
    let object_ids: Vec<ObjectId> = authority
        .db()
        .version_objects(ws, bundle, to_version)
        .await?;

    let parents = [parent];
    let parent_ids: Vec<[u8; 32]> = parents.iter().map(|c| c.0).collect();
    let forward = lifecycle::frame_version_id(&parent_ids, target_digest, attribution, message)?;

    let op_id = mint_op_id()?;
    let forward_commit_oid = lifecycle::stage_forward_commit(
        authority,
        ws,
        &op_id,
        forward,
        target_tree_oid,
        &[parent_commit_oid],
        &parents,
        &object_ids,
        attribution,
        message,
        now,
    )
    .await?;

    let digest_hex = crate::id::hex32(&target_digest);
    let outcome = authority
        .db()
        .commit_version(
            ws,
            bundle,
            &VersionFacts {
                version_id: forward,
                parent: Some(parent),
                attribution,
                bundle_digest_hex: &digest_hex,
                object_ids: &object_ids,
                op_id: &op_id,
                git_commit_oid: &forward_commit_oid,
                message,
            },
            PointerAction::Cas(Some(expected_generation)),
            now,
        )
        .await;
    match outcome {
        Ok(txn) => {
            authority.db().release_lease(ws, &op_id).await?;
            let pointer = txn
                .pointer
                .ok_or_else(|| AuthorityError::internal(MissingPointerState))?;
            Ok((
                CommittedVersion {
                    version_id: forward,
                    bundle_digest: target_digest,
                    deduped: txn.deduped,
                },
                PointerState {
                    version_id: pointer.version_id,
                    generation: pointer.generation,
                    moved_at_ms: pointer.moved_at_ms,
                    moved_by: pointer.moved_by,
                    replayed: txn.replayed,
                },
            ))
        }
        Err(e) => {
            let _ = authority.db().release_lease(ws, &op_id).await;
            Err(e)
        }
    }
}

/// A version row's recorded git commit locator, failing closed: an unknown version is NotFound;
/// a pre-import row (NULL locator) names the cure.
async fn require_commit_oid(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    version: CommitId,
) -> Result<[u8; 20]> {
    match authority
        .db()
        .version_git_commit_oid(ws, bundle, version)
        .await?
    {
        None => Err(AuthorityError::NotFound),
        Some(None) => Err(AuthorityError::integrity(crate::lifecycle::PreImportRow)),
        Some(Some(oid)) => Ok(oid),
    }
}

/// The byte purge: refuse if pointed-at; tombstone the blobs unique to the version; stamp
/// `purged_at`; then reclaim the denylisted bytes inline through the ordinary three-step fence (they
/// are unrooted the moment the transaction commits). The version row — the hash, the attribution,
/// the timestamps — stays.
pub(crate) async fn purge_version(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    version: CommitId,
    attribution: &str,
    now: i64,
) -> Result<PurgeReport> {
    validate_attribution(attribution)?;
    let unique = authority
        .db()
        .purge_version(ws, bundle, version, attribution, now)
        .await?;

    // Targeted reclaim: the same acquire → single-attempt delete → finalize fence a GC pass runs,
    // driven over exactly the just-denylisted set. A blob a concurrent ingest holds under a live
    // lease is spared here and picked up by a later pass (the install CAS refuses the tombstoned
    // blob, so the lease lapses); an ambiguous delete leaves the row `deleting` for recovery.
    let started = tokio::time::Instant::now();
    let mut reclaimed = 0;
    for object_id in &unique {
        let acquire_now = now.saturating_add(lifecycle::elapsed_ms(started));
        let git_oid = match authority
            .db()
            .acquire_for_delete(ws, *object_id, acquire_now)
            .await?
        {
            crate::db::AcquireOutcome::Spared => continue,
            crate::db::AcquireOutcome::Acquired { git_oid } => git_oid,
        };
        if !crate::gc::delete_owned(authority, ws, *object_id, git_oid, acquire_now).await? {
            continue;
        }
        let finalize_now = now.saturating_add(lifecycle::elapsed_ms(started));
        authority
            .db()
            .finalize_delete(ws, *object_id, acquire_now, finalize_now)
            .await?;
        reclaimed += 1;
    }
    Ok(PurgeReport {
        tombstoned: unique.len(),
        reclaimed,
    })
}

/// Bundle GC on app instruction (the app already decided the deletion): drop every row of the
/// bundle, then run one GC pass so the newly-unrooted bytes are reclaimed inline. The store keeps
/// the (tiny) commit/tree skeleton objects of the deleted versions — unreadable without the
/// dropped rows, reclaimed with the workspace (exactly the retention the per-workspace repo had).
pub(crate) async fn delete_bundle(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    now: i64,
) -> Result<BundleDeleteReport> {
    let versions_dropped = authority.db().delete_bundle_rows(ws, bundle).await?;
    let objects_reclaimed = crate::gc::run_gc(authority, ws, now).await?;
    Ok(BundleDeleteReport {
        versions_dropped,
        objects_reclaimed,
    })
}

/// Workspace reclaim: drop every row of the workspace across all custody tables, then remove its
/// staging dirs and bulk-delete everything under its store prefix (blobs AND the skeleton
/// objects). Row-first, so a failed store deletion leaves only invisible orphan bytes (no row
/// reaches them) — and idempotent, so a re-run finishes the job.
pub(crate) async fn delete_workspace(authority: &Authority, ws: &WorkspaceId) -> Result<()> {
    authority.db().delete_workspace_rows(ws).await?;
    // The ephemeral staging dirs (local, whole-dir).
    let staging = authority.workspace_staging_root(ws);
    crate::authority::run_blocking(move || {
        match std::fs::remove_dir_all(&staging) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(AuthorityError::internal(e)),
        }
        Ok(())
    })
    .await?;
    // The store prefix — list + bulk-delete, the one sanctioned prefix operation.
    authority.store().delete_workspace_prefix(ws).await?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
#[error("a CAS-carrying commit transaction answered without a pointer state")]
struct MissingPointerState;

#[derive(Debug, thiserror::Error)]
#[error("a committed version has no recorded bundle digest")]
struct MissingVersionDigest;
