//! `status` — the one orientation read: enrollment, sign-in, follow counts, and the binary
//! version, computed ENTIRELY from local state (no network, no writes). The per-agent trigger rows
//! ride the same payload but are probed at the composition root (`ops::probe_detected` — the one
//! layer holding the real config port + `$HOME`), mirroring how the arming sweep's receipts land.
//!
//! The bare `topos` invocation renders this same snapshot on a TTY, so a human's first keystroke
//! answers "what is this, and where am I" without dialing anything.

use topos_types::persisted::SyncState;
use topos_types::results::{StatusData, StatusWorkspace};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::{doc, enroll};

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
    let instance = enroll::read_instance(ctx.fs, &ctx.layout)?;
    let user = enroll::read_user(ctx.fs, &ctx.layout)?;
    let signed_in = enroll::read_credentials(ctx.fs, &ctx.layout)?.is_some();
    let follows = enroll::read_follows(ctx.fs, &ctx.layout)?;

    let workspaces: Vec<StatusWorkspace> = user
        .map(|u| {
            u.workspaces
                .into_iter()
                .map(|m| StatusWorkspace {
                    workspace_id: m.workspace_id,
                    name: m.name,
                    display_name: m.display_name,
                })
                .collect()
        })
        .unwrap_or_default();

    // The active follows (the delivery entitlement this device acts on): following, not excluded
    // here. A pending FIRST-RECEIVE offer is one whose sync doc still holds the all-zero baseline
    // (nothing ever materialized). Cheap: one sidecar doc per followed skill; any unreadable doc
    // makes the count NOT cheaply knowable (absent), never a partial number presented as exact.
    let mut followed = 0u64;
    let mut pending = Some(0u64);
    for entry in follows.iter().flat_map(|f| f.follows.iter()) {
        if !entry.following || entry.excluded_here {
            continue;
        }
        followed += 1;
        let Ok(sid) = crate::id::SkillId::parse(&entry.skill_id) else {
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

    Ok(StatusData {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        enrolled: instance.is_some(),
        server: instance.map(|i| i.base_url),
        signed_in,
        workspaces,
        followed_skills: followed,
        pending_offers: pending,
        triggers: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::ctx::Ctx;
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
                base_url: "https://topos.sh/api".to_owned(),
                workspace_name: "acme".to_owned(),
                intent: enroll::EnrollIntentDoc::Follow {
                    target: None,
                    mode: enroll::FollowModeDoc::Auto,
                },
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

    #[test]
    fn an_enrolled_install_counts_follows_and_the_never_received_offer() {
        let home = TempHome::new();
        let fs = RealFs;
        let layout = Layout::new(&home.0);
        enroll::write_instance(
            &fs,
            &layout,
            &enroll::Instance {
                schema_version: PERSISTED_SCHEMA_VERSION,
                base_url: "https://topos.sh/api".to_owned(),
            },
        )
        .unwrap();
        let mut user = enroll::UserDoc {
            schema_version: PERSISTED_SCHEMA_VERSION,
            ..Default::default()
        };
        enroll::upsert_membership(
            &mut user,
            enroll::Membership {
                workspace_id: "w_acme".to_owned(),
                name: "acme".to_owned(),
                display_name: "Acme".to_owned(),
                enrolled_at: 0,
            },
        );
        enroll::write_user(&fs, &layout, &user).unwrap();
        // Two follow rows: one live (its sync doc still holds the never-received baseline — a
        // pending first-receive offer), one excluded on this device (not counted).
        enroll::write_follows_merged(
            &fs,
            &layout,
            &[
                enroll::FollowEntry {
                    skill_id: "s_deploy".to_owned(),
                    workspace_id: "w_acme".to_owned(),
                    mode: enroll::FollowModeDoc::Auto,
                    review_required: false,
                    following: true,
                    excluded_here: false,
                    agents: Vec::new(),
                    excluded_agents: Vec::new(),
                },
                enroll::FollowEntry {
                    skill_id: "s_laptop_only".to_owned(),
                    workspace_id: "w_acme".to_owned(),
                    mode: enroll::FollowModeDoc::Auto,
                    review_required: false,
                    following: true,
                    excluded_here: true,
                    agents: Vec::new(),
                    excluded_agents: Vec::new(),
                },
            ],
        )
        .unwrap();
        let sid = crate::id::SkillId::parse("s_deploy").unwrap();
        let sync_path = layout.published(&sid).sync;
        std::fs::create_dir_all(sync_path.parent().unwrap()).unwrap();
        doc::write_doc(
            &fs,
            &sync_path,
            &SyncState {
                schema_version: PERSISTED_SCHEMA_VERSION,
                observed: 0,
                observed_version_id: ZERO_HEX.to_owned(),
                applied: 0,
                base_commit: ZERO_HEX.to_owned(),
                work_hash: ZERO_HEX.to_owned(),
                held: false,
            },
        )
        .unwrap();

        let data = snapshot(&home);
        assert!(data.enrolled && !data.signed_in);
        assert_eq!(data.server.as_deref(), Some("https://topos.sh/api"));
        assert_eq!(data.workspaces.len(), 1);
        assert_eq!(data.workspaces[0].name, "acme");
        assert_eq!(data.followed_skills, 1, "the exclusion is not counted");
        assert_eq!(data.pending_offers, Some(1), "the baseline is the offer");
    }
}
