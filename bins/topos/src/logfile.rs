//! `log.jsonl` — append-only local action events, with torn-tail recovery (a partial trailing line
//! written by an interrupted append is discarded on read and truncated by recovery).

use std::path::Path;

use serde_json::Value;

use crate::atomic::atomic_write;
use crate::error::ClientError;
use crate::fs_seam::FsOps;

/// Append one event as a JSON line (`<json>\n`), fsynced.
///
/// # Errors
/// [`ClientError::Corrupt`] if the event cannot be serialized; otherwise the [`FsOps`] failure.
pub(crate) fn append_event(fs: &dyn FsOps, path: &Path, event: &Value) -> Result<(), ClientError> {
    let mut line = serde_json::to_vec(event).map_err(|e| ClientError::Corrupt(format!("{e}")))?;
    line.push(b'\n');
    fs.append_fsync(path, &line)?;
    Ok(())
}

/// Read all complete events; a partial/unparseable trailing line is skipped (torn-tail tolerant).
///
/// # Errors
/// The [`FsOps`] read failure.
pub(crate) fn read_events(fs: &dyn FsOps, path: &Path) -> Result<Vec<Value>, ClientError> {
    let bytes = fs.read_opt(path)?.unwrap_or_default();
    Ok(bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect())
}

/// Truncate a partial trailing line (an interrupted append) to the last complete record.
/// Idempotent: a clean file is left untouched.
///
/// # Errors
/// The [`FsOps`] failure if the truncate rewrite fails.
pub(crate) fn repair_torn_tail(fs: &dyn FsOps, path: &Path) -> Result<(), ClientError> {
    let Some(bytes) = fs.read_opt(path)? else {
        return Ok(());
    };
    match bytes.iter().rposition(|&b| b == b'\n') {
        // A partial line after the last newline -> drop it.
        Some(pos) if pos + 1 < bytes.len() => atomic_write(fs, path, &bytes[..=pos]),
        // No newline at all but non-empty -> the whole file is one partial line.
        None if !bytes.is_empty() => atomic_write(fs, path, b""),
        _ => Ok(()),
    }
}
