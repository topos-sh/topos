//! The verb-reshape TAIL seams closed in this leg, over fakes (no HTTP):
//! - **keep-as-yours** — `add <name>` on a RETAINED withdrawn/detached copy re-forks it into a NEW
//!   local skill (bytes rendered from the sidecar store, the old ghost follow entry retired);
//! - **`update` selectors + multi-target** — the `--channel`/`--skill`/multi-name resolution is
//!   all-or-none and dispatches to the targeted per-skill path or the channel-filtered sync.
//!
//! The `list` offline `behind`/`removed-upstream` columns + the `--channel`/`--skill` row filters, the
//! `-s '*'` skill enumeration, and the withdrawn `next_action` are unit-tested in their own modules
//! (`ops::list`, `git_source`, `render`).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::persisted::Lock;
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireMe, WireProposalIndex, WireReach, WireSkillIndex,
    WireSkillIndexEntry, WireSkillLog,
};
use topos_types::{CurrencyKind, Generation, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::enroll::{self, FollowEntry, FollowModeDoc};
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::{RealClock, RealIds};
use crate::plane::{
    DeliverySkill, DeliverySnapshot, DeliverySource, DirectorySource, InertFollow, InertPlane,
    PlaneError,
};
use crate::sidecar::Layout;
use crate::{doc, ops};

const WS: &str = "w_acme";
const API: &str = "https://api.acme.test";

fn scratch(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("topos-seams-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A harness that recognizes nothing — an `add` of a plain dir tracks it in place with no currency.
struct NullHarness;
impl HarnessAdapter for NullHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        Vec::new()
    }
    fn placement_for(
        &self,
        skill_id: &str,
        _n: topos_harness::PlacementNaming<'_>,
        _d: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: PathBuf::from("/nonexistent").join(skill_id),
        }
    }
    fn currency_kind(&self) -> CurrencyKind {
        CurrencyKind::ExplicitPullOnly
    }
    fn install_currency_trigger(&self) -> TriggerReport {
        no_trigger()
    }
    fn remove_currency_trigger(&self) -> TriggerReport {
        no_trigger()
    }
    fn uninstall_footprint(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}
fn no_trigger() -> TriggerReport {
    TriggerReport {
        harness: HarnessId::ClaudeCode,
        currency_kind: CurrencyKind::ExplicitPullOnly,
        touched_path: None,
        marker_id: "test".into(),
        state: TriggerState::Inactive,
    }
}

/// A rig with a real `~/.topos/` home + a real work root for skill dirs (the harness recognizes nothing,
/// so an adopt is a plain in-place track).
struct Rig {
    home: PathBuf,
    work: PathBuf,
    fs: RealFs,
    ids: RealIds,
    clock: RealClock,
    harness: NullHarness,
}

impl Rig {
    fn new(tag: &str) -> Self {
        Self {
            home: scratch(&format!("{tag}-home")),
            work: scratch(&format!("{tag}-work")),
            fs: RealFs,
            ids: RealIds,
            clock: RealClock,
            harness: NullHarness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home)
    }
    fn ctx<'a>(&'a self, plane: &'a InertPlane, follow: &'a InertFollow) -> Ctx<'a> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_seams".into(),
            layout: self.layout(),
            harness: &self.harness,
            plane,
            follow,
        }
    }

    /// Adopt a skill dir under `work/<name>/` (a plain `SKILL.md` body → the name is the basename), then
    /// mark it FOLLOWED in `follows.json` + write a workspace credential. Returns `(skill_id, dir)`.
    fn lay_followed(&self, name: &str, following: bool) -> (String, PathBuf) {
        let dir = self.work.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("# {name}\nbody\n")).unwrap();
        let plane = InertPlane;
        let follow = InertFollow;
        let ctx = self.ctx(&plane, &follow);
        let data = ops::add(&ctx, &dir).unwrap();
        enroll::write_follows_merged(
            &self.fs,
            &self.layout(),
            &[FollowEntry {
                skill_id: data.skill_id.clone(),
                workspace_id: WS.to_owned(),
                mode: FollowModeDoc::Auto,
                review_required: false,
                following,
                excluded_here: false,
            }],
        )
        .unwrap();
        enroll::write_credential(&self.fs, &self.layout(), WS, "wsc").unwrap();
        (data.skill_id, dir)
    }

    /// Seed the enrolled member docs `build_universe_via` reads (instance + one membership).
    fn seed_enrolled(&self) {
        enroll::write_instance(
            &self.fs,
            &self.layout(),
            &enroll::Instance {
                schema_version: 1,
                base_url: API.to_owned(),
                deployment_mode: topos_types::bootstrap::DeploymentMode::Cloud,
                enrollment_method: "device_code".to_owned(),
            },
        )
        .unwrap();
        let mut user = enroll::UserDoc {
            schema_version: 1,
            email: None,
            principal: Some("alice@acme.com".to_owned()),
            workspaces: Vec::new(),
        };
        enroll::upsert_membership(
            &mut user,
            enroll::Membership {
                workspace_id: WS.to_owned(),
                display_name: Some("Acme".to_owned()),
                roles: Vec::new(),
                verified_domain: None,
                verified_domain_status: topos_types::bootstrap::VerifiedDomainStatus::Unverified,
                invite_rooted: false,
                enrolled_at: 1,
            },
        );
        enroll::write_user(&self.fs, &self.layout(), &user).unwrap();
        enroll::write_credential(&self.fs, &self.layout(), WS, "wsc").unwrap();
    }

    fn read_follows(&self) -> Vec<FollowEntry> {
        enroll::read_follows(&self.fs, &self.layout())
            .unwrap()
            .map(|f| f.follows)
            .unwrap_or_default()
    }
    fn tracked_ids(&self) -> Vec<String> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(self.layout().skills_dir()).unwrap() {
            let name = entry.unwrap().file_name().to_string_lossy().into_owned();
            if !name.starts_with('.') {
                out.push(name);
            }
        }
        out
    }
}

