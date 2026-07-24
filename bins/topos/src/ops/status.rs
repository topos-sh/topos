//! `status` — the one orientation read AND the trust rail: the sessions, the resolved table for
//! "an agent here" (this folder's manifest chain + the person's profile layers from the offline
//! delivery cache + the personal manifest), and the binary version — computed ENTIRELY from local
//! state (no network, no writes). Per bundle: the version, ONE source (which manifest line — or
//! profile — asked), and an honest state (applied-as-of-the-last-sync / local edits / behind /
//! excluded / not available); recorded excludes render as their own rows. The per-agent trigger
//! rows ride the same payload but are probed at the composition root (`ops::probe_detected` — the
//! one layer holding the real config port + `$HOME`), mirroring how the arming sweep's receipts
//! land.
//!
//! The bare `topos` invocation renders this same snapshot on a TTY, so a human's first keystroke
//! answers "what is this, and where am I" without dialing anything.

use std::path::Path;

use topos_types::persisted::{Lock, SyncState};
use topos_types::results::{StatusData, StatusItem, StatusItemState, StatusSession};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::file::MANIFEST_FILE;
use crate::manifest::refs::ParsedRef;
use crate::manifest::resolve::{Layer, LayerSource, ResolvedScope, resolve_layers};
use crate::manifest::walk;
use crate::sync_status::{DeliveredSkill, SyncStatus, WorkspaceSync};
use crate::{doc, placement, sessions, sync_status};

/// The all-zero sentinel a first-receive baseline carries — a delivered skill whose sync doc
/// still holds it has never been applied here (the next `update` applies it).
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Assemble the offline status snapshot. `triggers` stays empty here — the composition root fills
/// it from the read-only probe (`ops::probe_detected`), the same layering the arming receipts use.
///
/// # Errors
/// An io/doc failure reading the local session/manifest documents (the probes this op refuses to
/// run cannot fail it; a missing doc is a plain "not connected", never an error).
pub(crate) fn status_snapshot(ctx: &Ctx<'_>) -> Result<StatusData, ClientError> {
    let all_sessions = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let signed_in = all_sessions.live().count() > 0;
    let cache = sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();

    // The PROFILE-delivered set (the offline cache; manifest-channel rows are not the profile's).
    // A delivery whose sidecar sync doc still holds the all-zero baseline has never been applied
    // here. Any unreadable doc makes the count honestly absent, never a partial number.
    let mut profile_skills = 0u64;
    let mut awaiting = Some(0u64);
    for entry in cache.workspaces.values() {
        for (skill_id, d) in &entry.delivered {
            if d.withdrawn || d.via_manifest {
                continue;
            }
            profile_skills += 1;
            let Ok(sid) = crate::id::SkillId::parse(skill_id) else {
                awaiting = None;
                continue;
            };
            // Never applied = the sidecar still holds the all-zero baseline, OR nothing local
            // exists at all (delivered per the cache; the next `update` lands it).
            match doc::read_doc::<SyncState>(ctx.fs, &ctx.layout.published(&sid).sync) {
                Ok(Some(sync)) if sync.base_commit == ZERO_HEX => {
                    awaiting = awaiting.map(|n| n + 1);
                }
                Ok(None) => awaiting = awaiting.map(|n| n + 1),
                Ok(Some(_)) => {}
                Err(_) => awaiting = None,
            }
        }
    }

    // This installation's SESSIONS (one per logged-into workspace).
    let session_rows: Vec<StatusSession> = all_sessions
        .sessions
        .iter()
        .map(|s| StatusSession {
            workspace_id: s.workspace_id.clone(),
            name: s.workspace_name.clone(),
            display_name: s.display_name.clone(),
            host: s.host.clone(),
            session_status: (s.status != sessions::SESSION_ACTIVE).then(|| s.status.clone()),
        })
        .collect();

    Ok(StatusData {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        server: all_sessions.sessions.first().map(|s| s.base_url.clone()),
        signed_in,
        profile_skills,
        awaiting_first_sync: awaiting,
        triggers: Vec::new(),
        items: trust_rail(ctx, &session_rows, &cache)?,
        sessions: session_rows,
    })
}

