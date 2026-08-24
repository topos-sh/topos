//! The object-lifecycle orchestration — ephemeral staging + lease-before-migrate-into-the-store,
//! built over the DB transitions (`mod db`) and the object-store seam (`crate::store`). `ingest` +
//! `migrate` are the entry every byte-introducing write shares; every object that reaches the
//! store does so through this path, so it carries an `object_presence` row (the sole presence
//! authority GC acts over).
//!
//! Steps map to the crash-safe publication protocol: **A/B (ingest)** record the upload row and
//! stage + rehash + denylist-check the candidate into an EPHEMERAL per-op directory under the
//! staging root (no volume required — losing staging on a container replacement loses only
//! in-flight candidates, which the client's write-ahead log replays); **D (migrate)** lease the
//! candidate's FULL object set, then put each not-already-present object to the store as a zlib
//! loose blob (the DB decides reuse; bytes reach `present` only after the put returns), then put
//! the version SKELETON (tree + commit objects) and record the commit locator. The version-row
//! transaction that consumes the lease is a later step (`commit`); a crash between the skeleton
//! put and that transaction leaves orphan skeleton objects — ACCEPTED (immutable,
//! content-addressed, bytes-cheap; reclaimed with the workspace). `migrate` is split into `lease`
//! / `install` / `finish` so a test can interleave a GC between them deterministically (no timing).
//!
//! **Clock convention: one server-clock unit = one epoch MILLISECOND.** Every TTL constant and
//! elapsed-time computation here is millisecond-valued — a seconds-valued constant added to an
//! epoch-ms `now` would silently collapse the lease/quarantine fences a thousandfold.

use std::path::{Path, PathBuf};
use std::time::Duration;

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::{self, Commit};
use topos_gitstore::GitstoreError;
use topos_gitstore::codec;

use crate::authority::Authority;
use crate::db::{InstallOutcome, ObjectStatus};
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, CommitId, ObjectId, OpId, WorkspaceId};
use crate::upload::CandidateUpload;

/// The most files one candidate may carry. The per-blob reject cap bounds each file's SIZE; this
/// bounds their NUMBER, which is what actually bounds ingest cost — every file costs a denylist
/// probe, a staged file, an install CAS and a store put, all serialized under the promotion
/// lease. The client's own importer carries the same dimension (`git_source.rs`).
pub(crate) const MAX_CANDIDATE_FILES: usize = 20_000;

/// How long an in-flight staging dir lives before the janitor may sweep it (epoch-ms; one hour).
/// Generous: in-process ingest→commit is sub-second.
pub(crate) const QUARANTINE_TTL_MS: i64 = 3600 * 1000;

/// How long an in-flight promotion lease lives before GC may treat it as a crashed/abandoned ingest
/// (epoch-ms; ten minutes). A SUCCESSFUL migrate makes its lease non-expiring (the candidate stays rooted
/// until its version row lands).
pub(crate) const LEASE_TTL_MS: i64 = 600 * 1000;

/// The deleting-wait backoff (it polls OUTSIDE any write transaction — holding one would stall GC's
/// finalize write transaction). Bounded so a stranded `deleting` (a crashed GC the recovery sweep
/// has not yet finalized) fails the ingest cleanly rather than hanging forever.
const WAIT_BACKOFF_START: Duration = Duration::from_millis(5);
const WAIT_BACKOFF_CAP: Duration = Duration::from_millis(200);
const WAIT_MAX_POLLS: u32 = 200;

/// One file staged into the per-op staging dir: its bundle-relative path + mode, its topos
/// `object_id` (`sha256(raw bytes)`, the authority's identity), the git blob `git_oid` (the
/// physical store key), and size.
#[derive(Debug, Clone)]
pub(crate) struct StagedEntry {
    pub path: String,
    pub mode: FileMode,
    pub object_id: [u8; 32],
    pub git_oid: [u8; 20],
    pub size: u64,
}

