//! `status` — the one orientation read: enrollment, sign-in, follow counts, and the binary
//! version, computed ENTIRELY from local state (no network, no writes). The per-agent trigger rows
//! ride the same payload but are probed at the composition root (`ops::probe_detected` — the one
//! layer holding the real config port + `$HOME`), mirroring how the arming sweep's receipts land.
//!
//! The bare `topos` invocation renders this same snapshot on a TTY, so a human's first keystroke
//! answers "what is this, and where am I" without dialing anything.

use topos_types::persisted::SyncState;
use topos_types::results::{
    StatusData, StatusItem, StatusItemState, StatusSession, StatusWorkspace,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::manifest::file::MANIFEST_FILE;
use crate::manifest::refs::ParsedRef;
use crate::manifest::resolve::{Layer, LayerSource, ResolvedScope, resolve_layers};
use crate::manifest::walk;
use crate::{doc, sessions};

/// The all-zero sentinel a first-receive baseline carries (`follow` lays it; accepting the offer
/// replaces it) — a followed skill whose sync doc still holds it has a PENDING first-receive offer.
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Assemble the offline status snapshot. `triggers` stays empty here — the composition root fills
/// it from the read-only probe (`ops::probe_detected`), the same layering the arming receipts use.
///
/// # Errors
/// An io/doc failure reading the local enrollment/follow documents (the probes this op refuses to
/// run cannot fail it; a missing doc is a plain "not enrolled", never an error).
pub(crate) fn status_snapshot(ctx: &Ctx<'_>) -> Result<StatusData, ClientError> {
    let all_sessions = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let signed_in = all_sessions.live().count() > 0;
    let cache = crate::sync_status::read(ctx.fs, &ctx.layout).unwrap_or_default();

    // The connected workspaces ARE the sessions (one row each; a non-active status is the fact).
    let workspaces: Vec<StatusWorkspace> = all_sessions
        .sessions
        .iter()
        .map(|s| StatusWorkspace {
            workspace_id: s.workspace_id.clone(),
            name: s.workspace_name.clone(),
            display_name: s.display_name.clone(),
            link_status: (s.status != sessions::SESSION_ACTIVE).then(|| s.status.clone()),
        })
        .collect();

    // The delivered set (the offline cache) — `followed_skills` counts what the profiles deliver;
    // a NOT-YET-RECONCILED delivery (its sidecar sync doc still at the all-zero baseline) counts
    // as pending. Any unreadable doc makes the count honestly absent, never a partial number.
    let mut followed = 0u64;
    let mut pending = Some(0u64);
    for entry in cache.workspaces.values() {
        for (skill_id, d) in &entry.delivered {
            if d.withdrawn {
                continue;
            }
            followed += 1;
            let Ok(sid) = crate::id::SkillId::parse(skill_id) else {
                pending = None;
                continue;
            };
            match doc::read_doc::<SyncState>(ctx.fs, &ctx.layout.published(&sid).sync) {
                Ok(Some(sync)) if sync.base_commit == ZERO_HEX => {
                    pending = pending.map(|n| n + 1);
                }
                Ok(Some(_)) | Ok(None) => {}
                Err(_) => pending = None,
            }
        }
    }

    // This installation's SESSIONS (the session model — one per logged-into workspace).
    let session_rows: Vec<StatusSession> = sessions::read_sessions(ctx.fs, &ctx.layout)?
        .sessions
        .into_iter()
        .map(|s| StatusSession {
            workspace_id: s.workspace_id,
            name: s.workspace_name,
            display_name: s.display_name,
            host: s.host,
            session_status: (s.status != sessions::SESSION_ACTIVE).then_some(s.status),
        })
        .collect();

    Ok(StatusData {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        enrolled: !all_sessions.sessions.is_empty(),
        server: all_sessions.sessions.first().map(|s| s.base_url.clone()),
        signed_in,
        workspaces,
        followed_skills: followed,
        pending_offers: pending,
        triggers: Vec::new(),
        items: trust_rail(ctx, &session_rows)?,
        sessions: session_rows,
    })
}

/// The TRUST-RAIL table for the current directory, from LOCAL knowledge only (the project
/// manifest chain + the personal manifest + the stored sessions — never a network read): per
/// resolved line, the winning reference, ONE source label, the scope, and an honest state.
fn trust_rail(
    ctx: &Ctx<'_>,
    session_rows: &[StatusSession],
) -> Result<Vec<StatusItem>, ClientError> {
    // The local layers: this folder's chain (nearest first), then the personal manifest.
    let mut layers: Vec<Layer> = Vec::new();
    if let Some(roots) = &ctx.roots
        && let Some(cwd) = roots.cwd.as_deref()
    {
        for l in walk::project_layers(ctx.fs, cwd, Some(&roots.home))? {
            layers.push(Layer::project(l.dir, l.manifest));
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
    let mut items = Vec::with_capacity(resolution.items.len());
    for item in resolution.items {
        let state = match &item.parsed {
            // A local folder: its presence IS the delivery (adopted in place).
            ParsedRef::LocalPath { raw } => {
                let base = match &item.source {
                    LayerSource::Project { dir } => dir.clone(),
                    _ => ctx.layout.home().to_path_buf(),
                };
                let dir = if std::path::Path::new(raw).is_absolute() {
                    std::path::PathBuf::from(raw)
                } else {
                    base.join(raw.trim_start_matches("./"))
                };
                if ctx.fs.exists(&dir) {
                    StatusItemState::Applied
                } else {
                    StatusItemState::Unknown
                }
            }
            // A workspace reference resolves through the session its HOST/WORKSPACE names — the
            // honest line comes from the LOCAL session file, never a server answer.
            ParsedRef::Skill {
                host, workspace, ..
            }
            | ParsedRef::Channel {
                host, workspace, ..
            } => match host {
                Some(h) => {
                    match session_rows
                        .iter()
                        .find(|s| &s.host == h && &s.name == workspace)
                    {
                        None => StatusItemState::NotAvailable,
                        Some(s) if s.session_status.as_deref() == Some("pending") => {
                            StatusItemState::PendingSession
                        }
                        Some(s) if s.session_status.as_deref() == Some("ended") => {
                            StatusItemState::NotAvailable
                        }
                        // Connected — whether the bytes are current is the reconcile's answer.
                        Some(_) => StatusItemState::Unknown,
                    }
                }
                // A host-less `@ws/…` spelling (hand-written): not resolvable offline.
                None => StatusItemState::Unknown,
            },
            // External + bare spellings: nothing local answers them yet.
            ParsedRef::GitHub { .. } | ParsedRef::Bare { .. } => StatusItemState::Unknown,
        };
        items.push(StatusItem {
            name: item.name,
            reference: item.reference,
            source: item.source.label(),
            scope: match item.scope {
                ResolvedScope::Project { .. } => "project".to_owned(),
                ResolvedScope::Person => "person".to_owned(),
            },
            version: None,
            state,
            shadows: item.shadowed_from.iter().map(LayerSource::label).collect(),
        });
    }
    Ok(items)
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
        // The table: three resolved lines, each with its one source + an honest state.
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
        assert!(!data.enrolled);
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
    fn a_fresh_install_reads_not_enrolled_with_nothing_followed() {
        let home = TempHome::new();
        let data = snapshot(&home);
        assert!(!data.enrolled && !data.signed_in);
        assert_eq!(data.server, None);
        assert!(data.workspaces.is_empty());
        assert_eq!(data.followed_skills, 0);
        assert_eq!(data.pending_offers, Some(0));
        assert_eq!(data.version, env!("CARGO_PKG_VERSION"));
        assert!(data.triggers.is_empty());
    }
}
