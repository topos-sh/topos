//! The read surface — the pointer record, one object's bytes, a version's metadata + file listing,
//! and the first-parent log. Every byte read is **verified against the id that named it** before it
//! is served (verify-on-read); corruption is an [`AuthorityError::Integrity`] alarm, never a
//! not-found. There is no read-by-bare-hash path anywhere: an object is served only through a
//! bundle whose live (non-purged) version reaches it.
//!
//! History answers from POSTGRES (`log` walks version rows; parents/author/message are columns);
//! the store is consulted only where a file listing or byte payload is needed (the version
//! skeleton's commit/tree objects, the blobs). A row that predates the object-store import (NULL
//! commit locator/message) fails CLOSED with a typed error naming `topos-plane import-local`.

use topos_core::digest::{self, FileMode};
use topos_gitstore::codec;

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, CommitId, ObjectId, WorkspaceId};
use crate::lifecycle::PreImportRow;

/// The deepest tree nesting the store walk follows — matches the codec's build bound, so a
/// version deep enough to be refused here could never have been encoded.
const MAX_TREE_DEPTH: usize = 64;

/// A bundle's `current` pointer, ready to serve: the pointed version, the CAS generation, the move
/// attribution + time, and the pointed version's consent digest.
#[derive(Debug, Clone)]
pub struct CurrentInfo {
    pub version_id: CommitId,
    pub generation: u64,
    pub moved_at_ms: i64,
    pub moved_by: String,
    pub bundle_digest: [u8; 32],
}

/// One file of a version's metadata — its bundle-relative path, mode, and content id (`object_id`).
/// The bytes are NOT here: a caller fetches each by id through the object read.
#[derive(Debug, Clone)]
pub struct VersionFile {
    pub path: String,
    pub mode: FileMode,
    pub object_id: [u8; 32],
}

/// A version's metadata — its id, the COMPLETE parent set, display attribution + message, the
/// consent `bundle_digest`, and the per-file `(path, mode, object_id)` leaves. Assembled WITHOUT
/// reading any blob bytes; the digest is the pin the byte fetches + the client's re-hash must
/// reproduce.
#[derive(Debug, Clone)]
pub struct VersionMeta {
    pub version_id: [u8; 32],
    pub parents: Vec<[u8; 32]>,
    pub author: String,
    pub message: String,
    pub bundle_digest: [u8; 32],
    pub created_at_ms: i64,
    pub files: Vec<VersionFile>,
}

/// One workspace's stored byte total — the sum of its `present` object sizes (see
/// [`Authority::storage_stats`]).
#[derive(Debug, Clone)]
pub struct WorkspaceStorage {
    pub workspace_id: WorkspaceId,
    pub stored_bytes: u64,
}

/// One hop of the first-parent log.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub version_id: CommitId,
    /// The commit message (the version row's recorded frame message).
    pub message: String,
    /// The attribution recorded on the version row (`author_display`).
    pub author_display: String,
    /// When the version row was created (epoch milliseconds).
    pub created_at_ms: i64,
    /// When the version's bytes were purged (epoch milliseconds), if they were.
    pub purged_at_ms: Option<i64>,
}

/// Read a bundle's `current` pointer. `None` until the pointer has first been created. The pointed
/// version's digest rides along (it is what a follower re-verifies after the fetch); a pointer over
/// a version with no digest row is corruption, never a not-found.
pub(crate) async fn read_current(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
) -> Result<Option<CurrentInfo>> {
    let Some(pointer) = authority.db().read_pointer(ws, bundle).await? else {
        return Ok(None);
    };
    let bundle_digest = authority
        .db()
        .read_bundle_digest(ws, bundle, pointer.version_id)
        .await?
        .ok_or_else(|| AuthorityError::integrity(MissingPointedDigest))?;
    Ok(Some(CurrentInfo {
        version_id: pointer.version_id,
        generation: pointer.generation,
        moved_at_ms: pointer.moved_at_ms,
        moved_by: pointer.moved_by,
        bundle_digest,
    }))
}

