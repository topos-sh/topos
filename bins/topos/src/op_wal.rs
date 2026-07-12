//! The contribute write-ahead log — `ops/<op_id>.json`, one durable [`OpRecord`] per in-flight
//! device-signed write, persisted `0600` BEFORE the first send so an uncertain write replays the SAME
//! `op_id` (the plane returns the byte-identical receipt; no double-advance, no duplicate commit). Modeled
//! on the enrollment WAL ([`crate::enroll`]). This module is the durable-doc layer only; the
//! request-rebuild + send lives in [`crate::ops`]'s contribute helper.

use std::io;

use topos_types::persisted::{OpKind, OpRecord};

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// Parse a canonical hyphenated `op_id` back to the raw 16 bytes the device-op frame binds. Also the
/// path boundary for this module: a canonical hyphenated lowercase UUID is trivially a safe
/// `ops/<op_id>.json` file name, so every join below runs this gate first (a persisted record's `op_id`
/// is a doc value, never trusted raw — the same rule as the skill/workspace ids).
///
/// # Errors
/// [`ClientError::Corrupt`] if the stored `op_id` is not the canonical hyphenated UUID form.
pub(crate) fn op_id_bytes(op_id: &str) -> Result<[u8; 16], ClientError> {
    let parsed = uuid::Uuid::parse_str(op_id)
        .map_err(|_| ClientError::Corrupt("op_id is not a canonical UUID".to_owned()))?;
    // `parse_str` accepts several spellings (braced / URN / simple / uppercase); require the CANONICAL
    // hyphenated lowercase form byte-for-byte, so the id IS exactly the file name it will be joined as.
    if parsed.as_hyphenated().to_string() != op_id {
        return Err(ClientError::Corrupt(
            "op_id is not the canonical hyphenated UUID form".to_owned(),
        ));
    }
    Ok(parsed.into_bytes())
}

/// Write an op record `0600` (BEFORE the first send). Creates `ops/` if absent.
///
/// # Errors
/// An [`FsOps`] write failure; [`ClientError::Corrupt`] for a non-canonical `op_id`.
pub(crate) fn write(fs: &dyn FsOps, layout: &Layout, rec: &OpRecord) -> Result<(), ClientError> {
    op_id_bytes(&rec.op_id)?; // the path gate, before the join
    fs.create_dir_all(&layout.ops_dir())?;
    doc::write_doc_private(fs, &layout.op_path(&rec.op_id), rec)
}

/// Read an op record by id, or `None` if absent. Fail-closed on a permissive secret-file mode.
///
/// # Errors
/// As [`doc::read_doc_private`]; [`ClientError::Corrupt`] for a non-canonical `op_id`.
pub(crate) fn read(
    fs: &dyn FsOps,
    layout: &Layout,
    op_id: &str,
) -> Result<Option<OpRecord>, ClientError> {
    op_id_bytes(op_id)?;
    doc::read_doc_private(fs, &layout.op_path(op_id))
}

