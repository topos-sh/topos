//! `state/sync_status.json` — the per-workspace delivery/report freshness record.
//!
//! Written by the delivery-driven reconcile on every successful delivery fetch and applied-state
//! report; read by the hook's staleness warning (`update --quiet`) and by `auth status` (the
//! reporting posture). A PLAIN document — timestamps and the workspace's staleness window, never a
//! secret — through the ordinary crash-safe [`crate::doc`] writers (atomic, fail-closed schema).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;

use crate::doc;
use crate::error::ClientError;
use crate::fs_seam::FsOps;
use crate::sidecar::Layout;

/// The whole document: one entry per enrolled workspace, keyed by workspace id (a `BTreeMap`, so
/// the on-disk bytes are deterministic).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncStatus {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub workspaces: BTreeMap<String, WorkspaceSync>,
}

/// One workspace's freshness: when this device last received a delivery answer, when it last
/// reported its applied state, and the staleness window the workspace policy declares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceSync {
    /// When the last successful `GET /delivery` answered (epoch millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery_at: Option<i64>,
    /// When the last successful `PUT /report` landed (epoch millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report_at: Option<i64>,
    /// The workspace's staleness window (ms) — a device whose last delivery is older is stale.
    #[serde(default)]
    pub staleness_window_ms: u64,
}

/// Read the document, or an empty default when absent. Fail-closed on a newer `schema_version`
/// (the shared [`doc::read_doc`] dispatch).
pub(crate) fn read(fs: &dyn FsOps, layout: &Layout) -> Result<SyncStatus, ClientError> {
    Ok(doc::read_doc(fs, &layout.sync_status_path())?.unwrap_or_default())
}

/// Merge `updates` into the document and write it (read-modify-write; the reconcile is the only
/// writer in practice, so no cross-process lock is taken — the file is advisory freshness, and the
/// atomic write keeps it never-torn). Each update replaces its workspace's entry wholesale.
pub(crate) fn record(
    fs: &dyn FsOps,
    layout: &Layout,
    updates: &[(String, WorkspaceSync)],
) -> Result<(), ClientError> {
    if updates.is_empty() {
        return Ok(());
    }
    let mut status = read(fs, layout)?;
    status.schema_version = PERSISTED_SCHEMA_VERSION;
    for (ws, entry) in updates {
        status.workspaces.insert(ws.clone(), entry.clone());
    }
    fs.create_dir_all(&layout.state_dir())?;
    doc::write_doc(fs, &layout.sync_status_path(), &status)
}

/// Whether a workspace's last delivery is STALE against its recorded window: `true` only when a
/// last-delivery time exists, the window is non-zero, and `now` is past `last + window`. A
/// workspace never yet delivered (no record) is NOT stale — there is nothing to be stale FROM, and
/// warning on a fresh install would train people to ignore the line.
pub(crate) fn is_stale(entry: Option<&WorkspaceSync>, now_millis: i64) -> bool {
    let Some(e) = entry else {
        return false;
    };
    let (Some(last), window) = (e.last_delivery_at, e.staleness_window_ms) else {
        return false;
    };
    if window == 0 {
        return false;
    }
    let Ok(window) = i64::try_from(window) else {
        return false;
    };
    now_millis.saturating_sub(last) > window
}

/// A human-readable duration for the staleness warning line (`3d`, `5h`, `12m` — coarse on
/// purpose: the line is a nudge, not telemetry).
pub(crate) fn human_duration(millis: i64) -> String {
    let mins = millis.max(0) / 60_000;
    if mins >= 60 * 24 {
        format!("{}d", mins / (60 * 24))
    } else if mins >= 60 {
        format!("{}h", mins / 60)
    } else {
        format!("{mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs_seam::RealFs;

    fn scratch(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-ss-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn record_merges_per_workspace_and_survives_reload() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("merge"));
        record(
            &fs,
            &layout,
            &[(
                "w_a".into(),
                WorkspaceSync {
                    last_delivery_at: Some(1_000),
                    last_report_at: Some(1_001),
                    staleness_window_ms: 604_800_000,
                },
            )],
        )
        .unwrap();
        // A second workspace merges in; the first survives untouched.
        record(
            &fs,
            &layout,
            &[(
                "w_b".into(),
                WorkspaceSync {
                    last_delivery_at: Some(2_000),
                    last_report_at: None,
                    staleness_window_ms: 1_000,
                },
            )],
        )
        .unwrap();
        let status = read(&fs, &layout).unwrap();
        assert_eq!(status.workspaces.len(), 2);
        assert_eq!(status.workspaces["w_a"].last_delivery_at, Some(1_000));
        assert_eq!(status.workspaces["w_b"].staleness_window_ms, 1_000);
        // An empty update writes nothing (and needs no state dir).
        record(&fs, &Layout::new(&scratch("noop")), &[]).unwrap();
    }

    #[test]
    fn staleness_needs_a_record_a_window_and_an_expired_clock() {
        let fresh = WorkspaceSync {
            last_delivery_at: Some(1_000),
            last_report_at: None,
            staleness_window_ms: 10_000,
        };
        // Inside the window — not stale; past it — stale.
        assert!(!is_stale(Some(&fresh), 5_000));
        assert!(is_stale(Some(&fresh), 12_000));
        // No record at all, no delivery yet, or a zero window: never stale.
        assert!(!is_stale(None, i64::MAX));
        assert!(!is_stale(Some(&WorkspaceSync::default()), i64::MAX));
        let zero_window = WorkspaceSync {
            last_delivery_at: Some(0),
            staleness_window_ms: 0,
            ..WorkspaceSync::default()
        };
        assert!(!is_stale(Some(&zero_window), i64::MAX));
    }

    #[test]
    fn human_duration_is_coarse() {
        assert_eq!(human_duration(30_000), "0m");
        assert_eq!(human_duration(5 * 60_000), "5m");
        assert_eq!(human_duration(3 * 3_600_000), "3h");
        assert_eq!(human_duration(9 * 86_400_000), "9d");
        assert_eq!(human_duration(-5), "0m");
    }
}
