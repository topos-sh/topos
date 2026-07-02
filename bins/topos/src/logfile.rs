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
/// by construction: tokens/keys are redacted at their type, never rendered into an error). A failure
/// scoped to ONE skill (a sweep's isolated per-skill failure) carries `skill_id` as a first-class
/// field, so `topos log <skill>`'s `skill_id` filter surfaces it; a verb-level failure passes `None`
/// and the field is omitted. Best-effort at every call site (a diagnostics hiccup must never mask the
/// error being reported): the parent dir is ensured, and the returned `bool` says whether the event
/// landed (the TTY pointer prints only then).
pub(crate) fn append_error_event(
    fs: &dyn FsOps,
    path: &Path,
    verb: &str,
    code: &str,
    detail: &str,
    skill_id: Option<&str>,
    at_millis: u64,
) -> bool {
    let parent_ok = path
        .parent()
        .map(|p| fs.create_dir_all(p).is_ok())
        .unwrap_or(true);
    let mut event = serde_json::json!({
        "action": "error",
        "verb": verb,
        "code": code,
        "detail": detail,
        "at": at_millis,
    });
    if let (Some(sid), Some(obj)) = (skill_id, event.as_object_mut()) {
        obj.insert("skill_id".to_owned(), Value::String(sid.to_owned()));
    }
    parent_ok && append_event(fs, path, &event).is_ok()
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
            None,
            42,
        ));
        // A skill-scoped failure (the sweep's isolation path) carries the id as a first-class field.
        assert!(append_error_event(
            &fs,
            &path,
            "pull",
            "CORRUPT_STATE",
            "skill topos_abc: sync.json unreadable",
            Some("topos_abc"),
            43,
        ));
        let events = read_events(&fs, &path).unwrap();
        assert_eq!(events.len(), 2);
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
        // Verb-level: no skill_id field at all; skill-scoped: the first-class field is present.
        assert!(events[0].get("skill_id").is_none());
        assert_eq!(events[1]["skill_id"], "topos_abc");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
