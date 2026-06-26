//! Typed read/write of the persisted sidecar documents (lock / map / sync), atomic on write and
//! fail-closed on an unknown schema version.

use std::path::Path;

use serde::Serialize;
use serde::de::DeserializeOwned;
use topos_types::SCHEMA_VERSION;

use crate::atomic::{atomic_write, load_versioned};
use crate::error::ClientError;
use crate::fs_seam::FsOps;

/// Serialize a document (pretty + trailing newline — the committed on-disk shape) and write it atomically.
///
/// # Errors
/// [`ClientError::Corrupt`] if serialization fails; otherwise the [`FsOps`] failure.
pub(crate) fn write_doc<T: Serialize>(
    fs: &dyn FsOps,
    target: &Path,
    doc: &T,
) -> Result<(), ClientError> {
    let mut bytes = serde_json::to_vec_pretty(doc)
        .map_err(|e| ClientError::Corrupt(format!("serialize: {e}")))?;
    bytes.push(b'\n');
    atomic_write(fs, target, &bytes)
}

/// Read + parse a document, returning `None` if the file does not exist. Fails closed on an
/// unknown/newer `schema_version` (never silently parsed).
///
/// # Errors
/// As [`load_versioned`], plus the [`FsOps`] read failure.
pub(crate) fn read_doc<T: DeserializeOwned>(
    fs: &dyn FsOps,
    path: &Path,
) -> Result<Option<T>, ClientError> {
    match fs.read_opt(path)? {
        None => Ok(None),
        Some(bytes) => Ok(Some(load_versioned::<T>(&bytes, SCHEMA_VERSION)?)),
    }
}