/// A candidate staged into its per-op dir, ready to migrate. Carries the staging result + the
/// recomputed identity; the `op_id` ties it to its staging dir + lease + `upload` audit row.
#[derive(Debug, Clone)]
pub(crate) struct StagedCandidate {
    pub op_id: OpId,
    pub quarantine_dir: PathBuf,
    pub version_id: CommitId,
    pub bundle_digest: [u8; 32],
    pub entries: Vec<StagedEntry>,
    pub parent: Option<CommitId>,
    pub attribution: String,
    pub message: String,
}

/// The linearity fence: a version has **at most one parent** — `0` for a bundle's genesis, exactly
/// `1` for every other version — so a bundle's remote history is a LIST, never a DAG. The kernel
/// frame and the codec both stay general (they accept any parent slice); the AUTHORITY is what
/// refuses, which is where every other custody rule already lives.
///
/// It fences the COUNT at the commit FRAME, the earliest point a candidate's full parent set is
/// known — before a byte is staged and long before any pointer moves — and that is what covers
/// BOTH pointer lanes with one check. The direct publish CASes right after its frame is minted
/// here; the proposal's frame is minted here too, but its pointer move happens later, on the
/// approve path, which re-reads the version row's persisted `first_parent` (`db/custody/pointer.rs`)
/// — a single column that is a faithful summary of the whole frame ONLY because the frame that
/// wrote it could not have carried a second parent. Fencing at the CAS instead would leave a
/// multi-parent proposal committable and therefore approvable.
fn fence_linear_lineage(parents: &[[u8; 32]]) -> Result<()> {
    if parents.len() > 1 {
        return Err(AuthorityError::RejectedUpload(
            "a candidate must declare at most one parent".to_owned(),
        ));
    }
    Ok(())
}

/// Mint a candidate frame's `version_id` — the ONE place the vault turns `(parents, tree, author,
/// message)` into a version id, so [`fence_linear_lineage`] cannot be bypassed by a new caller. The
/// id BINDS the parent set, so a frame minted here can never gain a parent on its way to the
/// durable commit.
pub(crate) fn frame_version_id(
    parents: &[[u8; 32]],
    tree: [u8; 32],
    author: &str,
    message: &str,
) -> Result<CommitId> {
    fence_linear_lineage(parents)?;
    identity::commit_id(&Commit {
        parents,
        tree,
        author,
        message,
    })
    .map(CommitId)
    .map_err(|e| AuthorityError::RejectedUpload(format!("invalid commit frame: {e:?}")))
}