// =================================================================================================
// keep-as-yours
// =================================================================================================

#[test]
fn keep_as_yours_withdrawn_forks_a_new_local_skill_and_retires_the_ghost() {
    let rig = Rig::new("kay-withdrawn");
    let (old_id, dir) = rig.lay_followed("deploy", true);
    // Simulate an UPSTREAM WITHDRAWAL: the agent dir is cleaned, the follow entry stays `following`.
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(!dir.exists());

    let plane = InertPlane;
    let follow = InertFollow;
    let ctx = rig.ctx(&plane, &follow);

    // Bare `add deploy` DESCRIBES the fork (nothing changes yet), naming the withdrawal + the `--yes` argv.
    let describe = ops::keep_as_yours(&ctx, "deploy", false).unwrap();
    match describe {
        Some(ops::KeepAsYoursOutcome::Described { data, yes_argv }) => {
            assert_eq!(data.name, "deploy");
            assert!(matches!(
                data.reason,
                topos_types::results::KeepReason::WithdrawnUpstream
            ));
            assert_eq!(
                yes_argv,
                vec![
                    "topos".to_owned(),
                    "add".to_owned(),
                    "deploy".to_owned(),
                    "--yes".to_owned()
                ]
            );
        }
        other => panic!("expected a describe, got {other:?}"),
    }
    // Nothing changed: the ghost follow entry + the old sidecar survive the describe.
    assert!(rig.read_follows().iter().any(|f| f.skill_id == old_id));
    assert!(rig.tracked_ids().contains(&old_id));

    // `--yes` FORKS: the bytes are re-rendered into the placement, a NEW local skill is minted, and the
    // old ghost follow entry + sidecar are retired.
    let forked = ops::keep_as_yours(&ctx, "deploy", true).unwrap();
    let new_id = match forked {
        Some(ops::KeepAsYoursOutcome::Forked(data)) => {
            assert_eq!(data.name, "deploy");
            assert!(data.tracked);
            data.skill_id.clone()
        }
        other => panic!("expected a fork, got {other:?}"),
    };
    assert_ne!(new_id, old_id, "the fork is a fresh local identity");
    // The bytes are back on disk under the placement, and it is adopted anew.
    assert!(dir.join("SKILL.md").exists());
    // The old ghost is gone; the new local skill has NO follow entry (no upstream).
    assert!(!rig.read_follows().iter().any(|f| f.skill_id == old_id));
    assert!(!rig.read_follows().iter().any(|f| f.skill_id == new_id));
    let ids = rig.tracked_ids();
    assert!(!ids.contains(&old_id), "the old sidecar is retired");
    assert!(ids.contains(&new_id), "the fork is tracked");
}

