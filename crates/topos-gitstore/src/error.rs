//! The two typed error families. Raw `gix` error strings are captured for diagnostics only; the client
//! maps these typed variants to clean wire codes and never echoes the inner string to a user surface.

use topos_core::digest::RejectReason;

/// A failure writing to (or addressing) the store. Distinct from [`VerifyError`]: these are write-side /
/// structural faults, not the read-side integrity check.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GitstoreError {
    /// A bundle path failed the kernel's canonical reject rules (absolute / `..` / NUL / collision / …).
    #[error("bundle path rejected by the canonical rules: {0:?}")]
    Reject(RejectReason),
    /// The caller-supplied `version_id` does not equal the kernel `commit_id` recomputed from the same
    /// arguments — a ref that would lie about its own identity. Refused before any ref is written.
    #[error("supplied version_id does not match the recomputed commit id")]
    VersionMismatch,
    /// A parent `version_id` named at commit time is not present in this store.
    #[error("a parent version is not present in this store")]
    MissingParent,
    /// An underlying `gix` object/ref operation failed.
    #[error("git object store error: {0}")]
    Gix(String),
    /// A filesystem operation on the store failed.
    #[error("store io error: {0}")]
    Io(String),
    /// A content-addressed large object's bytes do not match their `blob_id` (`sha256(bytes) != blob_id`):
    /// either a caller mis-declared the id on `put`, or `get`'s verify-on-read found at-rest corruption.
    /// The bytes are never installed or returned — placement never weakens the byte-exact guarantee.
    #[error("large-object bytes do not match their content id")]
    BlobIntegrity,
}

/// A failure reading + **authenticating** a stored version. Every variant means "do not trust these
/// bytes" — the caller refuses to materialize anything.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum VerifyError {
    /// No `refs/topos/versions/<version_id>` exists — the version was never stored here.
    #[error("the requested version is not present in this store")]
    MissingVersion,
    /// A commit, tree, or blob the version references is missing from the object database.
    #[error("a referenced object is missing from the store")]
    MissingObject,
    /// A stored tree entry name is not valid UTF-8 (the scanner rejects these at write time; a stored
    /// one means corruption/forgery).
    #[error("a stored tree entry has a non-UTF-8 name")]
    NonUtf8Name,
    /// A stored tree entry is neither a regular nor an executable file blob (a symlink/gitlink/… that
    /// the scanner would never have written).
    #[error("a stored tree entry is neither a regular nor an executable file blob")]
    NonBlobEntry,
    /// The digest recomputed from the stored bytes does not equal the digest the caller is pinned to —
    /// the integrity stop a corrupted or forged object trips.
    #[error("recomputed bundle digest does not match the pinned digest")]
    BundleDigestMismatch,
    /// Two version refs point at one git commit, making first-parent lineage ambiguous.
    #[error("two version refs point at one commit (ambiguous lineage)")]
    DuplicateLineage,
    /// No blob in the requested version's tree re-hashes to the requested object id — the object is
    /// not reachable in that version. Distinct from [`Self::MissingObject`] (a referenced object
    /// absent from the object database, i.e. corruption): this is a clean "not in this tree" answer.
    #[error("the requested object is not present in this version")]
    ObjectNotInVersion,
    /// The stored commit/tree could not be decoded into the expected shape.
    #[error("a stored object is malformed: {0}")]
    Malformed(String),
    /// An underlying `gix` object/ref operation failed.
    #[error("git object store error: {0}")]
    Gix(String),
}