/// Step A + B: record the `upload` staging row, stage the candidate's full tree into its EPHEMERAL
/// per-op dir (server rehash), and reject any blob on the denylist (a best-effort early guard; the
/// serializing check is the install CAS + the commit transaction). Recomputes the `version_id` from
/// the rehashed bytes — a client id is never trusted, and none is even carried. Staging writes are
/// deliberately NOT fsynced: the dir is ephemeral by design (a crash re-uploads; nothing durable
/// references it).
pub(crate) async fn ingest(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    op_id: &OpId,
    candidate: CandidateUpload,
    now: i64,
) -> Result<StagedCandidate> {
    if candidate.files.is_empty() {
        return Err(AuthorityError::RejectedUpload(
            "a bundle must contain at least one file".to_owned(),
        ));
    }
    // Bound the file COUNT, not just each blob's size. Ingest cost is per-file and large — a
    // denylist probe, a staged file, an install CAS and a store put — so a body well under the
    // byte cap can still buy hundreds of thousands of round trips, serialized under the promotion
    // lease. The ceiling is far above any real bundle and far below what it takes to stall the vault.
    if candidate.files.len() > MAX_CANDIDATE_FILES {
        return Err(AuthorityError::RejectedUpload(
            "a bundle exceeds the maximum allowed file count".to_owned(),
        ));
    }
    // Per-blob guards BEFORE staging, so an oversize or purged blob is never persisted to the
    // staging dir (the object id is `sha256(bytes)`, exactly what the stage recomputes below, so
    // neither needs staging).
    for f in &candidate.files {
        // Reject cap: a blob over the per-blob limit fails typed and stages nothing.
        if f.bytes.len() as u64 > authority.reject_cap() {
            return Err(AuthorityError::RejectedUpload(
                "a candidate blob exceeds the maximum allowed size".to_owned(),
            ));
        }
        // Denylist: never (re-)introduce purged bytes. Best-effort early; the install CAS + the commit
        // transaction re-check serializably.
        let oid = ObjectId(digest::sha256(&f.bytes));
        if authority.db().is_tombstoned(ws, oid).await? {
            return Err(AuthorityError::RejectedUpload(
                "a candidate blob is on the denylist".to_owned(),
            ));
        }
    }

    let quarantine_dir = authority.workspace_quarantine_dir(ws, op_id);

    // Record the staging row (state 'staging') before touching the disk, so a crash mid-stage leaves a
    // janitor-able row. The janitor rebuilds the sweep path from the validated ids, never a stored path.
    authority.db().insert_upload(ws, bundle, op_id, now).await?;

    // Stage on the blocking pool: the whole candidate's bytes are written to disk, so running it
    // inline would pin an async worker. The files move into the closure (owned).
    let CandidateUpload {
        files,
        parent,
        attribution,
        message,
    } = candidate;
    let stage_dir = quarantine_dir.clone();
    let staged = crate::authority::run_blocking(move || {
        // Validate + compute the consent digest through the one kernel implementation (re-runs
        // check_path + the collision rules), BEFORE writing anything — a rejected bundle stages
        // nothing.
        let manifest: Vec<ManifestEntry> = files
            .iter()
            .map(|f| ManifestEntry {
                path: f.path.clone(),
                mode: f.mode,
                content_sha256: digest::sha256(&f.bytes),
            })
            .collect();
        let bundle_digest = digest::bundle_digest(&manifest).map_err(|r| {
            AuthorityError::RejectedUpload(format!("canonical rule violated: {r:?}"))
        })?;

        // Stage into a FRESH dir: a retry / re-ingest that reuses the op id (the authority's
        // quarantine row is an upsert, so reuse is a supported path) must not inherit a prior
        // candidate's files — the staged set is exactly THIS candidate's.
        if stage_dir.exists() {
            std::fs::remove_dir_all(&stage_dir).map_err(AuthorityError::internal)?;
        }
        std::fs::create_dir_all(&stage_dir).map_err(AuthorityError::internal)?;
        let mut entries = Vec::with_capacity(files.len());
        for (f, m) in files.iter().zip(&manifest) {
            let git_oid = codec::blob_git_oid(&f.bytes).map_err(map_stage_reject)?;
            // One staged file per DISTINCT object (a blob at two paths is one object) — named by
            // its content id, so the install can read it back by `object_id` alone.
            let staged_path = stage_dir.join(crate::store::hex_lower(&m.content_sha256));
            if !staged_path.exists() {
                std::fs::write(&staged_path, &f.bytes).map_err(AuthorityError::internal)?;
            }
            entries.push(StagedEntry {
                path: f.path.clone(),
                mode: f.mode,
                object_id: m.content_sha256,
                git_oid,
                size: f.bytes.len() as u64,
            });
        }
        Ok((entries, bundle_digest))
    })
    .await?;
    let (entries, bundle_digest) = staged;

    let parent_ids: Vec<[u8; 32]> = parent.iter().map(|c| c.0).collect();
    let version_id = frame_version_id(&parent_ids, bundle_digest, &attribution, &message)?;

    // The stage completed — flip the audit row to 'quarantined' and record the recomputed digest.
    authority
        .db()
        .mark_upload_quarantined(op_id, &crate::id::hex32(&bundle_digest))
        .await?;

    Ok(StagedCandidate {
        op_id: op_id.clone(),
        quarantine_dir,
        version_id,
        bundle_digest,
        entries,
        parent,
        attribution,
        message,
    })
}

/// Step D, part 1: insert the promotion lease over the candidate's FULL distinct object set BEFORE any
/// byte migrates — so a concurrent GC's keep-set already protects every needed object (including an old,
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
            now + LEASE_TTL_MS,
        )
        .await
}