/// Read one object's bytes through the bundle-scoped reachability rule.
///
/// The bytes are returned only if some live (non-purged) version of `bundle` reaches `object_id`.
/// Every miss — unknown bundle, unreachable object, purged-away version — is the single typed
/// [`AuthorityError::NotFound`]. Every returned byte is re-verified against the content id that
/// named it; a post-reachability store failure is re-checked once (a concurrent purge/GC that
/// legitimately reclaimed the bytes reads NotFound, genuine corruption stays Integrity).
pub(crate) async fn read_object(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    object_id: ObjectId,
) -> Result<Vec<u8>> {
    // Step one (async DB): the reachability witness — some live version of THIS bundle reaches the
    // object. The borrow on the database is released before the store read below.
    if authority
        .db()
        .object_witness(ws, bundle, object_id)
        .await?
        .is_none()
    {
        return Err(AuthorityError::NotFound);
    }

    // Step two: fetch + verify the bytes from the store by the locator the database records. A
    // present row always carries one (`git_oid` is written in the same CAS that flips `present`);
    // no live row means the object was reclaimed between the witness and here — the re-check below
    // sorts legitimate reclaim from corruption.
    let fetched = match authority.db().object_dispatch(ws, object_id).await? {
        None => Err(AuthorityError::integrity(NoLivePresence)),
        Some(git_oid) => match authority.store().get_loose(ws, &git_oid).await? {
            None => Err(AuthorityError::integrity(LooseObjectMissing)),
            Some(zlib) => {
                // Verify-on-read is two gates: the frame must hash to the locator that named it,
                // and the payload must hash to the CONTENT id the caller asked for.
                let (kind, payload) =
                    codec::decode_loose(&zlib, git_oid).map_err(AuthorityError::integrity)?;
                if !matches!(kind, codec::ObjectKind::Blob) {
                    Err(AuthorityError::integrity(GitLocatorMismatch))
                } else if digest::sha256(&payload) == object_id.0 {
                    Ok(payload)
                } else {
                    Err(AuthorityError::integrity(GitLocatorMismatch))
                }
            }
        },
    };

    // Re-check-on-miss (the read-time TOCTOU guard). The witness above and this fetch are two steps;
    // between them a purge can tombstone the version (and a GC reclaim its unique bytes). On a
    // post-witness failure, re-run the reachability probe: if the object is no longer reachable, it
    // was legitimately reclaimed → NotFound. A still-reachable object that fails to load IS genuine
    // corruption → the Integrity fault stands.
    if let Err(AuthorityError::Integrity(_)) = &fetched
        && authority
            .db()
            .object_witness(ws, bundle, object_id)
            .await?
            .is_none()
    {
        return Err(AuthorityError::NotFound);
    }
    fetched
}

/// Every workspace's stored byte total, ordered by workspace id (deterministic). Counts `present`
/// rows ONLY — `deleting`/`absent`/`unavailable` bytes are not custody the product should bill or
/// display (they are either mid-reclaim, already gone, or denylisted forever). A workspace holding
/// no present object is absent from the list. The figure is RAW file bytes (the size recorded at
/// install) — never compressed store bytes, and never the version skeleton.
pub(crate) async fn storage_stats(authority: &Authority) -> Result<Vec<WorkspaceStorage>> {
    Ok(authority
        .db()
        .storage_stats()
        .await?
        .into_iter()
        .map(|(workspace_id, stored_bytes)| WorkspaceStorage {
            workspace_id,
            stored_bytes,
        })
        .collect())
}

/// Read a version's metadata + file listing (no blob bytes). A purged version reads NotFound — its
/// bytes are gone by decision; the log still lists the row. Parents/author/message come from the
/// VERSION ROW; the file listing walks the version's skeleton (commit → trees) in the store and
/// joins each leaf's locator back to its content id over the present rows.
pub(crate) async fn read_version(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    version: CommitId,
) -> Result<VersionMeta> {
    let row = match authority.db().read_version_row(ws, bundle, version).await? {
        None => return Err(AuthorityError::NotFound),
        Some(row) if row.purged_at_ms.is_some() => return Err(AuthorityError::NotFound),
        Some(row) => row,
    };
    // A committed version always carries a recorded digest; its absence is a divergence
    // (corruption), never a not-found.
    let bundle_digest = authority
        .db()
        .read_bundle_digest(ws, bundle, version)
        .await?
        .ok_or_else(|| AuthorityError::integrity(MissingVersionDigest))?;
    // Pre-import rows fail closed, naming the cure.
    let (git_commit_oid, message) = match (row.git_commit_oid, row.message) {
        (Some(oid), Some(message)) => (oid, message),
        _ => return Err(AuthorityError::integrity(PreImportRow)),
    };

    // The version's structure: GET the commit object, then walk its trees — each object verified
    // against the id that named it — THEN the presence rows are queried for exactly those tree
    // leaves, so the DB read scales with the requested version, never the workspace's lifetime
    // object count.
    let commit = read_commit(authority, ws, git_commit_oid).await?;
    let leaves = walk_tree_leaves(authority, ws, commit.tree_oid).await?;
    let leaf_oids: Vec<[u8; 20]> = leaves.iter().map(|(_, _, oid)| *oid).collect();
    let by_git_oid = authority.db().objects_by_git_oids(ws, &leaf_oids).await?;

    let mut files = Vec::with_capacity(leaves.len());
    for (path, mode, git_oid) in leaves {
        // Each tree-entry git OID joins to its content id over the workspace's PRESENT rows. A leaf with no
        // present row is a bookkeeping/store divergence.
        let object_id = by_git_oid
            .get(&git_oid)
            .copied()
            .ok_or_else(|| AuthorityError::integrity(VersionObjectMissing))?;
        files.push(VersionFile {
            path,
            mode,
            object_id,
        });
    }
    Ok(VersionMeta {
        version_id: version.0,
        parents: row.first_parent.into_iter().map(|p| p.0).collect(),
        author: row.author_display,
        message,
        bundle_digest,
        created_at_ms: row.created_at_ms,
        files,
    })
}

