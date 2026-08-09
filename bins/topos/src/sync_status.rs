//! `state/sync_status.json` — the per-workspace delivery/report freshness record.
//!
//! Written by the delivery-driven reconcile on every successful delivery fetch and applied-state
//! report — and, between sweeps, advanced by a pointer move this device itself landed ([`record_served`],
//! the remote-tracking half of read-your-writes); read by the hook's staleness warning
//! (`update --quiet`), by `auth status` (the reporting posture), and by the offline `list`/`status`
//! surfaces. A PLAIN document — timestamps and the workspace's staleness window, never a
//! secret — through the ordinary crash-safe [`crate::doc`] writers (atomic, fail-closed schema).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::results::ExchangeFault;

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
    /// The server host the workspace lives on (the manifest grammar's host half) — lets the
    /// offline surfaces (and the cache-backed follow seam) answer canonical references without a
    /// session read. Absent on records written before the session model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// The workspace's ADDRESS slug (the manifest grammar's middle).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// When the last successful `GET /delivery` answered (epoch millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_delivery_at: Option<i64>,
    /// When the last successful `PUT /report` landed (epoch millis).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_report_at: Option<i64>,
    /// The workspace's staleness window (ms) — a device whose last delivery is older is stale.
    #[serde(default)]
    pub staleness_window_ms: u64,
    /// The last delivery's per-skill cache — what the plane last SERVED this device (keyed by skill
    /// id). Additive + cheap: it lets `list` answer OFFLINE two questions the follow-state alone
    /// cannot — `behind` (the served version differs from the locally-applied one) and
    /// `removed-upstream` (the skill was withdrawn) — and it records each skill's `via` channels so
    /// `list --channel <name>` filters offline. Absent keys just narrow what a column can say, exactly
    /// as the timestamps do; a full reconcile REPLACES this map, so a re-delivered skill self-heals.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub delivered: BTreeMap<String, DeliveredSkill>,
    /// The caller's DECLINED bundles at the last delivery (skill id → catalog name) — what the
    /// offline disclosures read ("declined on the web, delivered here by your manifest").
    /// Replaced wholesale by each successful delivery.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub declined: BTreeMap<String, String>,
    /// The LAST exchange's fault, when it did not land — the durable half of the quiet hook's
    /// transient warning, so a later read (`log`) can still say the workspace this copy comes from
    /// did not answer. A successful delivery REPLACES the whole entry, so a landed exchange clears
    /// it by construction; absent therefore means "the last exchange landed".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_exchange_fault: Option<ExchangeFault>,
}

/// One skill's last-delivery cache entry (see [`WorkspaceSync::delivered`]). Written by the reconcile,
/// read only by offline `list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct DeliveredSkill {
    /// The catalog's user-facing name at the last delivery — the offline name↔id join the
    /// cache-backed follow seam and `status`/`list` read. Absent on pre-session records.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Whether the bundle was effectively `reviewed` at the last delivery (the offline publish
    /// preflight; the server re-decides authoritatively on every write).
    #[serde(default, skip_serializing_if = "is_false")]
    pub review_required: bool,
    /// The version id (hex) the plane last served — compared to the local applied version to flag
    /// `behind`. Empty for a withdrawn skill (nothing is served).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub served_version: String,
    /// Whether upstream WITHDREW the skill at the last reconcile (archived, or its last delivering
    /// channel dropped it) — the offline source of the `removed-upstream` detach cause.
    #[serde(default, skip_serializing_if = "is_false")]
    pub withdrawn: bool,
    /// The channels that deliver this skill (the `via` attribution) — the offline source for
    /// `list --channel <name>`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub via_channels: Vec<String>,
    /// Delivered through a MANIFEST channel row, NOT the workspace's own feed. These rows give
    /// the offline surfaces (`remove`'s set-split arm, provenance reads) the channel-member fact;
    /// they are EXCLUDED from everything feed-shaped — the offline feed materialization, the
    /// feed-drop cleaner, the delivered count.
    #[serde(default, skip_serializing_if = "is_false")]
    pub via_manifest: bool,
    /// The display name of the direct assignment's creator, when someone else aimed the bundle
    /// at this person — offline attribution for `status` ("assigned by <name>").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_by: Option<String>,
    /// `true` when the caller's own pick (a self-assignment) delivers it ("picked by you").
    #[serde(default, skip_serializing_if = "is_false")]
    pub picked: bool,
    /// The catalog's bundle kind, cached when it is not the default (`Some("mcp")` for a
    /// config-placed MCP-server bundle) — what lets `list`/`status` answer the kind (and the
    /// offline reconcile route the bundle) without a network call. Absent ⇒ `"skill"`. Additive.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// For an `mcp` bundle: the per-agent config states the last converge recorded (slug + state
    /// + note + placed file) — the offline source for `list <name>`'s per-agent detail. Additive.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub harness_states: Vec<topos_types::results::McpAgentState>,
}