/// Step D, part 2: put every not-already-present object to the store as a zlib loose blob. The DB
/// decides reuse (`present` → skip); a `deleting` object is waited out (OUTSIDE any write
/// transaction) then re-put fresh; bytes reach `present` only after the put returns (durable on
/// both backends — the local backend fsyncs inside the seam). The lease (part 1) protects them
/// throughout.
pub(crate) async fn migrate_install(
    authority: &Authority,
    ws: &WorkspaceId,
    staged: &StagedCandidate,
    now: i64,
) -> Result<()> {
    for entry in distinct_entries(&staged.entries) {
        install_one(authority, ws, &staged.quarantine_dir, entry, now).await?;
    }
    Ok(())
}

/// Step D, part 3: put the migrated candidate's version SKELETON — its (nested) tree objects and
/// its commit object, encoded in memory — then make the lease non-expiring (the candidate stays
/// rooted until its version row lands) and drop the staging dir. Returns the commit's git OID for
/// the version row.
///
/// Skeleton objects carry no presence/reachability rows: they are the version's addressing
/// skeleton, retained until workspace deletion (exactly as the per-workspace repo retained them).
/// A crash between the skeleton put and the version-row transaction leaves orphan skeleton
/// objects — accepted (immutable, content-addressed, bytes-cheap).
pub(crate) async fn migrate_finish(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    staged: &StagedCandidate,
    now: i64,
) -> Result<[u8; 20]> {
    // The parent's git commit locator comes from its VERSION ROW — refs are gone. A parent row
    // predating the object-store import (NULL locator) fails closed, naming the cure.
    let parent_commit_oids: Vec<[u8; 20]> = match staged.parent {
        None => Vec::new(),
        Some(parent) => {
            let oid = authority
                .db()
                .version_git_commit_oid(ws, bundle, parent)
                .await?;
            match oid {
                None => {
                    return Err(AuthorityError::RejectedUpload(
                        "a parent version is not present in this workspace".to_owned(),
                    ));
                }
                Some(None) => return Err(AuthorityError::integrity(PreImportRow)),
                Some(Some(oid)) => vec![oid],
            }
        }
    };

    // Encode the skeleton in memory (pure; component validation matches the client's write path).
    let entry_refs: Vec<(&str, FileMode, [u8; 20])> = staged
        .entries
        .iter()
        .map(|e| (e.path.as_str(), e.mode, e.git_oid))
        .collect();
    let tree = codec::encode_tree(&entry_refs).map_err(map_stage_reject)?;
    let commit = codec::encode_commit(
        tree.root_oid,
        &parent_commit_oids,
        &staged.attribution,
        &staged.message,
    )
    .map_err(map_stage_reject)?;
    let commit_oid = commit.git_oid;

    // Put trees then the commit (idempotent content-addressed puts — a re-run re-writes the
    // identical objects).
    for obj in tree.objects {
        authority
            .store()
            .put_loose(ws, &obj.git_oid, obj.zlib_bytes)
            .await?;
    }
    authority
        .store()
        .put_loose(ws, &commit.git_oid, commit.zlib_bytes)
        .await?;

    // Success: the lease becomes the durable root until the version-row transaction consumes it. The CAS
    // on the staged commit + lease liveness fails closed if this ingest's lease lapsed (so it cannot acquire
    // a success whose objects GC may already have reclaimed).
    authority
        .db()
        .commit_lease(ws, &staged.op_id, staged.version_id, now)
        .await?;
    // Post-commit cleanup (the objects are safely in the store): remove the staging dir. A rm
    // failure leaves an orphan dir beside a live audit row — a low-severity, disk-only residual
    // (the janitor sweeps rows still in-flight; a committed row's leaked dir is ephemeral anyway).
    remove_quarantine_dir(&staged.quarantine_dir);
    Ok(commit_oid)
}

