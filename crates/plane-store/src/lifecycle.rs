//! The object-lifecycle orchestration — quarantine ingest + lease-before-migrate-into-git, built over the
//! DB transitions (`mod sqlite`) and the dumb git fence primitives (`topos-gitstore`). These are the
//! directly-testable `pub(crate)` ops the fence is exercised through; the legacy `upload_candidate` write
//! path is left untouched (so GC, which acts only on objects with an `object_presence` row, never reclaims
//! a legacy straight-to-git blob).
//!
//! Steps map to the crash-safe publication protocol: **A/B (ingest)** open a GC-excluded quarantine and
//! stage + rehash + denylist-check the candidate; **D (migrate)** lease the commit's FULL object set, then
//! install each not-already-present object durably (the DB decides reuse; bytes reach `present` only after
//! the durable install), then record the version. The pointer-move that consumes the lease is a later step.
//! `migrate` is split into `lease` / `install` / `finish` so a test can interleave a GC between them
//! deterministically (no timing).

use std::path::PathBuf;
use std::time::Duration;

use topos_core::sign::{self, Commit};
use topos_gitstore::{GitstoreError, ImportFile, StagedEntry, Store};

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, OpId, WorkspaceId};
use crate::sqlite::{InstallOutcome, ObjectStatus};
use crate::upload::CandidateUpload;

/// How long an in-flight quarantine lives before the janitor may sweep it. Generous: in-process
/// ingest→migrate is sub-second, and a slow client can re-ingest under a fresh op id.
const QUARANTINE_TTL_SECS: i64 = 3600;

/// How long an in-flight promotion lease lives before GC may treat it as a crashed/abandoned migrate. A
/// SUCCESSFUL migrate makes its lease non-expiring (the version stays rooted until the later pointer-move).
const LEASE_TTL_SECS: i64 = 600;

/// The deleting-wait backoff (it polls OUTSIDE any write transaction — holding one would deadlock GC's
/// finalize on SQLite's single writer). Bounded so a stranded `deleting` (a crashed GC the recovery sweep
/// has not yet finalized) fails the migrate cleanly rather than hanging forever.
const WAIT_BACKOFF_START: Duration = Duration::from_millis(5);
const WAIT_BACKOFF_CAP: Duration = Duration::from_millis(200);
const WAIT_MAX_POLLS: u32 = 200;

/// A candidate staged into its quarantine, ready to migrate. Carries the gitstore staging result + the
/// recomputed identity; the `op_id` ties it to its quarantine + lease.
#[derive(Debug, Clone)]
pub(crate) struct StagedCandidate {
    pub op_id: OpId,
    pub quarantine_dir: PathBuf,
    pub version_id: CommitId,
    pub bundle_digest: [u8; 32],
    pub entries: Vec<StagedEntry>,
    pub parents: Vec<CommitId>,
    pub author: String,
    pub message: String,
}

/// Step A + B: open a GC-excluded quarantine, stage the candidate's full tree into it (server rehash), and
/// reject any blob on the denylist (a best-effort early guard; the serializing check is the install CAS).
/// Recomputes the `version_id` from the rehashed bytes — a client id is never trusted.
pub(crate) async fn ingest(
    authority: &Authority,
    ws: &WorkspaceId,
    op_id: &OpId,
    candidate: CandidateUpload,
    now: i64,
) -> Result<StagedCandidate> {
    if candidate.files.is_empty() {
        return Err(AuthorityError::RejectedUpload(
            "a skill bundle must contain at least one file".to_owned(),
        ));
    }
    // Reject a denylisted candidate blob BEFORE staging, so purged bytes are never persisted to disk (the
    // object id is `sha256(bytes)`, exactly what `Store::stage` recomputes, so this needs no staging). A
    // best-effort early check; the serializing check is the install CAS.
    for f in &candidate.files {
        let oid = ObjectId(topos_core::digest::sha256(&f.bytes));
        if authority.db().is_tombstoned(ws, oid).await? {
            return Err(AuthorityError::RejectedUpload(
                "a candidate blob is on the denylist".to_owned(),
            ));
        }
    }

    let quarantine_dir = authority.workspace_quarantine_dir(ws, op_id);

    // Record the quarantine objdir (GC-excluded) before staging, so a crash mid-stage leaves a janitor-able
    // row. The stored objdir is reference metadata only — the janitor rebuilds the path from the ids.
    authority
        .db()
        .insert_quarantine(
            ws,
            op_id,
            &quarantine_dir.to_string_lossy(),
            now + QUARANTINE_TTL_SECS,
        )
        .await?;

    let import: Vec<ImportFile<'_>> = candidate
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let staged = Store::stage(&quarantine_dir, &import).map_err(map_stage_reject)?;

    let parents: Vec<[u8; 32]> = candidate.parents.iter().map(|c| c.0).collect();
    let version_id = sign::commit_id(&Commit {
        parents: &parents,
        tree: staged.bundle_digest,
        author: &candidate.author,
        message: &candidate.message,
    })
    .map_err(|e| AuthorityError::RejectedUpload(format!("invalid commit frame: {e:?}")))?;

    Ok(StagedCandidate {
        op_id: op_id.clone(),
        quarantine_dir,
        version_id: CommitId(version_id),
        bundle_digest: staged.bundle_digest,
        entries: staged.entries,
        parents: candidate.parents,
        author: candidate.author,
        message: candidate.message,
    })
}