/// serde skip helper — a `false` bool is the common case and stays out of the on-disk bytes.
fn is_false(b: &bool) -> bool {
    !*b
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

/// Record each workspace's LAST exchange fault against its EXISTING entry, leaving every other
/// field alone (the successful path replaces an entry wholesale, so a landed exchange clears the
/// fault by construction). Existing entries ONLY: a workspace that never delivered here has nothing
/// to be stale from and nothing to fault — the same philosophy [`is_stale`] keeps — so no entry is
/// born and, when none matched, no write happens at all.
pub(crate) fn record_faults(
    fs: &dyn FsOps,
    layout: &Layout,
    faults: &[(String, ExchangeFault)],
) -> Result<(), ClientError> {
    if faults.is_empty() {
        return Ok(());
    }
    let mut status = read(fs, layout)?;
    let mut touched = false;
    for (ws, fault) in faults {
        if let Some(entry) = status.workspaces.get_mut(ws) {
            entry.last_exchange_fault = Some(*fault);
            touched = true;
        }
    }
    if !touched {
        return Ok(());
    }
    status.schema_version = PERSISTED_SCHEMA_VERSION;
    fs.create_dir_all(&layout.state_dir())?;
    doc::write_doc(fs, &layout.sync_status_path(), &status)
}

/// Merge ONE delivered-skill row into a workspace's cache entry (creating the entry when absent,
/// preserving everything else) — the landed-publish seed: the publisher's own machine knows the
/// workspace now governs the bundle without waiting for the next sweep.
pub(crate) fn merge_delivered(
    fs: &dyn FsOps,
    layout: &Layout,
    workspace_id: &str,
    host: &str,
    workspace_name: &str,
    skill_id: &str,
    row: DeliveredSkill,
) -> Result<(), ClientError> {
    let mut status = read(fs, layout)?;
    status.schema_version = PERSISTED_SCHEMA_VERSION;
    let entry = status
        .workspaces
        .entry(workspace_id.to_owned())
        .or_default();
    if entry.host.is_none() {
        entry.host = Some(host.to_owned());
    }
    if entry.workspace_name.is_none() {
        entry.workspace_name = Some(workspace_name.to_owned());
    }
    // A standing row keeps its richer facts (the sweep is the authority); only absent rows seed.
    entry.delivered.entry(skill_id.to_owned()).or_insert(row);
    fs.create_dir_all(&layout.state_dir())?;
    doc::write_doc(fs, &layout.sync_status_path(), &status)
}

/// Advance ONE delivered row's SERVED version — the remote-tracking half of a pointer move this
/// device just made (a landed publish; a landed revert). `list`/`status` derive `behind` OFFLINE by
/// comparing the served version cached here against the locally applied one, so a move whose
/// receipt just printed has to land here too, or the very next row contradicts it until the
/// following sweep. It is git's own rule: a landed push updates the remote-tracking ref.
///
/// EXISTING rows ONLY — the rule [`record_faults`] keeps: a bundle the sweep never delivered here
/// has no cached version to correct, and an absent row already reads as nothing-served rather than
/// `behind`, so nothing is born and, when no row matched, no write happens at all. A WITHDRAWN row
/// keeps its empty version: a write of ours does not prove the workspace serves the bundle to this
/// caller again — only a delivery says that.
pub(crate) fn record_served(
    fs: &dyn FsOps,
    layout: &Layout,
    workspace_id: &str,
    skill_id: &str,
    version_id: &str,
) -> Result<(), ClientError> {
    let mut status = read(fs, layout)?;
    let Some(row) = status
        .workspaces
        .get_mut(workspace_id)
        .and_then(|w| w.delivered.get_mut(skill_id))
        .filter(|row| !row.withdrawn && row.served_version != version_id)
    else {
        return Ok(());
    };
    row.served_version = version_id.to_owned();
    status.schema_version = PERSISTED_SCHEMA_VERSION;
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
                    ..WorkspaceSync::default()
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
                    ..WorkspaceSync::default()
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
    fn a_fault_lands_on_an_existing_entry_and_a_delivery_clears_it() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("fault"));
        record(
            &fs,
            &layout,
            &[(
                "w_a".into(),
                WorkspaceSync {
                    last_delivery_at: Some(1_000),
                    staleness_window_ms: 10,
                    ..WorkspaceSync::default()
                },
            )],
        )
        .unwrap();
        // The fault lands beside the freshness facts — nothing else in the entry moves.
        record_faults(&fs, &layout, &[("w_a".into(), ExchangeFault::Malformed)]).unwrap();
        let entry = read(&fs, &layout).unwrap().workspaces["w_a"].clone();
        assert_eq!(entry.last_exchange_fault, Some(ExchangeFault::Malformed));
        assert_eq!(entry.last_delivery_at, Some(1_000));
        assert_eq!(entry.staleness_window_ms, 10);

        // The next successful delivery replaces the entry wholesale — the fault goes with it.
        record(
            &fs,
            &layout,
            &[(
                "w_a".into(),
                WorkspaceSync {
                    last_delivery_at: Some(2_000),
                    ..WorkspaceSync::default()
                },
            )],
        )
        .unwrap();
        assert_eq!(
            read(&fs, &layout).unwrap().workspaces["w_a"].last_exchange_fault,
            None
        );
    }

    #[test]
    fn a_never_delivered_workspace_gains_no_entry_from_a_fault() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("faultnew"));
        // No entry to modify: nothing is born, and the document is never even written (a first-run
        // failure must not manufacture a freshness row there is nothing to be fresh about).
        record_faults(
            &fs,
            &layout,
            &[("w_ghost".into(), ExchangeFault::Unreachable)],
        )
        .unwrap();
        assert!(!layout.sync_status_path().exists());
        assert!(read(&fs, &layout).unwrap().workspaces.is_empty());
        // An empty update writes nothing either.
        record_faults(&fs, &layout, &[]).unwrap();
        assert!(!layout.sync_status_path().exists());
    }

    #[test]
    fn a_landed_move_advances_the_served_version_of_an_existing_row_only() {
        let fs = RealFs;
        let layout = Layout::new(&scratch("served"));
        let row = |v: &str, withdrawn: bool| DeliveredSkill {
            name: "deploy".to_owned(),
            served_version: v.to_owned(),
            withdrawn,
            via_channels: vec!["everyone".to_owned()],
            ..DeliveredSkill::default()
        };
        record(
            &fs,
            &layout,
            &[(
                "w_a".into(),
                WorkspaceSync {
                    last_delivery_at: Some(1_000),
                    delivered: [
                        ("s_deploy".to_owned(), row(&"1".repeat(64), false)),
                        ("s_gone".to_owned(), row("", true)),
                    ]
                    .into_iter()
                    .collect(),
                    ..WorkspaceSync::default()
                },
            )],
        )
        .unwrap();

        // The move lands on the row it names; every other fact in the entry stays the sweep's.
        record_served(&fs, &layout, "w_a", "s_deploy", &"2".repeat(64)).unwrap();
        let entry = read(&fs, &layout).unwrap().workspaces["w_a"].clone();
        assert_eq!(entry.delivered["s_deploy"].served_version, "2".repeat(64));
        assert_eq!(entry.delivered["s_deploy"].via_channels, vec!["everyone"]);
        assert_eq!(entry.last_delivery_at, Some(1_000));

        // A WITHDRAWN row keeps its empty version — only a delivery may say it is served again.
        record_served(&fs, &layout, "w_a", "s_gone", &"3".repeat(64)).unwrap();
        assert!(
            read(&fs, &layout).unwrap().workspaces["w_a"].delivered["s_gone"]
                .served_version
                .is_empty()
        );

        // An unknown workspace or skill is not born here (the [`record_faults`] rule): a bundle the
        // sweep never delivered has no cached version to correct.
        record_served(&fs, &layout, "w_ghost", "s_deploy", &"4".repeat(64)).unwrap();
        record_served(&fs, &layout, "w_a", "s_ghost", &"4".repeat(64)).unwrap();
        let after = read(&fs, &layout).unwrap();
        assert!(!after.workspaces.contains_key("w_ghost"));
        assert_eq!(after.workspaces["w_a"].delivered.len(), 2);
    }

    #[test]
    fn staleness_needs_a_record_a_window_and_an_expired_clock() {
        let fresh = WorkspaceSync {
            last_delivery_at: Some(1_000),
            last_report_at: None,
            staleness_window_ms: 10_000,
            ..WorkspaceSync::default()
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
