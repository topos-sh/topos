//! The size-routed large-object offload seam — **declared, unwired** this increment.
//!
//! Every file currently lives in the embedded git object store as a real content-addressed blob, with
//! **no size cap and no Git-LFS pointer files**. Because identity is recomputed over real bytes (the
//! kernel sha256), *which* store physically holds a blob never changes its `blob_id`, the
//! `bundle_digest`, or any `version_id` — so a later size-routed backend is a pure drop-in behind this
//! one trait, client-invisible.
//!
//! The v0 backend would be a local sharded content-addressed directory (`objects/aa/bb/<sha256>`); the
//! deferred remote backend an S3-compatible object store. Both are keyed by the **same**
//! `blob_id = sha256(raw bytes)`. None of that ships here — this file pins the surface so the offload
//! lands without touching identity, fixtures, or the read path.

use crate::error::GitstoreError;

/// A content-addressed byte store keyed by `blob_id = sha256(raw bytes)`, with verify-on-read and a
/// crash-safe two-phase install (`temp → fsync → recompute-sha256 == blob_id → commit`). **No impl is
/// wired this increment** — the trait exists so the deferred offload is a drop-in.
pub trait LargeObjectStore {
    /// Store `bytes` under `blob_id` (the caller guarantees `blob_id == sha256(bytes)`).
    ///
    /// # Errors
    /// Implementation-defined I/O / integrity failures.
    fn put(&self, blob_id: [u8; 32], bytes: &[u8]) -> Result<(), GitstoreError>;

    /// Fetch the bytes for `blob_id`, **re-verifying** `sha256(bytes) == blob_id` before returning.
    ///
    /// # Errors
    /// Implementation-defined; a verify failure is fatal.
    fn get(&self, blob_id: [u8; 32]) -> Result<Vec<u8>, GitstoreError>;

    /// Whether `blob_id` is present.
    ///
    /// # Errors
    /// Implementation-defined I/O failures.
    fn exists(&self, blob_id: [u8; 32]) -> Result<bool, GitstoreError>;

    /// Remove `blob_id` (GC).
    ///
    /// # Errors
    /// Implementation-defined I/O failures.
    fn delete(&self, blob_id: [u8; 32]) -> Result<(), GitstoreError>;
}
