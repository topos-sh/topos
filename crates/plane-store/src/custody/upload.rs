//! The candidate-upload types — a full bundle's files + lineage, as the publish/propose paths receive them.
//!
//! Every file carries its raw bytes (there is **no** blob-id field — no reference-by-id; the server rehashes
//! every byte). The server-rehash + canonical-rule + roster + cross-bundle enforcement lives in the
//! ingest → migrate → one-pointer-move-transaction path (`lifecycle` + `set_current`); this module is just
//! the shared input shape.

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

/// A full candidate bundle: every file's bytes, the candidate commit's declared parents (bound into
/// the recomputed id, so a lie changes the id), and the author + message.
#[derive(Debug, Clone)]
pub struct CandidateUpload {
    /// Every file in the candidate bundle.
    pub files: Vec<UploadedFile>,
    /// The candidate commit's parents (`0` for a genesis publish, `1` for a normal publish/revert/propose,
    /// `2` for an author merge). Each must already be present in the workspace's store.
    pub parents: Vec<CommitId>,
    /// The author device id recorded in the commit frame.
    pub author: String,
    /// The commit message (title + body composed into one string).
    pub message: String,
}
