//! The read surface — the pointer record, one object's bytes, a version's metadata + file listing,
//! and the first-parent log. Every byte read is **verified against the id that named it** before it
//! is served (verify-on-read); corruption is an [`AuthorityError::Integrity`] alarm, never a
//! not-found. There is no read-by-bare-hash path anywhere: an object is served only through a
//! bundle whose live (non-purged) version reaches it.

use std::collections::HashMap;

use topos_core::digest::{self, FileMode, ManifestEntry, RejectReason};
use topos_gitstore::{LargeObjectStore, RenderedBundle, RenderedFile, Store};

use crate::authority::{Authority, run_blocking};
use crate::db::Location;
use crate::error::{AuthorityError, Result};
use crate::id::{BundleId, CommitId, ObjectId, WorkspaceId};

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

/// One hop of the first-parent log.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub version_id: CommitId,
    /// The commit message (from the git commit frame).
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
    let Some(witness) = authority.db().object_witness(ws, bundle, object_id).await? else {
        return Err(AuthorityError::NotFound);
    };

    // Step two: fetch + verify the bytes from the store the database records, dispatching on
    // `location`. The verify-on-read byte fetch runs on the blocking pool; the non-`Send` gix
    // `Store` opens + drops inside the closure.
    let dispatch = authority.db().object_dispatch(ws, object_id).await?;
    let git_dir = authority.workspace_git_dir(ws);
    let large = authority.large_store(ws);
    let fetched = run_blocking(move || match dispatch {
        // Offloaded: fetch from the large store (its `get` re-verifies sha256 == object_id).
        Some((Location::LargeLocal, _)) => {
            large.get(object_id.0).map_err(AuthorityError::integrity)
        }
        // Git-resident: read the loose object DIRECTLY by its locator and re-verify the content id — NOT a
        // whole-version-tree walk, which would fault on an offloaded sibling's absent git object in a mixed
        // bundle before reaching the requested blob.
        Some((Location::Git, git_oid)) => Store::open(&git_dir)
            .map_err(AuthorityError::integrity)?
            .read_git_blob_verified(git_oid)
            .map_err(AuthorityError::integrity)
            .and_then(|(bytes, content_sha256)| {
                if content_sha256 == object_id.0 {
                    Ok(bytes)
                } else {
                    Err(AuthorityError::integrity(GitLocatorMismatch))
                }
            }),
        // No live presence row: fall back to the witness version's tree walk (safe: an all-git
        // version). A reclaimed object also lands here, because `object_dispatch` filters
        // `status = 'present'`; the re-check below catches that case.
        None => Store::open(&git_dir)
            .map_err(AuthorityError::integrity)?
            .read_object_in_version(witness.0, object_id.0)
            .map_err(AuthorityError::integrity),
    })
    .await;

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

/// Read a version's metadata + file listing (no blob bytes). A purged version reads NotFound — its
/// bytes are gone by decision; the log still lists the row.
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

    // The version's structure comes FIRST, from a blocking-pool store section (the non-`Send` gix
    // `Store` opens + drops inside the closure); THEN the presence rows are queried for exactly
    // those tree leaves, so the DB read scales with the requested version, never the workspace's
    // lifetime object count.
    let git_dir = authority.workspace_git_dir(ws);
    let version_bytes = version.0;
    let (node, leaves) = run_blocking(move || {
        let store = Store::open(&git_dir).map_err(AuthorityError::integrity)?;
        let node = store
            .read_commit_meta(version_bytes)
            .map_err(AuthorityError::integrity)?;
        let leaves = store
            .read_tree_structure(version_bytes)
            .map_err(AuthorityError::integrity)?;
        Ok((node, leaves))
    })
    .await?;
    let leaf_oids: Vec<[u8; 20]> = leaves.iter().map(|l| l.git_oid).collect();
    let by_git_oid = authority.db().objects_by_git_oids(ws, &leaf_oids).await?;

    let mut files = Vec::with_capacity(leaves.len());
    for leaf in leaves {
        // Each tree-entry git OID joins to its content id over the workspace's PRESENT rows. A leaf with no
        // present row is a bookkeeping/store divergence.
        let object_id = by_git_oid
            .get(&leaf.git_oid)
            .copied()
            .ok_or_else(|| AuthorityError::integrity(VersionObjectMissing))?;
        files.push(VersionFile {
            path: leaf.path,
            mode: leaf.mode,
            object_id,
        });
    }
    Ok(VersionMeta {
        version_id: node.version_id,
        parents: node.parents,
        author: node.author,
        message: node.message,
        bundle_digest,
        created_at_ms: row.created_at_ms,
        files,
    })
}