/// The TRUST-RAIL table for the current directory, from LOCAL knowledge only (the project
/// manifest chain + the PROFILE layers materialized from the offline delivery cache + the
/// personal manifest + the stored sessions — never a network read): per resolved line, the
/// winning reference, ONE source label, the scope, and an honest state; excludes as their own
/// rows; a manifest channel's cached members itemized under it.
fn trust_rail(
    ctx: &Ctx<'_>,
    session_rows: &[StatusSession],
    cache: &SyncStatus,
) -> Result<Vec<StatusItem>, ClientError> {
    // The layers, nearest-first: this folder's chain → each session's profile (from the offline
    // cache — the same stand-in the offline reconcile resolves with) → the personal manifest.
    let mut layers: Vec<Layer> = Vec::new();
    if let Some(roots) = &ctx.roots
        && let Some(cwd) = roots.cwd.as_deref()
    {
        for l in walk::project_layers(ctx.fs, cwd, Some(&roots.home))? {
            layers.push(Layer::project(l.dir, l.manifest));
        }
    }
    for sess in session_rows {
        let Some(entry) = cache.workspaces.get(&sess.workspace_id) else {
            continue;
        };
        let delivered: Vec<(String, String, Option<String>)> = entry
            .delivered
            .values()
            .filter(|d| !d.withdrawn && !d.via_manifest && !d.name.is_empty())
            .map(|d| {
                (
                    d.name.clone(),
                    format!("{}/{}/{}", sess.host, sess.name, d.name),
                    None,
                )
            })
            .collect();
        if !delivered.is_empty() {
            layers.push(Layer::profile(
                sess.host.clone(),
                sess.name.clone(),
                delivered,
            ));
        }
    }
    if let Some(personal) =
        crate::manifest::file::read_manifest(ctx.fs, &ctx.layout.home().join(MANIFEST_FILE))?
    {
        layers.push(Layer::personal(personal));
    }
    if layers.is_empty() {
        return Ok(Vec::new());
    }
    let resolution = resolve_layers(&layers);
    let mut items = Vec::with_capacity(resolution.items.len() + resolution.excluded.len());
    // Direct items claim their names; EXCLUDES are checked per channel member with the
    // layer-ordered rule (`exclude_wins`) — a broader exclude never hides a nearer channel's
    // member from the rail.
    let mut claimed: std::collections::HashSet<String> =
        resolution.items.iter().map(|i| i.name.clone()).collect();
    for item in resolution.items {
        let mut via = None;
        let (state, version, applied_as_of) = match &item.parsed {
            // A local folder: its presence IS the delivery (adopted in place; the folder itself
            // is the source of truth, so there is no upstream to be behind or ahead of).
            ParsedRef::LocalPath { raw } => {
                let base = match &item.source {
                    LayerSource::Project { dir } => dir.clone(),
                    _ => ctx.layout.home().to_path_buf(),
                };
                let dir = if Path::new(raw).is_absolute() {
                    std::path::PathBuf::from(raw)
                } else {
                    base.join(raw.trim_start_matches("./"))
                };
                if ctx.fs.exists(&dir) {
                    (StatusItemState::Applied, None, None)
                } else {
                    (StatusItemState::Unknown, None, None)
                }
            }
            // A workspace skill reference (or a profile item): the offline delivery cache + the
            // sidecar answer applied/edits/behind honestly ("applied as of the last sync");
            // without a cache row, the LOCAL session file phrases the line.
            ParsedRef::Skill {
                host,
                workspace,
                name,
                ..
            } => match cache_lookup(cache, host.as_deref(), Some(workspace), name) {
                Some(hit) => {
                    via = hit.ds.via_channels.first().cloned();
                    applied_state(ctx, &hit)
                }
                None => (
                    session_fallback(session_rows, host.as_deref(), Some(workspace)),
                    None,
                    None,
                ),
            },
            // A bare name: any workspace's cached delivery answers it.
            ParsedRef::Bare { name, .. } => match cache_lookup(cache, None, None, name) {
                Some(hit) => {
                    via = hit.ds.via_channels.first().cloned();
                    applied_state(ctx, &hit)
                }
                None => (session_fallback(session_rows, None, None), None, None),
            },
            // A channel reference: the row itself reads from the session; its cached MEMBERS
            // (the reconcile records them `via_manifest`) itemize right below.
            ParsedRef::Channel {
                host,
                workspace,
                name,
            } => {
                for (sess, entry) in
                    cache_workspaces(cache, session_rows, host.as_deref(), workspace)
                {
                    for (skill_id, ds) in &entry.delivered {
                        if ds.withdrawn
                            || !ds.via_manifest
                            || !ds.via_channels.iter().any(|c| c == name)
                            || resolution.excluded.iter().any(|ex| {
                                ex.name == ds.name
                                    && crate::manifest::resolve::exclude_wins(
                                        ex.layer_index,
                                        item.layer_index,
                                    )
                            })
                            || !claimed.insert(ds.name.clone())
                        {
                            continue;
                        }
                        let hit = CacheHit {
                            ws_entry: entry,
                            skill_id,
                            ds,
                        };
                        let (state, version, applied_as_of) = applied_state(ctx, &hit);
                        items.push(StatusItem {
                            name: ds.name.clone(),
                            reference: format!("{}/{}/{}", sess.host, sess.name, ds.name),
                            source: item.source.label(),
                            scope: scope_label(&item.scope),
                            via: Some(name.clone()),
                            version,
                            applied_as_of,
                            state,
                            shadows: Vec::new(),
                        });
                    }
                }
                (
                    session_fallback(session_rows, host.as_deref(), Some(workspace)),
                    None,
                    None,
                )
            }
            // An external GitHub origin: the sidecar's origin record answers offline.
            ParsedRef::GitHub {
                owner,
                repo,
                subdir,
                ..
            } => {
                let origin_source = format!("github.com/{owner}/{repo}");
                match super::reconcile::find_tracked_github(ctx, &origin_source, subdir) {
                    Some((_, lock, origin)) => {
                        let recorded = origin.commit.as_deref().unwrap_or_default();
                        let pin_ok = item
                            .pin
                            .as_deref()
                            .is_none_or(|p| super::reconcile::commit_matches(recorded, p));
                        if pin_ok {
                            (StatusItemState::Applied, Some(lock.base_commit), None)
                        } else {
                            // The pin moved past the installed commit — the next update lands it.
                            (StatusItemState::Behind, Some(lock.base_commit), None)
                        }
                    }
                    None => (StatusItemState::Unknown, None, None),
                }
            }
        };
        items.push(StatusItem {
            name: item.name,
            reference: item.reference,
            source: item.source.label(),
            scope: scope_label(&item.scope),
            via,
            version,
            applied_as_of,
            state,
            shadows: item.shadowed_from.iter().map(LayerSource::label).collect(),
        });
    }
    // Recorded EXCLUDES render as their own rows — the "why not?" half of the rail. An exclude
    // a NEARER channel member shadowed (its name claimed above) does not render: the member's
    // own row already tells the delivered story, and one name must never read as both applied
    // and excluded.
    for ex in resolution.excluded {
        if claimed.contains(&ex.name) {
            continue;
        }
        let scope = match &ex.by {
            LayerSource::Project { .. } => "project",
            LayerSource::Profile { .. } | LayerSource::Personal => "person",
        };
        items.push(StatusItem {
            name: ex.name.clone(),
            reference: ex.name,
            source: ex.by.label(),
            scope: scope.to_owned(),
            via: None,
            version: None,
            applied_as_of: None,
            state: StatusItemState::Excluded,
            shadows: ex.shadowed_from.iter().map(LayerSource::label).collect(),
        });
    }
    let _ = claimed;
    Ok(items)
}