/// Step D, part 1: insert the promotion lease over the commit's FULL distinct object set BEFORE any byte
/// migrates — so a concurrent GC's keep-set already protects every needed object (including an old,
/// already-present one a dedup-skip would otherwise leave exposed: the dedup race).
pub(crate) async fn migrate_lease(
    authority: &Authority,
    ws: &WorkspaceId,
    staged: &StagedCandidate,
    now: i64,
) -> Result<()> {
    let objects = distinct_object_ids(&staged.entries);
    authority
        .db()
        .insert_lease(
            ws,
            &staged.op_id,
            staged.version_id,
            &objects,
            now + LEASE_TTL_SECS,
        )
        .await
}

/// Step D, part 2: install every not-already-present object into the main store durably. The DB decides
/// reuse (`present` → skip); a `deleting` object is waited out (OUTSIDE any write transaction) then
/// re-copied fresh; bytes reach `present` only after the durable install. The lease (part 1) protects them
/// throughout.
pub(crate) async fn migrate_install(
    authority: &Authority,
    ws: &WorkspaceId,
    staged: &StagedCandidate,
    now: i64,
) -> Result<()> {
    let quarantine = Store::open(&staged.quarantine_dir).map_err(AuthorityError::internal)?;
    for entry in distinct_entries(&staged.entries) {
        install_one(authority, ws, &quarantine, entry, now).await?;
    }
    Ok(())
}

/// Step D, part 3: record the migrated version durably (build its tree from the installed blob ids — never
/// re-writing a blob outside the fence — write the commit + version ref + fsync), then make the lease
/// non-expiring (the version stays rooted until the later pointer-move) and drop the quarantine.
pub(crate) async fn migrate_finish(
    authority: &Authority,
    ws: &WorkspaceId,
    staged: &StagedCandidate,
    now: i64,
) -> Result<()> {
    let main = authority.store_for_write(ws)?;
    let entries: Vec<(&str, topos_core::digest::FileMode, [u8; 20])> = staged
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.mode, e.git_oid))
        .collect();
    let parents: Vec<[u8; 32]> = staged.parents.iter().map(|c| c.0).collect();
    main.commit_durable(
        staged.version_id.0,
        &parents,
        &entries,
        staged.bundle_digest,
        &staged.author,
        &staged.message,
    )
    .map_err(map_stage_reject)?;

    // Success: the lease becomes the durable root until the pointer-move consumes it. The CAS on the
    // staged commit + lease liveness fails closed if this migrate's lease lapsed (so it cannot claim a
    // success whose objects GC may already have reclaimed).
    authority
        .db()
        .commit_lease(ws, &staged.op_id, staged.version_id, now)
        .await?;
    // Post-commit cleanup (the objects are safely in the main store): remove the quarantine dir FIRST, then
    // drop its tracking row — and only if the removal succeeded. A failure/crash before the row is dropped
    // leaves it for the janitor to retry (the row is the only way to rebuild the rm -rf path); dropping the
    // row first would orphan the dir.
    if remove_quarantine_dir(&staged.quarantine_dir) {
        authority.db().delete_quarantine(ws, &staged.op_id).await?;
    }
    Ok(())
}

