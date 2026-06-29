//! The skill-scoped object read — the one auditable access surface.
//!
//! Authorization is one database join that yields a *witness* commit (or nothing); only then is the
//! per-workspace git store touched, to fetch the bytes by content id. There is no read-by-bare-hash
//! path anywhere, and the two outcomes are kept textually separate so the distinction cannot rot: an
//! empty join is the single not-found; a store failure on an already-authorized object is a corruption
//! alarm, never a not-found.

use std::collections::HashMap;

use topos_core::digest::{self, ManifestEntry, RejectReason};
use topos_gitstore::{LargeObjectStore, RenderedBundle, RenderedFile};

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{ObjectId, Principal, SkillId, WorkspaceId};
use crate::sqlite::Location;

pub(crate) async fn read_object(
    authority: &Authority,
    principal: &Principal,
    ws: &WorkspaceId,
    skill: &SkillId,
    object_id: ObjectId,
) -> Result<Vec<u8>> {
    // Step one (async DB): authorize. The witness commit proves BOTH facts at once — the principal is
    // rostered for the skill, and that skill reaches the object. The borrow on the database is released
    // before the synchronous store read below (no git borrow ever crosses an await).
    let witness = match authority
        .db()
        .authorize_object_read(ws, skill, principal, object_id)
        .await?
    {
        Some(witness) => witness,
        // Not rostered, the skill does not reach the object, or the object does not exist — all one
        // indistinguishable not-found.
        None => return Err(AuthorityError::NotFound),
    };

    // Step two: fetch + verify the bytes from the store the database records, dispatching on `location`. The
    // witness already proved reachability, so a clean run has no benign miss: a post-authz failure is a
    // provenance/store divergence (corruption) → an Integrity fault, kept distinct from the not-found path
    // (so the indistinguishable 404 holds across the large-object surface), never by bare hash.
    let fetched = match authority.db().object_dispatch(ws, object_id).await? {
        // Offloaded: fetch from the large store (its `get` re-verifies sha256 == object_id).
        Some((Location::LargeLocal, _)) => authority
            .large_store(ws)
            .get(object_id.0)
            .map_err(AuthorityError::integrity),
        // Git-resident: read the loose object DIRECTLY by its locator and re-verify the content id — NOT a
        // whole-version-tree walk, which would fault on an offloaded sibling's absent git object in a mixed
        // bundle before reaching the requested blob.
        Some((Location::Git, git_oid)) => {
            let store = authority.open_store(ws)?;
            store
                .read_git_blob_verified(git_oid)
                .map_err(AuthorityError::integrity)
                .and_then(|(bytes, content_sha256)| {
                    if content_sha256 == object_id.0 {
                        Ok(bytes)
                    } else {
                        Err(AuthorityError::integrity(GitLocatorMismatch))
                    }
                })
        }
        // No live presence row: a legacy straight-to-git object — its version is all-git, so the tree walk is
        // safe — read by content id from the witness version. (A reclaimed object also lands here, because
        // `object_dispatch` filters `status = 'present'`; the re-authorize guard below catches that case.)
        None => {
            let store = authority.open_store(ws)?;
            store
                .read_object_in_version(witness.0, object_id.0)
                .map_err(AuthorityError::integrity)
        }
    };

    // Re-authorize-on-miss (the read-time TOCTOU guard). The authorization above and this fetch are two
    // steps; between them a proposal can go stale (an eventless derived transition — a concurrent publish
    // advances `current`) or be rejected, and a GC can then reclaim the proposal's now-unrooted unique bytes.
    // Any of the fetch arms would then fault Integrity over bytes that are gone BY DESIGN, not corrupt. So on
    // a post-authz failure, re-run the authorization: if the object is no longer authorized, it was legitimately
    // reclaimed → the indistinguishable `NotFound` (404), preserving "reclaimed ⇒ 404, never Integrity" across
    // the window. A still-authorized object that fails to load IS genuine corruption → the Integrity fault stands.
    if let Err(AuthorityError::Integrity(_)) = &fetched
        && authority
            .db()
            .authorize_object_read(ws, skill, principal, object_id)
            .await?
            .is_none()
    {
        return Err(AuthorityError::NotFound);
    }
    fetched
}

#[derive(Debug, thiserror::Error)]
#[error("a present object's git locator does not resolve to its content id")]
struct GitLocatorMismatch;

/// Assemble + verify a whole bundle for a version, dispatching each file to the store the database records.
///
/// **Tree-driven** — the fenced migrate writes no `commit_object` edges, so render anchors on the version's
/// git **tree structure** (`(path, mode, git_oid)` per file), not reachability. The offloaded subset is the
/// workspace's present `large-local` rows, joined in memory by `git_oid → object_id`; each file's bytes come
/// from the large store (offloaded) or git (git-resident / legacy), re-verified to its content id; the
/// recomputed `bundle_digest` must then equal the pin. Offload never forks identity (the digest is over real
/// bytes) and never adds a pointer object. **Authorization is the caller's job** (mirrors [`read_object`]:
/// authorize first, then assemble) — this is the assembly primitive the future read-bundle / review-diff op
/// builds on; it is test-driven this increment (no public verb yet), like the rest of the fence.
///
/// # Errors
/// [`AuthorityError::Integrity`] if a file's bytes are missing/corrupt in either store, a stored path is
/// illegal, or the recomputed digest does not match `expected_bundle_digest`; [`AuthorityError::Internal`]
/// on a database fault.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn render_version(
    authority: &Authority,
    ws: &WorkspaceId,
    version_id: [u8; 32],
    expected_bundle_digest: [u8; 32],
) -> Result<RenderedBundle> {
    let store = authority.open_store(ws)?;
    let structure = store
        .read_tree_structure(version_id)
        .map_err(AuthorityError::integrity)?;
    // The offloaded set for this workspace: git_oid -> object_id (small — big blobs are rare). A git-resident
    // leaf is absent from this map and recovers its id by rehashing the git blob, with no DB dependency.
    let offloaded: HashMap<[u8; 20], [u8; 32]> = authority
        .db()
        .large_local_objects(ws)
        .await?
        .into_iter()
        .map(|(git_oid, object_id)| (git_oid, object_id.0))
        .collect();

    let mut files = Vec::with_capacity(structure.len());
    let mut manifest = Vec::with_capacity(structure.len());
    for leaf in structure {
        let (bytes, content_sha256) = match offloaded.get(&leaf.git_oid) {
            Some(&object_id) => {
                // Offloaded: fetch from the large store (its `get` re-verifies sha256 == object_id).
                let bytes = authority
                    .large_store(ws)
                    .get(object_id)
                    .map_err(AuthorityError::integrity)?;
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

    // Recompute the consent digest over the assembled real bytes and assert it equals the pin — the integrity
    // gate that makes "reviewed-bytes == run-bytes" hold regardless of which store each blob came from.
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
}

#[derive(Debug, thiserror::Error)]
#[error("recomputed bundle digest does not match the pinned digest")]
struct RenderDigestMismatch;

#[derive(Debug, thiserror::Error)]
#[error("a rendered bundle path was rejected by the canonical rules: {0:?}")]
struct RenderPathRejected(RejectReason);
