//! Full-tree upload + server rehash — the publish-side confused-deputy guard.
//!
//! The server recomputes every id from the uploaded bytes (a client id is never trusted, and there is
//! no reference-by-id — every byte must be uploaded). Objects are written to the per-workspace store,
//! then — only after the authoritative roster check, in one transaction — the commit's provenance and
//! reachability are recorded. The edges are derived internally from the recomputed bytes, never from
//! client input. No pointer moves; the receipt is a pure function of the uploaded tree, so a dedup hit
//! and a first upload are indistinguishable.

use std::collections::BTreeSet;

use topos_core::digest::{self, FileMode};
use topos_core::sign::{self, Commit};
use topos_gitstore::{GitstoreError, ImportFile};

use crate::authority::Authority;
use crate::error::{AuthorityError, Result};
use crate::id::{CommitId, ObjectId, Principal, SkillId, WorkspaceId};
use crate::sqlite::RecordOutcome;

/// One file in a candidate upload — its bundle-relative path, mode, and raw bytes. There is **no**
/// blob-id field: every byte must be uploaded (no reference-by-id).
#[derive(Debug, Clone)]
pub struct UploadedFile {
    /// The bundle-relative, forward-slash path.
    pub path: String,
    /// The file mode (regular or executable).
    pub mode: FileMode,
    /// The raw file bytes.
    pub bytes: Vec<u8>,
}

/// A full candidate bundle: every file's bytes, the candidate commit's declared parents (bound into
/// the recomputed id, so a lie changes the id), and the author + message.
#[derive(Debug, Clone)]
pub struct CandidateUpload {
    /// Every file in the candidate bundle.
    pub files: Vec<UploadedFile>,
    /// The candidate commit's parents (`0` for a genesis publish, `1` for a normal publish/revert, `2`
    /// for an author merge). Each must already be present in the workspace's store.
    pub parents: Vec<CommitId>,
    /// The author device id recorded in the commit frame.
    pub author: String,
    /// The commit message (title + body composed into one string).
    pub message: String,
}

/// The receipt for a successful upload — a pure function of the uploaded tree, identical whether the
/// bytes were new or already present (dedup is invisible).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UploadReceipt {
    /// The recomputed commit id (= `version_id`) of the candidate.
    pub version_id: CommitId,
    /// The recomputed byte-exact consent digest of the candidate's bundle.
    pub bundle_digest: [u8; 32],
    /// The **logical** uploaded byte count (the sum of the uploaded file lengths) — never the physical
    /// stored bytes, so dedup is not observable through accounting.
    pub logical_bytes: u64,
}

pub(crate) async fn upload_candidate(
    authority: &Authority,
    principal: &Principal,
    ws: &WorkspaceId,
    skill: &SkillId,
    candidate: CandidateUpload,
) -> Result<UploadReceipt> {
    // A skill bundle must contain at least one file. The git store is a dumb layer that happily snapshots
    // a zero-entry tree, so the authority enforces the no-empty-bundle policy itself (it cannot trust the
    // client scanner to have done so) — before writing any object or recording any provenance.
    if candidate.files.is_empty() {
        return Err(AuthorityError::RejectedUpload(
            "a skill bundle must contain at least one file".to_owned(),
        ));
    }

    // Logical accounting is a pure function of the upload (the sum of uploaded file lengths), computed
    // up front and never from storage, so it is identical on a dedup hit and on a first upload.
    let logical_bytes: u64 = candidate.files.iter().map(|f| f.bytes.len() as u64).sum();

    // The kernel import view over the uploaded bytes. The canonical reject rules fire ONCE, inside the
    // kernel via `write_bundle` below — they are never re-implemented here.
    let import: Vec<ImportFile<'_>> = candidate
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();

    // A cheap roster pre-read: a non-load-bearing fail-fast that bounds orphan writes to principals
    // actually rostered for the skill. It does NOT change the response shape — the authoritative check
    // is inside the recording transaction, which is what closes the time-of-check/time-of-use window.
    if !authority.db().is_rostered(ws, skill, principal).await? {
        return Err(AuthorityError::Denied);
    }

    // Sync git phase: write the objects + the commit. The store re-derives the id from the same bytes
    // and refuses a lying ref; we compute the id ourselves and never trust a client value.
    let store = authority.store_for_write(ws)?;
    let tree = store.write_bundle(&import).map_err(map_upload_reject)?;
    let parents: Vec<[u8; 32]> = candidate.parents.iter().map(|c| c.0).collect();
    let version_id = sign::commit_id(&Commit {
        parents: &parents,
        tree: tree.bundle_digest,
        author: &candidate.author,
        message: &candidate.message,
    })
    .map_err(|e| AuthorityError::RejectedUpload(format!("invalid commit frame: {e:?}")))?;
    store
        .commit(
            version_id,
            &parents,
            &tree,
            &candidate.author,
            &candidate.message,
        )
        .map_err(map_upload_reject)?;

    // Derive the reachability edges INTERNALLY from the recomputed bytes (never from client input): the
    // distinct blob ids of the uploaded files (a blob at two paths is one edge).
    let mut seen = BTreeSet::new();
    let object_ids: Vec<ObjectId> = candidate
        .files
        .iter()
        .filter_map(|f| {
            let id = digest::sha256(&f.bytes);
            seen.insert(id).then_some(ObjectId(id))
        })
        .collect();

    // Record provenance + reachability under the authoritative roster check (one transaction). A deny
    // records nothing the access rule could later read; the receipt is computed purely from the upload.
    match authority
        .db()
        .record_authorized_commit(
            ws,
            skill,
            principal,
            CommitId(version_id),
            &object_ids,
            tree.bundle_digest,
        )
        .await?
    {
        RecordOutcome::Recorded => Ok(UploadReceipt {
            version_id: CommitId(version_id),
            bundle_digest: tree.bundle_digest,
            logical_bytes,
        }),
        // Both denials return the SAME error (do not name the owning skill).
        RecordOutcome::NotRostered | RecordOutcome::OwnedByOtherSkill => {
            Err(AuthorityError::Denied)
        }
    }
}

/// Map a git-store write failure to the upload's typed error. A canonical-rule reject, a missing
/// parent, or an id mismatch is the client's problem (a rejected upload); a low-level fault is internal.
fn map_upload_reject(e: GitstoreError) -> AuthorityError {
    match e {
        GitstoreError::Reject(reason) => {
            AuthorityError::RejectedUpload(format!("canonical rule violated: {reason:?}"))
        }
        GitstoreError::MissingParent => AuthorityError::RejectedUpload(
            "a parent version is not present in this workspace".into(),
        ),
        GitstoreError::VersionMismatch => {
            AuthorityError::RejectedUpload("the commit id does not match the uploaded bytes".into())
        }
        other => AuthorityError::internal(other),
    }
}