/// Stage a SERVER-CONSTRUCTED forward commit (a revert): its objects are already present in the
/// store (the revert restores a retained version's tree — the SAME root tree object), so there is
/// **nothing to install and no tree to build** — just lease the object set, put the new commit
/// object, and make the lease non-expiring. The commit transaction then consumes the lease exactly
/// as a publish does, so the lease→edge handoff behaves identically. No upload, no staging dir.
/// Returns the forward commit's git OID for the version row.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn stage_forward_commit(
    authority: &Authority,
    ws: &WorkspaceId,
    op_id: &OpId,
    version_id: CommitId,
    tree_oid: [u8; 20],
    parent_commit_oids: &[[u8; 20]],
    parents: &[CommitId],
    object_ids: &[ObjectId],
    attribution: &str,
    message: &str,
    now: i64,
) -> Result<[u8; 20]> {
    // This frame arrives already minted (the caller pre-derived its id), so it is the one parent
    // SLICE the crate does not build itself — the linearity fence runs on it here, before the lease
    // and before any commit object reaches the store.
    let parent_bytes: Vec<[u8; 32]> = parents.iter().map(|c| c.0).collect();
    fence_linear_lineage(&parent_bytes)?;

    // Lease the object set BEFORE recording the commit (the commit transaction's lease gate requires the
    // committed lease, and the GC keep-set protects the objects meanwhile — exactly as migrate does).
    authority
        .db()
        .insert_lease(ws, op_id, version_id, object_ids, now + LEASE_TTL_MS)
        .await?;

    let commit = codec::encode_commit(tree_oid, parent_commit_oids, attribution, message)
        .map_err(map_stage_reject)?;
    let commit_oid = commit.git_oid;
    authority
        .store()
        .put_loose(ws, &commit.git_oid, commit.zlib_bytes)
        .await?;

    authority
        .db()
        .commit_lease(ws, op_id, version_id, now)
        .await?;
    Ok(commit_oid)
}

/// Remove a staging dir, treating "already gone" as success (a replaced container lost it — the
/// ephemeral-staging contract). Returns whether the dir is now gone.
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
/// committing a candidate whose objects a concurrent GC may already have reclaimed. Returns the
/// commit's git OID for the version row.
pub(crate) async fn migrate(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    staged: &StagedCandidate,
    now: i64,
) -> Result<[u8; 20]> {
    let started = tokio::time::Instant::now();
    migrate_lease(authority, ws, staged, now).await?;
    migrate_install(authority, ws, staged, now).await?;
    // The install genuinely took real wall-clock time (its `deleting`-waits sleep); advance the finish
    // clock by that elapsed time so the lease-liveness CAS is meaningful. In the fast path (and in tests)
    // this is ~0, so finish ≈ now.
    let finish_now = now.saturating_add(elapsed_ms(started));
    migrate_finish(authority, ws, bundle, staged, finish_now).await
}