/// Delete an op record once its outcome is reconciled. NotFound-tolerant (a double-delete is a no-op).
///
/// # Errors
/// An [`FsOps`] failure other than not-found; [`ClientError::Corrupt`] for a non-canonical `op_id` (a
/// tampered record must never steer the remove outside `ops/`).
pub(crate) fn delete(fs: &dyn FsOps, layout: &Layout, op_id: &str) -> Result<(), ClientError> {
    op_id_bytes(op_id)?;
    match fs.remove_file(&layout.op_path(op_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Find the IN-FLIGHT (no terminal receipt) op for `(workspace, skill)` whose kind is one of `kinds`, if any.
/// Scans `ops/`. Scoping by kind lets each verb resume only ITS own ops; with the per-skill writer lock held
/// across the WAL-write + send, that gives at most one in-flight op per `(skill, kind)`. A corrupt /
/// permissive / unreadable record is SKIPPED (the live op re-mints fresh), never hard-failing the scan.
///
/// # Errors
/// An [`FsOps`] read-dir failure.
pub(crate) fn find_pending_for_skill(
    fs: &dyn FsOps,
    layout: &Layout,
    workspace_id: &str,
    skill_id: &str,
    kinds: &[OpKind],
) -> Result<Option<OpRecord>, ClientError> {
    let dir = layout.ops_dir();
    if !fs.exists(&dir) {
        return Ok(None);
    }
    for path in fs.read_dir(&dir)? {
        let Some(op_id) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        // A corrupt/permissive record is skipped here; the live op surfaces its own state.
        let Ok(Some(rec)) = read(fs, layout, op_id) else {
            continue;
        };
        if rec.last_receipt.is_none()
            && rec.workspace_id == workspace_id
            && rec.skill_id == skill_id
            && kinds.contains(&rec.op)
        {
            return Ok(Some(rec));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};
    use topos_types::{Generation, Receipt};

    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-opwal-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rec(op_id: &str, ws: &str, skill: &str) -> OpRecord {
        OpRecord {
            schema_version: 1,
            op_id: op_id.to_owned(),
            workspace_id: ws.to_owned(),
            skill_id: skill.to_owned(),
            op: OpKind::PublishDirect,
            candidate_commit: "a".repeat(64),
            bundle_digest: "b".repeat(64),
            expected_generation: Generation { epoch: 1, seq: 1 },
            good: None,
            display_name: None,
            channel: None,
            last_receipt: None,
        }
    }

    #[test]
    fn write_read_delete_round_trip() {
        let scratch = Scratch::new();
        let fs = RealFs;
        let layout = Layout::new(&scratch.0);
        let op = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        write(&fs, &layout, &rec(op, "w", "s")).unwrap();
        assert!(read(&fs, &layout, op).unwrap().is_some());
        delete(&fs, &layout, op).unwrap();
        assert!(read(&fs, &layout, op).unwrap().is_none());
        // A second delete is a no-op (NotFound-tolerant).
        delete(&fs, &layout, op).unwrap();
    }

    #[test]
    fn find_pending_matches_only_an_unreceipted_op_for_the_skill() {
        let scratch = Scratch::new();
        let fs = RealFs;
        let layout = Layout::new(&scratch.0);
        let op = "f47ac10b-58cc-4372-a567-0e02b2c3d479";
        write(&fs, &layout, &rec(op, "w_demo", "s_demo")).unwrap();
        // Matches its own (ws, skill).
        assert_eq!(
            find_pending_for_skill(&fs, &layout, "w_demo", "s_demo", &[OpKind::PublishDirect])
                .unwrap()
                .map(|r| r.op_id),
            Some(op.to_owned())
        );
        // A different skill / workspace does not match.
        assert!(
            find_pending_for_skill(&fs, &layout, "w_demo", "s_other", &[OpKind::PublishDirect])
                .unwrap()
                .is_none()
        );
        // A kind this verb does not own does not match (each verb resumes only its own ops).
        assert!(
            find_pending_for_skill(&fs, &layout, "w_demo", "s_demo", &[OpKind::Revert])
                .unwrap()
                .is_none()
        );
        // Once a terminal receipt is recorded, it is no longer "pending".
        let mut done = rec(op, "w_demo", "s_demo");
        done.last_receipt = Some(Receipt {
            schema_version: 1,
            op_id: op.to_owned(),
            command: "publish-direct".to_owned(),
            outcome: topos_types::TerminalOutcome::Ok,
            workspace_id: "w_demo".to_owned(),
            skill_id: Some("s_demo".to_owned()),
            version_id: Some("a".repeat(64)),
            bundle_digest: Some("b".repeat(64)),
            expected_generation: None,
            current_generation: None,
            created_at: "2026-06-30T00:00:00Z".to_owned(),
            details: None,
        });
        write(&fs, &layout, &done).unwrap();
        assert!(
            find_pending_for_skill(&fs, &layout, "w_demo", "s_demo", &[OpKind::PublishDirect])
                .unwrap()
                .is_none(),
            "a receipted op is no longer in-flight"
        );
    }

    #[test]
    fn find_pending_on_empty_ops_dir_is_none() {
        let scratch = Scratch::new();
        let fs = RealFs;
        let layout = Layout::new(&scratch.0);
        assert!(
            find_pending_for_skill(&fs, &layout, "w", "s", &[OpKind::PublishDirect])
                .unwrap()
                .is_none()
        );
    }
}
