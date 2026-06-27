//! The one crash-safe write primitive + the fail-closed schema-migration dispatch.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::error::ClientError;
use crate::fs_seam::FsOps;

/// The suffix recovery sweeps for a temp file a faulted [`atomic_write`] may have left pre-rename.
pub(crate) const TMP_SUFFIX: &str = ".tmp";

/// The one crash-safe write sequence: write `bytes` to `tmp` → fsync the file → atomic rename onto
/// `target` → fsync the dir. After any fault `target` is byte-for-byte the **pre** or **post** state —
/// never torn. The temp path is the caller's, so a foreign-file writer can pick a unique, namespaced
/// name (never the user's `<file>.tmp`); `target`'s parent must already exist. This is the single dance
/// — [`atomic_write`] and the harness config writer both go through it, so it cannot drift.
///
/// # Errors
/// Propagates the underlying [`FsOps`] failure (which the crash gate injects).
pub(crate) fn atomic_write_at(
    fs: &dyn FsOps,
    target: &Path,
    tmp: &Path,
    bytes: &[u8],
) -> io::Result<()> {
    fs.write_temp(tmp, bytes)?;
    fs.fsync_file(tmp)?;
    fs.rename(tmp, target)?;
    if let Some(dir) = target.parent() {
        fs.fsync_dir(dir)?;
    }
    Ok(())
}

/// Crash-safely write `bytes` to `target` under `~/.topos/` (temp = `target` + [`TMP_SUFFIX`], same
/// dir). The caller serializes + validates `bytes` in memory first, so a malformed value never reaches
/// disk.
///
/// # Errors
/// Propagates the underlying [`FsOps`] failure (which the crash gate injects).
pub(crate) fn atomic_write(fs: &dyn FsOps, target: &Path, bytes: &[u8]) -> Result<(), ClientError> {
    Ok(atomic_write_at(fs, target, &temp_path(target), bytes)?)
}

/// The temp path for a target: the same path with [`TMP_SUFFIX`] appended (same directory, so the rename
/// is same-filesystem; a recognizable suffix recovery can sweep).
pub(crate) fn temp_path(target: &Path) -> PathBuf {
    let mut s: OsString = target.as_os_str().to_owned();
    s.push(TMP_SUFFIX);
    PathBuf::from(s)
}

/// Deserialize a persisted document **fail-closed** on its `schema_version`.
///
/// The `schema_version` is probed FIRST; a value newer than `max` is **never** handed to serde (a newer
/// client wrote it — the caller must report "upgrade required", never silently parse or delete it). A
/// missing/non-integer `schema_version`, or a value below the floor, is rejected too. Real `vN-1 → vN`
/// migrations slot into the `1..=max` arm when a v2 schema exists.
///
/// # Errors
/// [`ClientError::UnknownSchemaVersion`] for a newer doc; [`ClientError::UnsupportedLegacy`] below the
/// floor; [`ClientError::Corrupt`] if the probe or the full parse fails.
pub(crate) fn load_versioned<T: DeserializeOwned>(
    bytes: &[u8],
    max: u32,
) -> Result<T, ClientError> {
    #[derive(Deserialize)]
    struct Probe {
        schema_version: u32,
    }
    let probe: Probe = serde_json::from_slice(bytes)
        .map_err(|e| ClientError::Corrupt(format!("missing/invalid schema_version: {e}")))?;
    match probe.schema_version {
        0 => Err(ClientError::UnsupportedLegacy { found: 0 }),
        v if v <= max => serde_json::from_slice(bytes)
            .map_err(|e| ClientError::Corrupt(format!("document parse: {e}"))),
        v => Err(ClientError::UnknownSchemaVersion { found: v, max }),
    }
}