/// Install one object, honoring the fence: reuse if `present` (re-putting into the store if a
/// crash lost the bytes); wait out `deleting` (no transaction held across the sleep, so GC's
/// finalize can never be blocked) then re-put; reject if `unavailable`. The CAS, the lease, and
/// the non-resurrectable `deleting` guard are exactly the pre-object-store fence.
async fn install_one(
    authority: &Authority,
    ws: &WorkspaceId,
    quarantine_dir: &Path,
    entry: &StagedEntry,
    now: i64,
) -> Result<()> {
    let object_id = ObjectId(entry.object_id);
    let mut backoff = WAIT_BACKOFF_START;
    for _ in 0..WAIT_MAX_POLLS {
        match authority.db().object_status(ws, object_id).await? {
            ObjectStatus::Present => {
                // Dedup reuse — but never PIN a version over bytes a past crash silently removed.
                // Re-put from this candidate's staging if the store lost them. This ingest's lease
                // already spares the row, so no GC can race the re-put; the DB row stays `present`
                // throughout (the store never becomes the presence authority — we only refuse to
                // root nothing). The store key is the row's recorded locator, which for a healthy
                // dedup equals this candidate's `git_oid` (same bytes ⇒ same blob framing).
                if !authority.store().exists(ws, &entry.git_oid).await? {
                    put_staged_blob(authority, ws, quarantine_dir, entry).await?;
                }
                return Ok(()); // dedup: reuse the already-present (and now re-materialized) bytes
            }
            ObjectStatus::Unavailable => {
                return Err(AuthorityError::RejectedUpload(
                    "a candidate blob is on the denylist".to_owned(),
                ));
            }
            ObjectStatus::Deleting => {
                // A GC is unlinking these bytes — wait for `absent` (poll on the pool; no txn held), then
                // re-put fresh. NEVER override `deleting` (the non-resurrectable fence).
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(WAIT_BACKOFF_CAP);
                continue;
            }
            ObjectStatus::Absent => {
                // Fresh install: put the bytes FIRST (the seam makes "put returned" imply durable),
                // then the `absent → present` CAS records the locator — so a `present` row always
                // denotes bytes durably in the store.
                put_staged_blob(authority, ws, quarantine_dir, entry).await?;
                match authority
                    .db()
                    .install_object(
                        ws,
                        object_id,
                        &entry.git_oid,
                        i64::try_from(entry.size).map_err(AuthorityError::integrity)?,
                        now,
                    )
                    .await?
                {
                    InstallOutcome::Installed | InstallOutcome::AlreadyPresent => return Ok(()),
                    // A GC acquired it between the status read and the CAS — wait it out and retry.
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

/// Read one staged blob from this op's staging dir (verifying `sha256(bytes) == object_id` — a
/// staged file corrupted after the stage can never be installed under a locator whose bytes now
/// lie), frame it as a zlib loose blob (asserting the frame lands on the recorded `git_oid`), and
/// put it to the store. The blocking-pool section covers the file read + framing; the put is the
/// seam's own async op.
async fn put_staged_blob(
    authority: &Authority,
    ws: &WorkspaceId,
    quarantine_dir: &Path,
    entry: &StagedEntry,
) -> Result<()> {
    let staged_path = quarantine_dir.join(crate::store::hex_lower(&entry.object_id));
    let (expected_object, expected_git) = (entry.object_id, entry.git_oid);
    let loose = crate::authority::run_blocking(move || {
        let bytes = std::fs::read(&staged_path).map_err(AuthorityError::internal)?;
        if digest::sha256(&bytes) != expected_object {
            return Err(AuthorityError::integrity(StagedBlobCorrupt));
        }
        let obj = codec::encode_blob(&bytes).map_err(AuthorityError::internal)?;
        if obj.git_oid != expected_git {
            return Err(AuthorityError::integrity(StagedBlobCorrupt));
        }
        Ok(obj)
    })
    .await?;
    authority
        .store()
        .put_loose(ws, &loose.git_oid, loose.zlib_bytes)
        .await
}

/// Whole elapsed milliseconds since `started`, saturating into `i64` — the increment added to a
/// caller-supplied epoch-ms `now` (the one server-clock unit).
pub(crate) fn elapsed_ms(started: tokio::time::Instant) -> i64 {
    i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)
}

/// The distinct object ids of a staged bundle's entries (a blob at two paths is one edge). The commit
/// transaction derives the candidate's reachability + availability set from this same function — the
/// authoritative in-process candidate tree — so the `version_object` edges it writes match exactly the
/// object set `migrate_lease` rooted.
pub(crate) fn distinct_object_ids(entries: &[StagedEntry]) -> Vec<ObjectId> {
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

/// Map a codec/staging failure to the boundary error: a canonical-rule reject / bad path is the
/// caller's problem; a low-level fault is internal.
fn map_stage_reject(e: GitstoreError) -> AuthorityError {
    match e {
        GitstoreError::Reject(reason) => {
            AuthorityError::RejectedUpload(format!("canonical rule violated: {reason:?}"))
        }
        GitstoreError::RejectPath(msg) => {
            AuthorityError::RejectedUpload(format!("invalid path component: {msg}"))
        }
        other => AuthorityError::internal(other),
    }
}

#[derive(Debug, thiserror::Error)]
#[error(
    "an object stayed in the deleting state past the wait bound (a crashed GC awaiting recovery)"
)]
struct DeletingWaitTimedOut;

/// A version row that predates the object-store import — its git commit locator/message are NULL.
/// The cure is one-shot: `topos-plane import-local`.
#[derive(Debug, thiserror::Error)]
#[error(
    "this version predates the object-store import (no recorded commit locator) — run `topos-plane import-local`"
)]
pub(crate) struct PreImportRow;

/// A staged file's bytes no longer match the identity recorded at stage time (post-stage
/// corruption of the ephemeral dir) — the install refuses rather than framing lying bytes.
#[derive(Debug, thiserror::Error)]
#[error("a staged blob's bytes do not match the identity recorded at stage time")]
struct StagedBlobCorrupt;