/// The first-parent commit chain from `current`, capped — version ids + messages + attributions +
/// timestamps (what a log/review surface renders). Answered from Postgres alone: the chain walks
/// the persisted `first_parent` column and each row carries its message; a purged version stays
/// listed with its purge stamp. A pre-import hop (NULL message) fails closed naming the cure.
pub(crate) async fn log(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    limit: usize,
) -> Result<Vec<LogEntry>> {
    let Some(pointer) = authority.db().read_pointer(ws, bundle).await? else {
        return Err(AuthorityError::NotFound);
    };
    let hops = authority
        .db()
        .log_chain(ws, bundle, pointer.version_id, limit)
        .await?;
    // A walk that stopped SHORT of the cap must have stopped at genesis (a NULL first parent).
    // Stopping on a hop that NAMES a parent means the named row is missing from this bundle —
    // lineage corruption, surfaced loudly rather than served as a silently shorter history.
    // (The head itself always joins — the pointer's FK guarantees its row.)
    if hops.len() < limit
        && let Some(last) = hops.last()
        && last.first_parent.is_some()
    {
        return Err(AuthorityError::integrity(LogChainBroken));
    }
    hops.into_iter()
        .map(|hop| {
            let message = hop
                .message
                .ok_or_else(|| AuthorityError::integrity(PreImportRow))?;
            Ok(LogEntry {
                version_id: hop.version_id,
                message,
                author_display: hop.author_display,
                created_at_ms: hop.created_at_ms,
                purged_at_ms: hop.purged_at_ms,
            })
        })
        .collect()
}

/// GET + decode one commit object by its locator (frame verified against the locator).
pub(crate) async fn read_commit(
    authority: &Authority,
    ws: &WorkspaceId,
    git_commit_oid: [u8; 20],
) -> Result<codec::CommitMeta> {
    let zlib = authority
        .store()
        .get_loose(ws, &git_commit_oid)
        .await?
        .ok_or_else(|| AuthorityError::integrity(LooseObjectMissing))?;
    let (kind, payload) =
        codec::decode_loose(&zlib, git_commit_oid).map_err(AuthorityError::integrity)?;
    if !matches!(kind, codec::ObjectKind::Commit) {
        return Err(AuthorityError::integrity(GitLocatorMismatch));
    }
    codec::decode_commit(&payload).map_err(AuthorityError::integrity)
}

/// Walk a version's tree skeleton in the store, yielding every file leaf's
/// `(path, mode, git_oid)` — no blob is read. Iterative (an explicit stack) with the same nesting
/// bound the codec's builder applies; every tree object is verified against the id that named it.
pub(crate) async fn walk_tree_leaves(
    authority: &Authority,
    ws: &WorkspaceId,
    root_tree_oid: [u8; 20],
) -> Result<Vec<(String, FileMode, [u8; 20])>> {
    let mut leaves = Vec::new();
    let mut stack: Vec<(String, [u8; 20], usize)> = vec![(String::new(), root_tree_oid, 0)];
    while let Some((prefix, tree_oid, depth)) = stack.pop() {
        if depth > MAX_TREE_DEPTH {
            return Err(AuthorityError::integrity(TreeTooDeep));
        }
        let zlib = authority
            .store()
            .get_loose(ws, &tree_oid)
            .await?
            .ok_or_else(|| AuthorityError::integrity(LooseObjectMissing))?;
        let (kind, payload) =
            codec::decode_loose(&zlib, tree_oid).map_err(AuthorityError::integrity)?;
        if !matches!(kind, codec::ObjectKind::Tree) {
            return Err(AuthorityError::integrity(GitLocatorMismatch));
        }
        for child in codec::decode_tree(&payload).map_err(AuthorityError::integrity)? {
            match child {
                codec::TreeChild::Subtree { name, git_oid } => {
                    let path = join_path(&prefix, &name);
                    stack.push((path, git_oid, depth + 1));
                }
                codec::TreeChild::File {
                    name,
                    mode,
                    git_oid,
                } => {
                    let path = join_path(&prefix, &name);
                    leaves.push((path, mode, git_oid));
                }
            }
        }
    }
    Ok(leaves)
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("a present object's locator does not resolve to its content id")]
struct GitLocatorMismatch;

#[derive(Debug, thiserror::Error)]
#[error("the pointed version has no recorded bundle digest")]
struct MissingPointedDigest;

#[derive(Debug, thiserror::Error)]
#[error("a committed version has no recorded bundle digest")]
struct MissingVersionDigest;

#[derive(Debug, thiserror::Error)]
#[error("a version's tree leaf has no present object row")]
struct VersionObjectMissing;

#[derive(Debug, thiserror::Error)]
#[error("a reachable object has no live presence row")]
struct NoLivePresence;

#[derive(Debug, thiserror::Error)]
#[error("a recorded loose object is missing from the store")]
struct LooseObjectMissing;

#[derive(Debug, thiserror::Error)]
#[error("a stored tree nests deeper than the codec could have written")]
struct TreeTooDeep;

#[derive(Debug, thiserror::Error)]
#[error("a first-parent log hop names a parent this bundle does not hold (broken lineage)")]
struct LogChainBroken;
