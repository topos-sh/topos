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

/// Append one structured ERROR event (`action: "error"`) — the diagnostics sink the redacted user
/// surfaces point at (`error: <fixed text>` + `details: ~/.topos/log.jsonl`). Carries the verb, the
/// stable wire code, and the FULL `Display` chain ([`crate::error::ClientError::detail`] — secret-free
/// by construction: tokens/keys are redacted at their type, never rendered into an error). Best-effort
/// at every call site (a diagnostics hiccup must never mask the error being reported): the parent dir
/// is ensured, and the returned `bool` says whether the event landed (the TTY pointer prints only then).
pub(crate) fn append_error_event(
    fs: &dyn FsOps,
    path: &Path,
    verb: &str,
    code: &str,
    detail: &str,
    at_millis: u64,
) -> bool {
    let parent_ok = path
        .parent()
        .map(|p| fs.create_dir_all(p).is_ok())
        .unwrap_or(true);
    parent_ok
        && append_event(
            fs,
            path,
            &serde_json::json!({
                "action": "error",
                "verb": verb,
                "code": code,
                "detail": detail,
                "at": at_millis,
            }),
        )
        .is_ok()
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

#[cfg(test)]
mod tests {
    use super::{append_error_event, read_events};
    use crate::fs_seam::RealFs;

    #[test]
    fn append_error_event_lands_the_full_detail() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("topos-logfile-{}-{nanos}", std::process::id()));
        let path = dir.join("log.jsonl");
        let fs = RealFs;
        // The parent dir is ensured (an error can fire before anything created the home).
        assert!(append_error_event(
            &fs,
            &path,
            "add",
            "IO_ERROR",
            "canonicalize /missing/skill: no such file",
            42,
        ));
        let events = read_events(&fs, &path).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["action"], "error");
        assert_eq!(events[0]["verb"], "add");
        assert_eq!(events[0]["code"], "IO_ERROR");
        assert_eq!(events[0]["at"], 42);
        // The detail carries the path/context the user surfaces redact.
        assert!(
            events[0]["detail"]
                .as_str()
                .unwrap()
                .contains("/missing/skill")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