fn scope_label(scope: &ResolvedScope) -> String {
    match scope {
        ResolvedScope::Project { .. } => "project".to_owned(),
        ResolvedScope::Person => "person".to_owned(),
    }
}

/// One cached delivery a trust-rail line resolves against.
struct CacheHit<'a> {
    ws_entry: &'a WorkspaceSync,
    skill_id: &'a str,
    ds: &'a DeliveredSkill,
}

/// Find the cached delivery for `name`, narrowed by the spelled host/workspace when present.
fn cache_lookup<'a>(
    cache: &'a SyncStatus,
    host: Option<&str>,
    ws_name: Option<&str>,
    name: &str,
) -> Option<CacheHit<'a>> {
    for entry in cache.workspaces.values() {
        let host_ok = host.is_none_or(|h| entry.host.as_deref() == Some(h));
        let ws_ok = ws_name.is_none_or(|w| entry.workspace_name.as_deref() == Some(w));
        if !host_ok || !ws_ok {
            continue;
        }
        if let Some((skill_id, ds)) = entry
            .delivered
            .iter()
            .find(|(_, d)| !d.withdrawn && d.name == name)
        {
            return Some(CacheHit {
                ws_entry: entry,
                skill_id,
                ds,
            });
        }
    }
    None
}

/// The `(session, cache entry)` pairs a channel reference's workspace resolves to.
fn cache_workspaces<'a>(
    cache: &'a SyncStatus,
    session_rows: &'a [StatusSession],
    host: Option<&str>,
    ws_name: &str,
) -> Vec<(&'a StatusSession, &'a WorkspaceSync)> {
    session_rows
        .iter()
        .filter(|s| host.is_none_or(|h| s.host == h) && s.name == ws_name)
        .filter_map(|s| cache.workspaces.get(&s.workspace_id).map(|e| (s, e)))
        .collect()
}