/// Remove a quarantine dir, treating "already gone" as success. Returns whether the dir is now gone (so the
/// caller may safely drop its tracking row); any other error leaves it for the janitor to retry.
pub(crate) fn remove_quarantine_dir(dir: &std::path::Path) -> bool {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// The full migrate (lease → install → finish). `migrate_finish` is given a finish time advanced by the
/// REAL install duration, not the lease-start `now` — so if installation ran past the lease TTL (e.g. many
/// `deleting`-waits), `commit_lease`'s liveness CAS sees the lease expired and fails closed instead of
/// committing a version whose objects a concurrent GC may already have reclaimed. (The decomposed steps
/// take explicit per-step `now`s; a production caller that drives them passes a fresh time to each.)
pub(crate) async fn migrate(
    authority: &Authority,
    ws: &WorkspaceId,
    staged: &StagedCandidate,
    now: i64,
) -> Result<()> {
    let started = tokio::time::Instant::now();
    migrate_lease(authority, ws, staged, now).await?;
    migrate_install(authority, ws, staged, now).await?;
    // The install genuinely took real wall-clock time (its `deleting`-waits sleep); advance the finish
    // clock by that elapsed time so the lease-liveness CAS is meaningful. In the fast path (and in tests)
    // this is ~0, so finish ≈ now. (Sampled before `migrate_finish`'s `commit_durable`, which is fast git
    // I/O; a pathological multi-minute fsync stall there is the one window this approximation misses — and
    // it cannot corrupt bytes, only over-commit a lapsed lease, which the deferred pointer-move that
    // consumes the lease re-verifies for renderability before trusting.)
    let finish_now = now.saturating_add(started.elapsed().as_secs() as i64);
    migrate_finish(authority, ws, staged, finish_now).await
}

/// Install one object, honoring the fence: reuse if `present`; wait out `deleting` (no transaction held
/// across the sleep, so GC's finalize can never be blocked) then re-copy; reject if `unavailable`.
async fn install_one(
    authority: &Authority,
    ws: &WorkspaceId,
    quarantine: &Store,
    entry: &StagedEntry,
    now: i64,
) -> Result<()> {
    let object_id = ObjectId(entry.object_id);
    let mut backoff = WAIT_BACKOFF_START;
    for _ in 0..WAIT_MAX_POLLS {
        match authority.db().object_status(ws, object_id).await? {
            ObjectStatus::Present => return Ok(()), // dedup: reuse the already-present bytes
            ObjectStatus::Unavailable => {
                return Err(AuthorityError::RejectedUpload(
                    "a candidate blob is on the denylist".to_owned(),
                ));
            }
            ObjectStatus::Deleting => {
                // A GC is unlinking these bytes — wait for `absent` (poll on the pool; no txn held), then
                // re-copy fresh. NEVER override `deleting` (the non-resurrectable fence).
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(WAIT_BACKOFF_CAP);
                continue;
            }
            ObjectStatus::Absent => {
                // Open a FRESH main handle so a just-deleted object's bytes are actually re-written (gix's
                // object cache can otherwise make `write_blob` skip a loose object it believes still exists).
                let main = authority.store_for_write(ws)?;
                main.install_object_durable(quarantine, entry.git_oid)
                    .map_err(AuthorityError::internal)?;
                match authority
                    .db()
                    .install_object(ws, object_id, &entry.git_oid, entry.size as i64, now)
                    .await?
                {
                    InstallOutcome::Installed | InstallOutcome::AlreadyPresent => return Ok(()),
                    // A GC claimed it between the status read and the CAS — wait it out and retry.
                    InstallOutcome::Deleting => {
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(WAIT_BACKOFF_CAP);
                        continue;
                    }
                    InstallOutcome::Unavailable => {
                        return Err(AuthorityError::RejectedUpload(
                            "a candidate blob is on the denylist".to_owned(),
                        ));
                    }
                }
            }
        }
    }
    // The object stayed `deleting` past the bound — a crashed GC the recovery sweep has not finalized. A
    // retry (after recovery) succeeds; surface it as a transient internal fault.
    Err(AuthorityError::internal(DeletingWaitTimedOut))
}

/// The distinct object ids of a staged bundle's entries (a blob at two paths is one object).
fn distinct_object_ids(entries: &[StagedEntry]) -> Vec<ObjectId> {
    let mut seen = std::collections::BTreeSet::new();
    entries
        .iter()
        .filter_map(|e| seen.insert(e.object_id).then_some(ObjectId(e.object_id)))
        .collect()
}

/// One staged entry per distinct object id (for installation; the tree build uses all entries/paths).
fn distinct_entries(entries: &[StagedEntry]) -> Vec<&StagedEntry> {
    let mut seen = std::collections::BTreeSet::new();
    entries
        .iter()
        .filter(|e| seen.insert(e.object_id))
        .collect()
}

/// Map a gitstore stage/commit failure to the boundary error: a canonical-rule reject / missing parent /
/// id mismatch is the client's problem; a low-level fault is internal.
fn map_stage_reject(e: GitstoreError) -> AuthorityError {
    match e {
        GitstoreError::Reject(reason) => {
            AuthorityError::RejectedUpload(format!("canonical rule violated: {reason:?}"))
        }
        GitstoreError::MissingParent => AuthorityError::RejectedUpload(
            "a parent version is not present in this workspace".into(),
        ),
        GitstoreError::VersionMismatch => {
            AuthorityError::RejectedUpload("the commit id does not match the staged bytes".into())
        }
        other => AuthorityError::internal(other),
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "an object stayed in the deleting state past the wait bound (a crashed GC awaiting recovery)"
)]
struct DeletingWaitTimedOut;
