//! The candidate-upload types — a full bundle's files + lineage, as the commit/publish paths
//! receive them.
//!
//! Every file carries its raw bytes (there is **no** blob-id field — no reference-by-id; the vault
//! rehashes every byte). The server-rehash + canonical-rule enforcement lives in the
//! ingest → migrate → one-transaction commit path (`lifecycle` + `commit`); this module is just the
//! shared input shape.

use topos_core::digest::FileMode;

use crate::id::CommitId;

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

/// A full candidate bundle: every file's bytes, the declared parent (bound into the recomputed id,
/// so a lie changes the id), and the attribution + message recorded in the commit frame.
#[derive(Debug, Clone)]
pub struct CandidateUpload {
    /// Every file in the candidate bundle.
    pub files: Vec<UploadedFile>,
    /// The candidate's parent version (`None` for a genesis commit). Must already exist as a
    /// version of the target bundle.
    pub parent: Option<CommitId>,
    /// The attribution display string recorded verbatim as the commit frame's author (and the
    /// version row's `author_display`). Pass-through from the app; shape-checked, never interpreted.
    pub attribution: String,
    /// The commit message (title + body composed into one string).
    pub message: String,
}