/// The honest APPLIED state for one cached delivery, from the sidecar alone: never applied →
/// `Unknown` ("not applied here yet"); a scanned draft → `LocalEdits`; the cached served version
/// past the applied one → `Behind`; else `Applied` stamped "as of" the cache's last delivery
/// time — an offline fact, never a live claim. Any unreadable doc degrades to `Unknown`.
fn applied_state(
    ctx: &Ctx<'_>,
    hit: &CacheHit<'_>,
) -> (StatusItemState, Option<String>, Option<String>) {
    let unknown = (StatusItemState::Unknown, None, None);
    let Ok(sid) = crate::id::SkillId::parse(hit.skill_id) else {
        return unknown;
    };
    if !ctx.fs.exists(&ctx.layout.skill_dir(&sid)) {
        return unknown;
    }
    let sp = ctx.layout.published(&sid);
    let Ok(Some(sync)) = doc::read_doc::<SyncState>(ctx.fs, &sp.sync) else {
        return unknown;
    };
    if sync.base_commit == ZERO_HEX {
        return unknown;
    }
    let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &sp.lock) else {
        return unknown;
    };
    let version = Some(lock.base_commit.clone());
    if let Ok(Some(map)) = doc::read_map(ctx.fs, &sp.map)
        && let Ok(scans) = placement::scan_placements(ctx, &map)
        && scans
            .iter()
            .any(|s| matches!(s.status, placement::ScanStatus::Modified { .. }))
    {
        return (StatusItemState::LocalEdits, version, None);
    }
    if !hit.ds.served_version.is_empty() && hit.ds.served_version != lock.base_commit {
        return (StatusItemState::Behind, version, None);
    }
    let as_of = hit
        .ws_entry
        .last_delivery_at
        .map(super::connect::fmt_rfc3339_millis);
    (StatusItemState::Applied, version, as_of)
}