#[test]
fn keep_as_yours_detached_with_a_draft_forks_in_place() {
    let rig = Rig::new("kay-detached");
    let (old_id, dir) = rig.lay_followed("docs", false); // detached: following == false, dirs KEPT
    // A local edit ahead of the base → the draft rides along.
    std::fs::write(dir.join("SKILL.md"), "# docs\nedited body\n").unwrap();

    let plane = InertPlane;
    let follow = InertFollow;
    let ctx = rig.ctx(&plane, &follow);

    let describe = ops::keep_as_yours(&ctx, "docs", false).unwrap();
    match describe {
        Some(ops::KeepAsYoursOutcome::Described { data, .. }) => {
            assert!(matches!(
                data.reason,
                topos_types::results::KeepReason::Detached
            ));
            assert!(data.has_draft, "the on-disk edit is the draft riding along");
        }
        other => panic!("expected a describe, got {other:?}"),
    }

    let new_id = match ops::keep_as_yours(&ctx, "docs", true).unwrap() {
        Some(ops::KeepAsYoursOutcome::Forked(data)) => data.skill_id.clone(),
        other => panic!("expected a fork, got {other:?}"),
    };
    assert_ne!(new_id, old_id);
    // The edited bytes are what got forked (adopted in place from the live dir).
    let contents = std::fs::read_to_string(dir.join("SKILL.md")).unwrap();
    assert!(contents.contains("edited body"));
    assert!(!rig.tracked_ids().contains(&old_id));
    assert!(rig.tracked_ids().contains(&new_id));
}

#[test]
fn keep_as_yours_fails_closed_on_ambiguous_drafts_and_loses_nothing() {
    // Two draft snapshots on the base (a crash mid-snapshot, or repeated withdraw/refollow, can leave
    // more than one) + a cleaned agent dir. Rendering only the base and retiring the sidecar would
    // permanently destroy the un-rendered draft — the "nothing is ever lost" contract forbids it. So
    // `--yes` must FAIL CLOSED, touching neither the follow entry nor the sidecar store.
    let rig = Rig::new("kay-ambiguous");
    let (old_id, dir) = rig.lay_followed("deploy", true);
    let sid = crate::id::SkillId::parse(&old_id).unwrap();

    // Snapshot two DISTINCT drafts onto the base.
    {
        let plane = InertPlane;
        let follow = InertFollow;
        let ctx = rig.ctx(&plane, &follow);
        let sp = ctx.layout.published(&sid);
        let lock: Lock = doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
        for body in ["# deploy\ndraft one\n", "# deploy\ndraft two\n"] {
            std::fs::write(dir.join("SKILL.md"), body).unwrap();
            let scanned = crate::scan::scan(&dir).unwrap();
            crate::ops::sync_engine::snapshot_draft(&ctx, &sp, &lock, &scanned).unwrap();
        }
    }
    // The upstream withdrawal cleans the agent dir (the drafts survive only in the sidecar store).
    std::fs::remove_dir_all(&dir).unwrap();

    let plane = InertPlane;
    let follow = InertFollow;
    let ctx = rig.ctx(&plane, &follow);
    // The describe still runs (a draft rides along — the user is not told "nothing to keep").
    match ops::keep_as_yours(&ctx, "deploy", false).unwrap() {
        Some(ops::KeepAsYoursOutcome::Described { data, .. }) => {
            assert!(
                data.has_draft,
                "an ambiguous history still has a draft to keep"
            );
        }
        other => panic!("expected a describe, got {other:?}"),
    }
    // `--yes` fails closed — and deletes NOTHING.
    assert!(
        matches!(
            ops::keep_as_yours(&ctx, "deploy", true),
            Err(ClientError::Corrupt(_))
        ),
        "an ambiguous retained history refuses the fork rather than lose a draft"
    );
    assert!(
        rig.read_follows().iter().any(|f| f.skill_id == old_id),
        "the follow entry survives the refused fork"
    );
    assert!(
        rig.tracked_ids().contains(&old_id),
        "the sidecar (with both drafts) survives — nothing was lost"
    );
}