/// The first-parent commit chain from `current`, capped — version ids + messages + attributions +
/// timestamps (what a log/review surface renders). The chain walks the git commit frames (which
/// hold the parent links + messages) and joins each hop to its version row for the display facts;
/// a purged version stays listed with its purge stamp.
pub(crate) async fn log(
    authority: &Authority,
    ws: &WorkspaceId,
    bundle: &BundleId,
    limit: usize,
) -> Result<Vec<LogEntry>> {
    let Some(pointer) = authority.db().read_pointer(ws, bundle).await? else {
        return Err(AuthorityError::NotFound);
    };

    // Walk the first-parent chain in ONE blocking-pool section (each hop is a small commit-frame
    // read; the non-`Send` gix `Store` never crosses an await).
    let git_dir = authority.workspace_git_dir(ws);
    let head = pointer.version_id.0;
    let hops: Vec<([u8; 32], String)> = run_blocking(move || {
        let store = Store::open(&git_dir).map_err(AuthorityError::integrity)?;
        let mut hops = Vec::new();
        let mut cursor = Some(head);
        while let Some(id) = cursor {
            if hops.len() >= limit {
                break;
            }
            let node = store
                .read_commit_meta(id)
                .map_err(AuthorityError::integrity)?;
            cursor = node.parents.first().copied();
            hops.push((id, node.message));
        }
        Ok(hops)
    })
    .await?;

    // One batched join to the version rows for the display facts. A hop with no row is a
    // cross-bundle stray (the shared per-workspace repo can hold another bundle's commits) — that
    // would be a lineage corruption for a first-parent chain, so surface it.
    let ids: Vec<CommitId> = hops.iter().map(|(id, _)| CommitId(*id)).collect();
    let rows = authority.db().read_version_rows(ws, bundle, &ids).await?;
    hops.into_iter()
        .map(|(id, message)| {
            let row = rows
                .get(&CommitId(id))
                .ok_or_else(|| AuthorityError::integrity(LogHopWithoutRow))?;
            Ok(LogEntry {
                version_id: CommitId(id),
                message,
                author_display: row.author_display.clone(),
                created_at_ms: row.created_at_ms,
                purged_at_ms: row.purged_at_ms,
            })
        })
        .collect()
}

/// Assemble + verify a whole bundle for a version, dispatching each file to the store the database
/// records — the whole-bundle assembly primitive (tests + any composing verification drive it).
///
/// **Tree-driven** — render anchors on the version's git **tree structure** (`(path, mode,
/// git_oid)` per file). The offloaded subset is the workspace's present `large-local` rows, joined
/// in memory by `git_oid → object_id`; each file's bytes come from the large store (offloaded) or
/// git (git-resident), re-verified to its content id; the recomputed `bundle_digest` must then
/// equal the pin.
///
/// # Errors
/// [`AuthorityError::Integrity`] if a file's bytes are missing/corrupt in either store, a stored
/// path is illegal, or the recomputed digest does not match `expected_bundle_digest`;
/// [`AuthorityError::Internal`] on a database fault.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn render_version(
    authority: &Authority,
    ws: &WorkspaceId,
    version_id: [u8; 32],
    expected_bundle_digest: [u8; 32],
) -> Result<RenderedBundle> {
    // The offloaded set for this workspace: git_oid -> object_id (small — big blobs are rare). A git-resident
    // leaf is absent from this map and recovers its id by rehashing the git blob, with no DB dependency.
    let offloaded: HashMap<[u8; 20], [u8; 32]> = authority
        .db()
        .large_local_objects(ws)
        .await?
        .into_iter()
        .map(|(git_oid, object_id)| (git_oid, object_id.0))
        .collect();

    // The whole-bundle assembly (every blob read + re-hashed) runs on the blocking pool; the
    // non-`Send` gix `Store` opens + drops inside the closure.
    let git_dir = authority.workspace_git_dir(ws);
    let large = authority.large_store(ws);
    run_blocking(move || {
        let store = Store::open(&git_dir).map_err(AuthorityError::integrity)?;
        let structure = store
            .read_tree_structure(version_id)
            .map_err(AuthorityError::integrity)?;

        let mut files = Vec::with_capacity(structure.len());
        let mut manifest = Vec::with_capacity(structure.len());
        for leaf in structure {
            let (bytes, content_sha256) = match offloaded.get(&leaf.git_oid) {
                Some(&object_id) => {
                    // Offloaded: fetch from the large store (its `get` re-verifies sha256 == object_id).
                    let bytes = large.get(object_id).map_err(AuthorityError::integrity)?;
                    (bytes, object_id)
                }
                None => store
                    .read_git_blob_verified(leaf.git_oid)
                    .map_err(AuthorityError::integrity)?,
            };
            manifest.push(ManifestEntry {
                path: leaf.path.clone(),
                mode: leaf.mode,
                content_sha256,
            });
            files.push(RenderedFile {
                path: leaf.path,
                mode: leaf.mode,
                bytes,
                content_sha256,
            });
        }

        // Recompute the consent digest over the assembled real bytes and assert it equals the pin — the
        // integrity gate that makes "reviewed-bytes == run-bytes" hold regardless of which store each blob
        // came from.
        let recomputed = digest::bundle_digest(&manifest)
            .map_err(|r| AuthorityError::integrity(RenderPathRejected(r)))?;
        if recomputed != expected_bundle_digest {
            return Err(AuthorityError::integrity(RenderDigestMismatch));
        }
        files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Ok(RenderedBundle {
            files,
            bundle_digest: recomputed,
        })
    })
    .await
}

#[derive(Debug, thiserror::Error)]
#[error("a present object's git locator does not resolve to its content id")]
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
#[error("a first-parent log hop has no version row in this bundle")]
struct LogHopWithoutRow;

#[derive(Debug, thiserror::Error)]
#[error("recomputed bundle digest does not match the pinned digest")]
struct RenderDigestMismatch;

#[derive(Debug, thiserror::Error)]
#[error("a rendered bundle path was rejected by the canonical rules: {0:?}")]
struct RenderPathRejected(RejectReason);