/// The honest line for a workspace ref with NO cached delivery — phrased from the LOCAL session
/// file, never a server answer.
fn session_fallback(
    session_rows: &[StatusSession],
    host: Option<&str>,
    ws_name: Option<&str>,
) -> StatusItemState {
    let Some(ws) = ws_name else {
        // A bare name with no cache row: whether a live session could deliver it is the
        // reconcile's answer.
        return StatusItemState::Unknown;
    };
    match session_rows
        .iter()
        .find(|s| host.is_none_or(|h| s.host == h) && s.name == ws)
    {
        None => StatusItemState::NotAvailable,
        Some(s) if s.session_status.as_deref() == Some("pending") => {
            StatusItemState::PendingSession
        }
        Some(s) if s.session_status.as_deref() == Some("ended") => StatusItemState::NotAvailable,
        // Connected but never delivered here — the next `update` answers.
        Some(_) => StatusItemState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::ctx::Ctx;
    use crate::enroll;
    use crate::fs_seam::RealFs;
    use crate::ids::{RealClock, RealIds};
    use crate::plane::{InertFollow, InertPlane};
    use crate::sidecar::Layout;
    use topos_types::PERSISTED_SCHEMA_VERSION;

    /// A self-cleaning temp `~/.topos` home (RAII).
    struct TempHome(PathBuf);
    impl TempHome {
        fn new() -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!("topos-status-{}-{n}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }
    }
    impl Drop for TempHome {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn snapshot(home: &TempHome) -> StatusData {
        let fs = RealFs;
        let harness = topos_harness::ClaudeCode::new(home.0.join(".claude"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &RealIds,
            clock: &RealClock,
            device_id: String::new(),
            layout: Layout::new(&home.0),
            harness: &harness,
            plane: &InertPlane,
            follow: &InertFollow,
            roots: None,
        };
        status_snapshot(&ctx).expect("status snapshot")
    }

    /// A snapshot with machine roots (the trust-rail walk needs a cwd) — `home.0` doubles as the
    /// user home; the sidecar sits beside it.
    fn snapshot_at(home: &TempHome, cwd: &std::path::Path) -> StatusData {
        let fs = RealFs;
        let harness = topos_harness::ClaudeCode::new(home.0.join(".claude"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &RealIds,
            clock: &RealClock,
            device_id: String::new(),
            layout: Layout::new(&home.0.join(".topos")),
            harness: &harness,
            plane: &InertPlane,
            follow: &InertFollow,
            roots: Some(crate::ctx::AgentRoots {
                home: home.0.clone(),
                cwd: Some(cwd.to_path_buf()),
            }),
        };
        status_snapshot(&ctx).expect("status snapshot")
    }

    #[test]
    fn the_trust_rail_resolves_local_manifests_and_sessions_offline() {
        let home = TempHome::new();
        let repo = home.0.join("repo");
        std::fs::create_dir_all(repo.join("tools/my-skill")).unwrap();
        std::fs::write(
            repo.join(MANIFEST_FILE),
            "exclude = [\"noisy\"]\n[skills]\n\"./tools/my-skill\" = \"*\"\n\"topos.sh/acme/deploy\" = \"*\"\n\"topos.example.com/eng/api\" = \"*\"\n",
        )
        .unwrap();
        // One session: acme on topos.sh, PENDING. eng@topos.example.com has none.
        let fs = RealFs;
        let layout = Layout::new(&home.0.join(".topos"));
        crate::sessions::upsert_session(
            &fs,
            &layout,
            crate::sessions::Session {
                host: "topos.sh".to_owned(),
                base_url: "https://topos.sh/api".to_owned(),
                workspace_id: "w_acme".to_owned(),
                workspace_name: "acme".to_owned(),
                display_name: "Acme".to_owned(),
                session_id: "sn_1".to_owned(),
                credential: "c".to_owned(),
                status: crate::sessions::SESSION_PENDING.to_owned(),
                logged_in_at: 1,
            },
        )
        .unwrap();

        let d = snapshot_at(&home, &repo);
        // Sessions render with their non-active status.
        assert_eq!(d.sessions.len(), 1);
        assert_eq!(d.sessions[0].host, "topos.sh");
        assert_eq!(d.sessions[0].session_status.as_deref(), Some("pending"));
        // The table: three resolved lines + the EXCLUDE row, each with its one source + an
        // honest state.
        let by_name = |n: &str| d.items.iter().find(|i| i.name == n).unwrap();
        let local = by_name("my-skill");
        assert!(matches!(local.state, StatusItemState::Applied));
        assert_eq!(local.scope, "project");
        assert!(local.source.ends_with("topos.toml"), "{}", local.source);
        // A workspace ref whose session is PENDING says so; one with NO session is the honest
        // not-available line — phrased from local knowledge, nothing dialed.
        assert!(matches!(
            by_name("deploy").state,
            StatusItemState::PendingSession
        ));
        assert!(matches!(
            by_name("api").state,
            StatusItemState::NotAvailable
        ));
        // The recorded exclude appears as its own row, attributed to the layer that wrote it.
        let excluded = by_name("noisy");
        assert!(matches!(excluded.state, StatusItemState::Excluded));
        assert!(
            excluded.source.ends_with("topos.toml"),
            "{}",
            excluded.source
        );
    }

    #[test]
    fn a_cached_delivery_reads_applied_as_of_the_last_sync_and_the_profile_is_itemized() {
        let home = TempHome::new();
        let cwd = home.0.join("plain");
        std::fs::create_dir_all(&cwd).unwrap();
        let fs = RealFs;
        let layout = Layout::new(&home.0.join(".topos"));
        crate::sessions::upsert_session(
            &fs,
            &layout,
            crate::sessions::Session {
                host: "topos.sh".to_owned(),
                base_url: "https://topos.sh/api".to_owned(),
                workspace_id: "w_acme".to_owned(),
                workspace_name: "acme".to_owned(),
                display_name: "Acme".to_owned(),
                session_id: "sn_1".to_owned(),
                credential: "c".to_owned(),
                status: crate::sessions::SESSION_ACTIVE.to_owned(),
                logged_in_at: 1,
            },
        )
        .unwrap();
        // The offline cache records one profile-delivered skill (never applied on this machine:
        // no sidecar dir) and one manifest-channel row (excluded from profile surfaces).
        let mut delivered = std::collections::BTreeMap::new();
        delivered.insert(
            "topos_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            crate::sync_status::DeliveredSkill {
                name: "deploy".to_owned(),
                review_required: false,
                served_version: "d".repeat(64),
                withdrawn: false,
                via_channels: vec!["everyone".to_owned()],
                via_manifest: false,
            },
        );
        delivered.insert(
            "topos_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
            crate::sync_status::DeliveredSkill {
                name: "chan-member".to_owned(),
                review_required: false,
                served_version: "e".repeat(64),
                withdrawn: false,
                via_channels: vec!["backend".to_owned()],
                via_manifest: true,
            },
        );
        crate::sync_status::record(
            &fs,
            &layout,
            &[(
                "w_acme".to_owned(),
                crate::sync_status::WorkspaceSync {
                    host: Some("topos.sh".to_owned()),
                    workspace_name: Some("acme".to_owned()),
                    last_delivery_at: Some(1_700_000_000_000),
                    last_report_at: None,
                    staleness_window_ms: 0,
                    delivered,
                },
            )],
        )
        .unwrap();

        let d = snapshot_at(&home, &cwd);
        // The person layer is ITEMIZED: the profile row appears with its source + via channel.
        let row = d
            .items
            .iter()
            .find(|i| i.name == "deploy")
            .expect("the profile layer itemizes");
        assert_eq!(row.source, "your profile @ topos.sh/acme");
        assert_eq!(row.via.as_deref(), Some("everyone"));
        assert_eq!(row.scope, "person");
        // Never applied here (no sidecar): the honest not-applied-yet state, no false claim.
        assert!(matches!(row.state, StatusItemState::Unknown));
        // The manifest-channel row is NOT a profile item (no manifest here asks for it).
        assert!(!d.items.iter().any(|i| i.name == "chan-member"));
        assert_eq!(d.profile_skills, 1);
        assert_eq!(d.awaiting_first_sync, Some(1));
    }

    /// Every file under `dir`, as `relative path → bytes` — the byte-identity oracle.
    fn tree_bytes(dir: &PathBuf) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        fn walk(
            root: &PathBuf,
            dir: &PathBuf,
            out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
        ) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(root, &path, out);
                } else {
                    let rel = path.strip_prefix(root).unwrap().to_path_buf();
                    out.insert(rel, std::fs::read(&path).unwrap_or_default());
                }
            }
        }
        let mut out = std::collections::BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    /// The read-only promise, proven byte-for-byte: a status run over a sidecar holding a
    /// PENDING-RECOVERY fixture (an expired enrollment WAL the ordinary start-of-command sweep
    /// would reap) leaves every byte in place — the snapshot AND the trigger probe (the exact
    /// pre-recovery pair the composition root's fast path runs) write nothing, and the same
    /// fixture is then shown POTENT: the recovery sweep the fast path skips does mutate it.
    #[test]
    fn status_leaves_a_pending_recovery_sidecar_byte_identical() {
        let home = TempHome::new();
        let fs = RealFs;
        let layout = Layout::new(&home.0);
        enroll::write_wal(
            &fs,
            &layout,
            &enroll::PendingEnrollment {
                schema_version: PERSISTED_SCHEMA_VERSION,
                host: String::new(),
                base_url: "https://topos.sh/api".to_owned(),
                workspace_name: "acme".to_owned(),
                intent: enroll::EnrollIntentDoc::Session,
                device_code: "dc_expired".to_owned(),
                user_code: "XXXX-YYYY".to_owned(),
                verification_uri: "https://topos.sh/verify".to_owned(),
                interval_secs: 5,
                // Long expired — recovery would reap this WAL on any ordinary command.
                expires_at_millis: 1_000,
            },
        )
        .unwrap();

        let before = tree_bytes(&home.0);
        assert!(!before.is_empty(), "the fixture is on disk");

        // The exact pair the composition root's pre-recovery fast path runs.
        let harness = topos_harness::ClaudeCode::new(home.0.join(".claude"), &fs);
        let ctx = Ctx {
            fs: &fs,
            ids: &RealIds,
            clock: &RealClock,
            device_id: String::new(),
            layout: layout.clone(),
            harness: &harness,
            plane: &InertPlane,
            follow: &InertFollow,
            roots: None,
        };
        let data = status_snapshot(&ctx).expect("status snapshot");
        assert!(data.sessions.is_empty());
        let _ = crate::ops::probe_detected(&home.0, None, &harness, &fs);
        assert_eq!(
            before,
            tree_bytes(&home.0),
            "a status run must leave the sidecar byte-identical"
        );

        // The fixture is potent: the sweep the fast path skips DOES mutate it (the WAL is reaped).
        crate::sidecar::recover(&fs, &layout, i64::MAX).unwrap();
        assert_ne!(
            before,
            tree_bytes(&home.0),
            "the recovery sweep reaps the expired WAL — proving status really skipped it"
        );
    }

    #[test]
    fn a_fresh_install_reads_not_connected_with_nothing_delivered() {
        let home = TempHome::new();
        let data = snapshot(&home);
        assert!(data.sessions.is_empty() && !data.signed_in);
        assert_eq!(data.server, None);
        assert_eq!(data.profile_skills, 0);
        assert_eq!(data.awaiting_first_sync, Some(0));
        assert_eq!(data.version, env!("CARGO_PKG_VERSION"));
        assert!(data.triggers.is_empty());
    }
}