#[test]
fn keep_as_yours_is_none_for_a_live_followed_skill() {
    let rig = Rig::new("kay-live");
    // A LIVE followed skill (following, dirs present, not excluded) is NOT a fork case — `add` stays the
    // ordinary already-tracked answer.
    let _ = rig.lay_followed("live", true);
    let plane = InertPlane;
    let follow = InertFollow;
    let ctx = rig.ctx(&plane, &follow);
    assert!(ops::keep_as_yours(&ctx, "live", false).unwrap().is_none());
    // An untracked name is also None (falls through to the normal discovery adopt).
    assert!(ops::keep_as_yours(&ctx, "ghost", false).unwrap().is_none());
}

// =================================================================================================
// update selectors + multi-target
// =================================================================================================

/// A minimal directory over which `update`'s selectors resolve. Only `me`/`channels_index`/
/// `skills_index` are read (by `build_universe_via`); the row ops are never reached.
struct FakeDir;
impl DirectorySource for FakeDir {
    fn me(&self, _ws: &str) -> Result<WireMe, ClientError> {
        Ok(WireMe {
            workspace_id: WS.into(),
            name: "acme".into(),
            display_name: "Acme".into(),
            address: "https://topos.sh/acme".into(),
            principal: "alice@acme.com".into(),
            role: "member".into(),
            invited_by: None,
            invite_policy: "members".into(),
        })
    }
    fn channels_index(&self, _ws: &str) -> Result<WireChannelIndex, ClientError> {
        Ok(WireChannelIndex {
            channels: vec![WireChannelEntry {
                name: "eng".into(),
                mode: "open".into(),
                builtin: false,
                member: true,
                member_count: 3,
                skills: Vec::new(),
            }],
        })
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
        Ok(WireSkillIndex {
            skills: vec![WireSkillIndexEntry {
                skill_id: "s_deploy".into(),
                name: "deploy".into(),
                status: "active".into(),
                version_id: "a".repeat(64),
                bundle_digest: "b".repeat(64),
                generation: Generation { epoch: 1, seq: 1 },
                display_name: None,
                updated_at: 0,
                open_proposals: 0,
            }],
        })
    }
    fn proposals_index(&self, _ws: &str) -> Result<WireProposalIndex, ClientError> {
        unreachable!()
    }
    fn skill_log(&self, _ws: &str, _s: &str) -> Result<WireSkillLog, ClientError> {
        unreachable!()
    }
    fn reach(&self, _ws: &str, _s: &str) -> Result<WireReach, ClientError> {
        unreachable!()
    }
    fn follow_skill(&self, _ws: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn unfollow_skill(&self, _ws: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_join(&self, _ws: &str, _c: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_leave(&self, _ws: &str, _c: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_place(&self, _ws: &str, _c: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_unplace(&self, _ws: &str, _c: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn exclude_device(&self, _ws: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn protect_skill(&self, _ws: &str, _s: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn protect_channel(&self, _ws: &str, _c: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn ack_notices(&self, _ws: &str, _ids: &[String]) -> Result<(), ClientError> {
        unreachable!()
    }
}

/// A delivery whose one workspace serves a caller-supplied snapshot (never reports, never fetches when
/// every delivered skill is already current).
struct FakeDelivery {
    snapshot: DeliverySnapshot,
}
impl DeliverySource for FakeDelivery {
    fn workspaces(&self) -> Vec<String> {
        vec![WS.to_owned()]
    }
    fn fetch_delivery(&self, _ws: &str) -> Result<DeliverySnapshot, PlaneError> {
        Ok(self.snapshot.clone())
    }
    fn report_applied(&self, _ws: &str, _a: &[(String, [u8; 32])]) -> Result<(), PlaneError> {
        Ok(())
    }
}

fn parse_hex32(s: &str) -> [u8; 32] {
    let bytes = hex::decode(s).unwrap();
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    out
}

#[test]
fn update_selectors_are_all_or_none() {
    let rig = Rig::new("upd-aon");
    rig.seed_enrolled();
    let plane = InertPlane;
    let follow = InertFollow;
    let ctx = rig.ctx(&plane, &follow);
    let connect = |_: &str| -> Box<dyn DirectorySource> { Box::new(FakeDir) };

    // An unresolvable `--skill` name refuses the WHOLE invocation (nothing pulled) — like `follow`.
    let err = ops::update_selective(&ctx, &connect, None, &[], &[], &["ghost".into()], None)
        .err()
        .unwrap();
    assert_eq!(err.code(), "NOT_FOUND");
}

#[test]
fn update_channel_selector_reaches_the_channel_path() {
    let rig = Rig::new("upd-chan-path");
    rig.seed_enrolled();
    let plane = InertPlane;
    let follow = InertFollow;
    let ctx = rig.ctx(&plane, &follow);
    let connect = |_: &str| -> Box<dyn DirectorySource> { Box::new(FakeDir) };

    // `--channel eng` resolves (eng is a real channel), then the channel path needs a delivery transport —
    // absent, it is a typed enrollment refusal (proving resolution succeeded + the channel path is taken).
    let err = ops::update_selective(&ctx, &connect, None, &[], &["eng".into()], &[], None)
        .err()
        .unwrap();
    assert_eq!(err.code(), "ENROLLMENT_FAILED");
}

#[test]
fn update_channel_filters_delivered_skills_by_via() {
    let rig = Rig::new("upd-chan-filter");
    rig.seed_enrolled();
    // Two followed skills already installed at genesis; the delivery serves both at (0,0) so neither
    // needs a byte fetch — the reconcile just re-affirms them, and the channel filter selects one.
    let (eng_id, _) = rig.lay_followed("deploy", true);
    let (rel_id, _) = rig.lay_followed("docs", true);
    let eng_ver = parse_hex32(&read_base(&rig, &eng_id));
    let rel_ver = parse_hex32(&read_base(&rig, &rel_id));

    let snapshot = DeliverySnapshot {
        skills: vec![
            delivered(&eng_id, "deploy", eng_ver, &["eng"]),
            delivered(&rel_id, "docs", rel_ver, &["release"]),
        ],
        detached: Vec::new(),
        excluded: Vec::new(),
        proposals_awaiting: 0,
        notices: Vec::new(),
        staleness_window_ms: 604_800_000,
    };
    let delivery = FakeDelivery { snapshot };
    let plane = InertPlane;
    let follow = InertFollow;
    let ctx = rig.ctx(&plane, &follow);
    let connect = |_: &str| -> Box<dyn DirectorySource> { Box::new(FakeDir) };

    let out = ops::update_selective(
        &ctx,
        &connect,
        Some(&delivery),
        &[],
        &["eng".into()],
        &[],
        None,
    )
    .unwrap();
    // Only the eng-delivered skill was touched; the release-only skill was filtered out.
    let touched: Vec<&str> = out.data.skills.iter().map(|s| s.skill.as_str()).collect();
    assert_eq!(
        touched,
        vec!["deploy"],
        "channel filter selects via eng only"
    );
}

fn read_base(rig: &Rig, skill_id: &str) -> String {
    let sid = crate::id::SkillId::parse(skill_id).unwrap();
    let lock: Lock = doc::read_doc(&rig.fs, &rig.layout().published(&sid).lock)
        .unwrap()
        .unwrap();
    lock.base_commit
}

fn delivered(id: &str, name: &str, version: [u8; 32], via: &[&str]) -> DeliverySkill {
    DeliverySkill {
        skill_id: id.to_owned(),
        name: name.to_owned(),
        review_required: false,
        version_id: version,
        generation: Generation { epoch: 0, seq: 0 },
        bundle_digest: [0u8; 32],
        via_channels: via.iter().map(|c| (*c).to_owned()).collect(),
        via_direct: true,
    }
}
