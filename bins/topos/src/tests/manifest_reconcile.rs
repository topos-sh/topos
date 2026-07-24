//! The MANIFEST reconcile over fakes (no HTTP): profile items land in the home dirs silently
//! (login was the acceptance — no offer step), project `topos.toml` refs land INSIDE the checkout
//! (+ `.git/info/exclude`), nearest-wins routes a name to the project scope, a manifest pin
//! overrides the served version, an ended session freezes-and-prints-once, and a profile drop
//! cleans the person-scope placements while the sidecar keeps every byte.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::Commit;
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireMe, WireProposalIndex, WireReach,
    WireSkillIndex, WireSkillIndexEntry, WireSkillLog,
};
use topos_types::results::PullAction;
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    DeliverySkill, DeliverySnapshot, DeliverySource, DirectorySource, FetchedFile, FetchedVersion,
    InertFollow, InertPlane, KnownCurrent, LinkStatus, PlaneError, PlaneSource, PointerFetch,
};
use crate::sessions::{self, SESSION_ACTIVE, SESSION_ENDED, Session};
use crate::sidecar::Layout;
use crate::{ops, sync_status};

const WS: &str = "w_eng";
const HOST: &str = "acme.test";
const WS_NAME: &str = "eng";

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-mrec-{tag}-{}-{n}", std::process::id()));
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

struct TmpHarness {
    skills_root: PathBuf,
}
impl HarnessAdapter for TmpHarness {
    fn id(&self) -> HarnessId {
        HarnessId::ClaudeCode
    }
    fn discover(&self) -> Vec<DiscoveredPlacement> {
        Vec::new()
    }
    fn placement_for(
        &self,
        skill_id: &str,
        naming: topos_harness::PlacementNaming<'_>,
        _d: Option<&DiscoveredPlacement>,
    ) -> PlacementTarget {
        PlacementTarget {
            dir: topos_harness::choose_skill_dir(
                &self.skills_root,
                skill_id,
                naming,
                &topos_harness::dir_taken,
                &|_| false,
            ),
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

/// The rig: a fake $HOME (with `.claude/` so claude-code detects), a sidecar under `<home>/.topos`,
/// and a work dir. The cwd each test chooses (a project checkout, or the bare home).
struct Rig {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: SeqIds,
    clock: FixedClock,
    harness: TmpHarness,
}
impl Rig {
    fn new(tag: &str) -> Self {
        let home = Scratch::new(&format!("{tag}-home"));
        // Make claude-code DETECTED (its config home exists) so the shared-dir-first policy
        // engages: person scope → `<home>/.agents/skills`, project scope → `<proj>/.agents/skills`.
        std::fs::create_dir_all(home.0.join(".claude")).unwrap();
        let work = Scratch::new(&format!("{tag}-work"));
        let harness = TmpHarness {
            skills_root: work.0.join("skills"),
        };
        Self {
            home,
            work,
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness,
        }
    }
    fn layout(&self) -> Layout {
        Layout::new(&self.home.0.join(".topos"))
    }
    fn ctx_at<'a>(&'a self, cwd: Option<&std::path::Path>) -> Ctx<'a> {
        Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            plane: &InertPlane,
            follow: &InertFollow,
            roots: Some(crate::ctx::AgentRoots {
                home: self.home.0.clone(),
                cwd: cwd.map(std::path::Path::to_path_buf),
            }),
        }
    }
    fn seed_session(&self) {
        sessions::upsert_session(
            &self.fs,
            &self.layout(),
            Session {
                host: HOST.into(),
                base_url: format!("https://{HOST}/api"),
                workspace_id: WS.into(),
                workspace_name: WS_NAME.into(),
                display_name: "Engineering".into(),
                session_id: "sn_1".into(),
                credential: "cred-1".into(),
                status: SESSION_ACTIVE.into(),
                logged_in_at: 1,
            },
        )
        .unwrap();
    }
}

/// A version whose bytes reproduce a REAL commit id (the engine re-verifies on apply).
struct Version {
    id: [u8; 32],
    digest: [u8; 32],
    fetched: FetchedVersion,
}
fn mk_version(files: &[(&str, FileMode, &[u8])]) -> Version {
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|(p, m, b)| ManifestEntry {
            path: (*p).to_owned(),
            mode: *m,
            content_sha256: digest::sha256(b),
        })
        .collect();
    let tree = digest::bundle_digest(&entries).unwrap();
    let id = topos_core::identity::commit_id(&Commit {
        parents: &[],
        tree,
        author: "d_pub",
        message: "genesis",
    })
    .unwrap();
    Version {
        id,
        digest: tree,
        fetched: FetchedVersion {
            parents: Vec::new(),
            author: "d_pub".into(),
            message: "genesis".into(),
            files: files
                .iter()
                .map(|(p, m, b)| FetchedFile {
                    path: (*p).to_owned(),
                    mode: *m,
                    bytes: b.to_vec(),
                })
                .collect(),
        },
    }
}

type CallLog = Arc<Mutex<Vec<String>>>;

/// The per-session plane fake: a delivery script + versions keyed by `(skill, version-hex)`.
#[derive(Clone)]
struct FakePlane {
    delivery: Arc<Mutex<Result<DeliverySnapshot, &'static str>>>,
    versions: HashMap<(String, String), FetchedVersion>,
    log: CallLog,
}
impl FakePlane {
    fn new(log: CallLog) -> Self {
        Self {
            delivery: Arc::new(Mutex::new(Ok(empty_snapshot()))),
            versions: HashMap::new(),
            log,
        }
    }
    fn with_version(mut self, skill: &str, v: &Version) -> Self {
        self.versions.insert(
            (skill.to_owned(), topos_core::digest::to_hex(&v.id)),
            v.fetched.clone(),
        );
        self
    }
    fn serve(&self, snap: DeliverySnapshot) {
        *self.delivery.lock().unwrap() = Ok(snap);
    }
    fn serve_not_found(&self) {
        *self.delivery.lock().unwrap() = Err("nf");
    }
    fn serve_unreachable(&self) {
        *self.delivery.lock().unwrap() = Err("unreachable");
    }
}
fn empty_snapshot() -> DeliverySnapshot {
    DeliverySnapshot {
        skills: Vec::new(),
        proposals_awaiting: 0,
        notices: Vec::new(),
        staleness_window_ms: 604_800_000,
        link_status: LinkStatus::Active,
    }
}
fn delivered(skill_id: &str, name: &str, v: &Version) -> DeliverySkill {
    DeliverySkill {
        skill_id: skill_id.into(),
        name: name.into(),
        review_required: false,
        version_id: v.id,
        generation: 1,
        bundle_digest: v.digest,
        via_channels: vec!["everyone".into()],
    }
}
impl PlaneSource for FakePlane {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        Err(PlaneError::NotFound)
    }
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        self.versions
            .get(&(skill_id.to_owned(), topos_core::digest::to_hex(&version_id)))
            .cloned()
            .ok_or(PlaneError::NotFound)
    }
}
impl DeliverySource for FakePlane {
    fn fetch_delivery(&self, _ws: &str) -> Result<DeliverySnapshot, PlaneError> {
        match &*self.delivery.lock().unwrap() {
            Ok(s) => Ok(s.clone()),
            Err(m) if *m == "unreachable" => Err(PlaneError::Unreachable("network down".into())),
            Err(_) => Err(PlaneError::NotFound),
        }
    }
    fn report_applied(&self, _ws: &str, applied: &[(String, [u8; 32])]) -> Result<(), PlaneError> {
        self.log
            .lock()
            .unwrap()
            .push(format!("report {}", applied.len()));
        Ok(())
    }
}

/// The per-session directory fake: the catalog + channel indexes + a recording profile lane;
/// everything else unreachable.
#[derive(Clone)]
struct FakeDirectory {
    skills: Vec<WireSkillIndexEntry>,
    channels: Vec<WireChannelEntry>,
    calls: CallLog,
    removal: crate::plane::ProfileRemoval,
    /// When set, the index reads fail (a transport fault) — the freeze suites flip it.
    unavailable: Arc<Mutex<bool>>,
}

impl FakeDirectory {
    fn new(skills: Vec<WireSkillIndexEntry>, channels: Vec<WireChannelEntry>) -> Self {
        Self {
            skills,
            channels,
            calls: Arc::new(Mutex::new(Vec::new())),
            removal: crate::plane::ProfileRemoval::Removed,
            unavailable: Arc::new(Mutex::new(false)),
        }
    }
    fn set_unavailable(&self, v: bool) {
        *self.unavailable.lock().unwrap() = v;
    }
    fn check_reachable(&self) -> Result<(), ClientError> {
        if *self.unavailable.lock().unwrap() {
            return Err(ClientError::Plane("directory unreachable".into()));
        }
        Ok(())
    }
}
fn catalog_entry(skill_id: &str, name: &str, v: &Version) -> WireSkillIndexEntry {
    WireSkillIndexEntry {
        skill_id: skill_id.into(),
        name: name.into(),
        kind: "skill".into(),
        status: "active".into(),
        version_id: topos_core::digest::to_hex(&v.id),
        bundle_digest: topos_core::digest::to_hex(&v.digest),
        generation: 1,
        display_name: None,
        updated_at: 0,
        open_proposals: 0,
        upstream_host: None,
        upstream_repo: None,
        upstream_path: None,
    }
}
impl DirectorySource for FakeDirectory {
    fn me(&self, _ws: &str) -> Result<WireMe, ClientError> {
        unreachable!("no me read in these flows")
    }
    fn channels_index(&self, _ws: &str) -> Result<WireChannelIndex, ClientError> {
        self.check_reachable()?;
        Ok(WireChannelIndex {
            channels: self.channels.clone(),
        })
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
        self.check_reachable()?;
        Ok(WireSkillIndex {
            skills: self.skills.clone(),
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
    fn channel_place(&self, _ws: &str, _c: &str, _s: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn channel_unplace(&self, _ws: &str, _c: &str, _s: &str) -> Result<(), ClientError> {
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
    fn profile_include_skill(
        &self,
        _ws: &str,
        skill_id: &str,
        pin: Option<&str>,
    ) -> Result<(), ClientError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("include {skill_id} pin={}", pin.unwrap_or("*")));
        Ok(())
    }
    fn profile_remove_skill(
        &self,
        _ws: &str,
        skill_id: &str,
    ) -> Result<crate::plane::ProfileRemoval, ClientError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("remove {skill_id}"));
        Ok(self.removal)
    }
    fn profile_include_channel(&self, _ws: &str, channel: &str) -> Result<(), ClientError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("include-channel {channel}"));
        Ok(())
    }
    fn profile_remove_channel(
        &self,
        _ws: &str,
        channel: &str,
    ) -> Result<crate::plane::ProfileRemoval, ClientError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("remove-channel {channel}"));
        Ok(self.removal)
    }
}

/// Write lanes the reconcile suites never exercise.
struct NoContribute;
impl crate::plane::ContributeSource for NoContribute {
    fn publish(
        &self,
        _b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no contribute in these flows")
    }
    fn propose(
        &self,
        _b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no contribute in these flows")
    }
    fn revert(
        &self,
        _b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no contribute in these flows")
    }
    fn review(
        &self,
        _b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no contribute in these flows")
    }
}
struct NoGovernance;
impl crate::plane::GovernanceSource for NoGovernance {
    fn invite(
        &self,
        _w: &str,
        _b: topos_types::requests::InvitationRequest,
    ) -> Result<topos_types::requests::InvitationData, ClientError> {
        unreachable!("no governance in these flows")
    }
}

fn connect<'a>(
    plane: &'a FakePlane,
    dir: &'a FakeDirectory,
) -> impl Fn(&Session) -> ops::SessionTransports + 'a {
    move |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    }
}

// =================================================================================================
// The tests.
// =================================================================================================

#[test]
fn profile_items_install_silently_and_the_cache_records_the_session() {
    let rig = Rig::new("profile");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    plane.serve(DeliverySnapshot {
        skills: vec![delivered("s_deploy", "deploy", &v)],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();

    // Installed SILENTLY (no offer — login was the acceptance), into the HOME shared dir.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::FastForwarded, "{:?}", out.warnings);
    // claude-code is not shared-dir covered, so the person scope lands the NATIVE placement
    // (the active adapter's skills root in this rig).
    let placed = rig.work.0.join("skills/deploy/SKILL.md");
    assert!(placed.exists(), "profile items land in the home-scope dirs");
    // The applied report went out; the offline cache carries the session identity + name.
    assert!(log.lock().unwrap().iter().any(|l| l == "report 1"));
    let status = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    let ws = &status.workspaces[WS];
    assert_eq!(ws.host.as_deref(), Some(HOST));
    assert_eq!(ws.workspace_name.as_deref(), Some(WS_NAME));
    assert_eq!(ws.delivered["s_deploy"].name, "deploy");
}

#[test]
fn a_project_manifest_lands_in_the_checkout_with_a_git_exclude() {
    let rig = Rig::new("project");
    rig.seed_session();
    let proj = Scratch::new("proj");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(
        proj.0.join("topos.toml"),
        format!("[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    // The profile delivers NOTHING — the demand is the project manifest's.
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();

    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::FastForwarded, "{:?}", out.warnings);
    // The bytes live INSIDE the checkout (claude-code's project dir), not the home-scope dirs.
    assert!(proj.0.join(".claude/skills/deploy/SKILL.md").exists());
    assert!(!rig.work.0.join("skills/deploy").exists());
    // …and stay out of commits: one `.git/info/exclude` line, idempotent across runs.
    let exclude = std::fs::read_to_string(proj.0.join(".git/info/exclude")).unwrap();
    assert!(exclude.contains("/.claude/skills/deploy/"), "{exclude}");
    let before = exclude.clone();
    let _ = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let after = std::fs::read_to_string(proj.0.join(".git/info/exclude")).unwrap();
    assert_eq!(before, after, "the exclude write is idempotent");
}

#[test]
fn nearest_wins_routes_a_profile_name_into_the_project() {
    let rig = Rig::new("nearest");
    rig.seed_session();
    let proj = Scratch::new("proj-nw");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(
        proj.0.join("topos.toml"),
        format!("[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    // The profile ALSO delivers the same name — the nearer project line wins the scope.
    plane.serve(DeliverySnapshot {
        skills: vec![delivered("s_deploy", "deploy", &v)],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert_eq!(
        out.data
            .skills
            .iter()
            .filter(|s| s.skill == "deploy")
            .count(),
        1,
        "one name, one reconcile"
    );
    assert!(proj.0.join(".claude/skills/deploy/SKILL.md").exists());
    assert!(!rig.work.0.join("skills/deploy").exists());
}

#[test]
fn a_manifest_pin_overrides_the_served_version() {
    let rig = Rig::new("pin");
    rig.seed_session();
    let proj = Scratch::new("proj-pin");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    let old = mk_version(&[("SKILL.md", FileMode::Regular, b"# v1\n" as &[u8])]);
    let new = mk_version(&[("SKILL.md", FileMode::Regular, b"# v2\n" as &[u8])]);
    std::fs::write(
        proj.0.join("topos.toml"),
        format!(
            "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"{}\"\n",
            topos_core::digest::to_hex(&old.id)
        ),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &old)
        .with_version("s_deploy", &new);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &new)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let placed = std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap();
    assert_eq!(placed, "# v1\n", "the pin's bytes land, not current's");
}

#[test]
fn an_ended_session_freezes_and_prints_once() {
    let rig = Rig::new("ended");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serve_not_found();
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        out.warnings.iter().any(|w| w.starts_with("SESSION_ENDED")),
        "{:?}",
        out.warnings
    );
    assert_eq!(out.access_gone, vec![WS_NAME.to_owned()]);
    let all = sessions::read_sessions(&rig.fs, &rig.layout()).unwrap();
    assert_eq!(all.sessions[0].status, SESSION_ENDED);
    // The second run skips the ended session — the line printed once.
    let out2 = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out2.warnings.is_empty(), "{:?}", out2.warnings);
}

#[test]
fn a_profile_drop_cleans_the_home_placements_and_keeps_the_sidecar() {
    let rig = Rig::new("drop");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serve(DeliverySnapshot {
        skills: vec![delivered("s_deploy", "deploy", &v)],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.exists());

    // The profile stops delivering it (removed on the web, or the entitlement ended).
    plane.serve(empty_snapshot());
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::Withdrawn);
    assert!(!placed.exists(), "the person-scope copy is cleaned");
    // Every sidecar byte stays (the store keeps the version; nothing is lost).
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(rig.layout().skill_dir(&sid).exists());
    // A re-delivery reinstalls (the never-received reset).
    plane.serve(DeliverySnapshot {
        skills: vec![delivered("s_deploy", "deploy", &v)],
        ..empty_snapshot()
    });
    let out3 = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let row3 = out3
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(
        row3.action,
        PullAction::FastForwarded,
        "{:?}",
        out3.warnings
    );
    assert!(placed.exists(), "re-delivery reinstalls");
}

#[test]
fn a_channel_reference_expands_against_the_session() {
    let rig = Rig::new("channel");
    rig.seed_session();
    let proj = Scratch::new("proj-ch");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(
        proj.0.join("topos.toml"),
        format!("[channels]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n"),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &v)],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_deploy".into(),
                name: "deploy".into(),
            }],
        }],
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(proj.0.join(".claude/skills/deploy/SKILL.md").exists());
}

#[test]
fn a_workspace_ref_without_a_session_is_an_honest_local_line() {
    let rig = Rig::new("nosession");
    // NO session at all — the manifest references a workspace this install never logged into.
    let proj = Scratch::new("proj-ns");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(
        proj.0.join("topos.toml"),
        "[skills]\n\"elsewhere.dev/ops/deploy\" = \"*\"\n",
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let w = out
        .warnings
        .iter()
        .find(|w| w.starts_with("NOT_AVAILABLE"))
        .expect("the honest line");
    assert!(w.contains("topos login elsewhere.dev/ops"), "{w}");
    assert!(w.contains("topos.toml"), "names the manifest: {w}");
}

#[test]
fn add_reference_records_the_manifest_line_and_delivers_now() {
    let rig = Rig::new("addref");
    rig.seed_session();
    let proj = Scratch::new("proj-add");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let data = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        "acme.test/eng/deploy",
        false,
    )
    .unwrap();
    // The receipt names the manifest FIRST, the canonical stored reference, and the inverse.
    let manifest = proj.0.join("topos.toml");
    assert_eq!(
        data.manifest.as_deref(),
        Some(&*manifest.display().to_string())
    );
    assert_eq!(data.reference.as_deref(), Some("acme.test/eng/deploy"));
    assert_eq!(data.undo, vec!["topos", "remove", "acme.test/eng/deploy"]);
    let m = crate::manifest::file::read_manifest(&rig.fs, &manifest)
        .unwrap()
        .unwrap();
    assert_eq!(m.skills[0].reference, "acme.test/eng/deploy");
    // `add` chooses; the same sweep delivers — bytes are in the checkout NOW.
    assert!(proj.0.join(".claude/skills/deploy/SKILL.md").exists());

    // A name in NO connected catalog refuses without an existence claim — ONE template, never
    // the context sentence nested inside the fixed uniform-miss wrapper.
    let err =
        ops::add_reference(&ctx, &connect(&plane, &dir), None, "@eng/nonesuch", false).unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND");
    assert!(
        err.to_string()
            .contains("not visible with your current access"),
        "{err}"
    );
    assert!(
        !err.to_string().contains("was not found, or is not visible"),
        "the two miss templates must never nest: {err}"
    );
    // A workspace this installation is not logged into names the login, from local knowledge.
    let err =
        ops::add_reference(&ctx, &connect(&plane, &dir), None, "@ops/deploy", false).unwrap_err();
    assert!(err.to_string().contains("topos login ops"), "{err}");
}

#[test]
fn add_reference_global_edits_the_server_profile() {
    let rig = Rig::new("addg");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serve(DeliverySnapshot {
        skills: vec![delivered("s_deploy", "deploy", &v)],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // A BARE catalog name resolves against the connected workspaces (unique match).
    let data = ops::add_reference(&ctx, &connect(&plane, &dir), None, "deploy", true).unwrap();
    assert_eq!(
        data.manifest.as_deref(),
        Some("your profile @ acme.test/eng")
    );
    assert_eq!(
        data.undo,
        vec!["topos", "remove", "-g", "acme.test/eng/deploy"]
    );
    assert!(
        dir.calls
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == "include s_deploy pin=*"),
        "{:?}",
        dir.calls.lock().unwrap()
    );
}

#[test]
fn remove_reference_global_names_how_the_removal_settled() {
    let rig = Rig::new("rmg");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let mut dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    dir.removal = crate::plane::ProfileRemoval::Excluded;
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out =
        ops::remove_reference_global(&ctx, &connect(&plane, &dir), "acme.test/eng/deploy").unwrap();
    assert!(matches!(
        out.items[0].kind,
        topos_types::results::RemoveKind::ManifestExcluded
    ));
    assert!(
        out.items[0]
            .note
            .as_deref()
            .is_some_and(|n| n.contains("exclude line")),
        "{:?}",
        out.items[0].note
    );
    assert_eq!(out.undo, vec!["topos", "add", "-g", "acme.test/eng/deploy"]);
}

#[test]
fn publish_transfer_rewrites_the_path_line_to_the_governed_reference() {
    let rig = Rig::new("transfer");
    let proj = Scratch::new("proj-tr");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(
        proj.0.join("topos.toml"),
        "[skills]\n\"./tools/my-skill\" = \"*\"\n",
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    let skill_dir = proj.0.join("tools/my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    // A DIFFERENT dir sharing the basename must never be rewritten (match is by resolved path).
    let decoy = Scratch::new("decoy-tr");
    let miss =
        ops::rewrite_to_governed(&ctx, "my-skill", HOST, WS_NAME, &[decoy.0.join("my-skill")])
            .unwrap();
    assert!(miss.is_none(), "a same-name foreign dir never matches");
    let dirs = vec![skill_dir];
    let rw = ops::rewrite_to_governed(&ctx, "my-skill", HOST, WS_NAME, &dirs)
        .unwrap()
        .expect("the path line is rewritten");
    assert_eq!(rw.canonical, format!("{HOST}/{WS_NAME}/my-skill"));
    assert_eq!(rw.from, "./tools/my-skill");
    let m = crate::manifest::file::read_manifest(&rig.fs, &proj.0.join("topos.toml"))
        .unwrap()
        .unwrap();
    assert_eq!(m.skills.len(), 1);
    assert_eq!(m.skills[0].reference, format!("{HOST}/{WS_NAME}/my-skill"));
    // A second publish finds no path line — the transfer is one-shot.
    assert!(
        ops::rewrite_to_governed(&ctx, "my-skill", HOST, WS_NAME, &dirs)
            .unwrap()
            .is_none()
    );
}

#[test]
fn publish_transfer_keeps_an_existing_governed_pin() {
    let rig = Rig::new("transfer-pin");
    let proj = Scratch::new("proj-pin");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    let pin = "c".repeat(64);
    std::fs::write(
        proj.0.join("topos.toml"),
        format!(
            "[skills]\n\"./tools/my-skill\" = \"*\"\n\"{HOST}/{WS_NAME}/my-skill\" = \"{pin}\"\n"
        ),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    let skill_dir = proj.0.join("tools/my-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let rw = ops::rewrite_to_governed(&ctx, "my-skill", HOST, WS_NAME, &[skill_dir])
        .unwrap()
        .expect("the path line is removed");
    assert_eq!(rw.from, "./tools/my-skill");
    let m = crate::manifest::file::read_manifest(&rig.fs, &proj.0.join("topos.toml"))
        .unwrap()
        .unwrap();
    assert_eq!(m.skills.len(), 1, "the governed entry survives alone");
    assert_eq!(m.skills[0].reference, format!("{HOST}/{WS_NAME}/my-skill"));
    assert_eq!(
        m.skills[0].pin.as_deref(),
        Some(pin.as_str()),
        "the standing pin is never clobbered"
    );
}

#[test]
fn a_remote_add_gets_the_governed_copy_suggestion_from_the_catalog_upstream_fields() {
    use crate::source::{GitHost, RemoteSpec};

    let rig = Rig::new("dedup");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    // The catalog's one entry carries the ADDITIVE upstream provenance the suggestion matches on.
    let mut entry = catalog_entry("s_deploy", "deploy", &v);
    entry.upstream_host = Some("github.com".into());
    entry.upstream_repo = Some("owner/dedup".into());
    entry.upstream_path = Some("skills/deploy".into());
    let dir = FakeDirectory::new(vec![entry], Vec::new());
    let ctx = rig.ctx_at(None);
    let spec = |subdir: Option<&str>, repo: &str| RemoteSpec {
        host: GitHost::GitHub,
        owner: "owner".into(),
        repo: repo.into(),
        git_ref: None,
        subdir: subdir.map(str::to_owned),
    };

    // The exact subdir → the path-exact suggestion, spelled as the paste-ready workspace ref.
    let got = ops::governed_copy_suggestion(
        &ctx,
        &connect(&plane, &dir),
        &spec(Some("skills/deploy"), "dedup"),
        None,
    )
    .expect("a governed copy is suggested");
    assert_eq!(got.workspace, WS_NAME);
    assert_eq!(got.name, "deploy");
    assert_eq!(got.reference, "acme.test/eng/deploy");
    assert!(got.same_path);

    // The same repository at another path still gets named — honestly, as a sibling.
    let sibling =
        ops::governed_copy_suggestion(&ctx, &connect(&plane, &dir), &spec(None, "dedup"), None)
            .expect("the same-repo sibling is suggested");
    assert!(!sibling.same_path);
    assert_eq!(sibling.reference, "acme.test/eng/deploy");

    // A bare `add owner/repo` whose import RESOLVED to the governed subdir (a --skill pick or a
    // multi-skill repo) is path-exact by the recorded origin, not the spec's spelling.
    let resolved = ops::governed_copy_suggestion(
        &ctx,
        &connect(&plane, &dir),
        &spec(None, "dedup"),
        Some("skills/deploy"),
    )
    .expect("the resolved-subdir import matches path-exactly");
    assert!(resolved.same_path);

    // A repo nobody governs answers nothing (the import proceeds with no notice).
    assert!(
        ops::governed_copy_suggestion(&ctx, &connect(&plane, &dir), &spec(None, "other"), None)
            .is_none()
    );

    // An ENDED session's catalog is never consulted — with the one session ended, no suggestion.
    sessions::set_session_status(&rig.fs, &rig.layout(), HOST, WS, SESSION_ENDED).unwrap();
    assert!(
        ops::governed_copy_suggestion(
            &ctx,
            &connect(&plane, &dir),
            &spec(Some("skills/deploy"), "dedup"),
            None
        )
        .is_none()
    );
}

// =================================================================================================
// Excludes vs channel expansion + the channel-member freeze suites.
// =================================================================================================

/// A project with one channel line delivering `deploy`, plus the fakes serving it.
fn channel_project(tag: &str) -> (Rig, Scratch, FakePlane, FakeDirectory, Version) {
    let rig = Rig::new(tag);
    rig.seed_session();
    let proj = Scratch::new(&format!("proj-{tag}"));
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(
        proj.0.join("topos.toml"),
        format!("[channels]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n"),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &v)],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_deploy".into(),
                name: "deploy".into(),
            }],
        }],
    );
    (rig, proj, plane, dir, v)
}

#[test]
fn an_exclude_line_withholds_a_channel_member() {
    let (rig, proj, plane, dir, _v) = channel_project("chexcl");
    // The manifest carries the channel AND an exclude of one member — the exclude wins.
    std::fs::write(
        proj.0.join("topos.toml"),
        format!(
            "exclude = [\"deploy\"]\n[channels]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n"
        ),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        !proj.0.join(".claude/skills/deploy").exists(),
        "an excluded member never lands, however the channel expands"
    );
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "deploy"),
        "no row for a withheld name"
    );
}

#[test]
fn remove_of_a_channel_member_records_an_exclude_and_the_sweep_round_trips() {
    let (rig, proj, plane, dir, _v) = channel_project("chrm");
    let ctx = rig.ctx_at(Some(&proj.0));
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.exists());

    // `remove deploy` claims the manifest arm (the channel provides it — the recorded
    // `via_manifest` cache row is the offline fact): an exclude line, never a tracked-delete.
    let provided = ops::profile_provided_names(&ctx);
    assert!(
        provided
            .iter()
            .any(|p| p.name == "deploy" && p.via_manifest),
        "the cache records the channel-member provenance"
    );
    let out = ops::remove_from_manifests(&ctx, &["deploy".to_owned()], &provided)
        .unwrap()
        .expect("the manifest arm claims a channel-delivered name");
    assert!(matches!(
        out.items[0].kind,
        topos_types::results::RemoveKind::ManifestExcluded
    ));
    assert!(
        out.items[0]
            .note
            .as_deref()
            .is_some_and(|n| n.contains("channel 'backend'")),
        "{:?}",
        out.items[0].note
    );
    let m = crate::manifest::file::read_manifest(&rig.fs, &proj.0.join("topos.toml"))
        .unwrap()
        .unwrap();
    assert_eq!(m.exclude.len(), 1);

    // The next sweep withholds the member AND cleans its in-checkout dir — no re-install loop.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(!placed.exists(), "the exclusion retires the placement");

    // Adding it back lifts the exclude; the same sweep re-delivers.
    let mut data = topos_types::results::AddData {
        skill_id: String::new(),
        name: String::new(),
        version_id: "0".repeat(64),
        bundle_digest: "0".repeat(64),
        tracked: true,
        harness: None,
        harness_slug: None,
        currency: None,
        triggers: Vec::new(),
        origin: None,
        manifest: None,
        reference: None,
        undo: Vec::new(),
        governed_copy: None,
    };
    ops::note_added(
        &ctx,
        &mut data,
        &format!("{HOST}/{WS_NAME}/deploy"),
        None,
        false,
    )
    .unwrap();
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        placed.exists(),
        "add-back lifts the exclude and re-delivers"
    );
}

#[test]
fn an_offline_sweep_freezes_channel_delivered_project_skills() {
    let (rig, proj, plane, dir, _v) = channel_project("choffline");
    let ctx = rig.ctx_at(Some(&proj.0));
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.exists());

    // The whole plane is down (the session-start hook's world): NOTHING may be deleted.
    plane.serve_unreachable();
    dir.set_unavailable(true);
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(placed.exists(), "an offline sweep freezes, never cleans");
    assert!(
        !out.data
            .skills
            .iter()
            .any(|s| s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );
    // Back online: the member is still delivered (the cache rows survived the outage too).
    plane.serve(empty_snapshot());
    dir.set_unavailable(false);
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(placed.exists());
}

#[test]
fn an_ended_session_freezes_channel_delivered_project_skills() {
    let (rig, proj, plane, dir, _v) = channel_project("chended");
    let ctx = rig.ctx_at(Some(&proj.0));
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.exists());

    // The session ends server-side (uniform 404): the freeze covers the channel's members.
    plane.serve_not_found();
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        placed.exists(),
        "an ended session freezes the member in place"
    );
    // And the NEXT sweep (the session now locally ended) still leaves it alone.
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(placed.exists());
}

#[test]
fn publish_to_takes_an_existing_channel_never_a_silent_mint() {
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(
        Vec::new(),
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: Vec::new(),
        }],
    );
    let mk_lane = || ops::WriteLane {
        host: HOST.into(),
        workspace_name: WS_NAME.into(),
        workspace_id: WS.into(),
        base_url: format!("https://{HOST}/api"),
        transports: ops::SessionTransports {
            plane: Box::new(plane.clone()),
            directory: Box::new(dir.clone()),
            contribute: Box::new(NoContribute),
            governance: Box::new(NoGovernance),
        },
    };
    let lane = mk_lane();
    // An existing channel passes; no `--to` passes trivially.
    ops::check_channel_exists(&*lane.transports.directory, &lane, Some("backend")).unwrap();
    ops::check_channel_exists(&*lane.transports.directory, &lane, None).unwrap();
    // A nonexistent channel refuses typed, naming the create path — NEVER a silent mint.
    let err =
        ops::check_channel_exists(&*lane.transports.directory, &lane, Some("acme")).unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND", "{err}");
    assert!(
        err.to_string().contains("create 'acme' on the web"),
        "{err}"
    );
    assert!(err.to_string().contains("nothing was published"), "{err}");
    // The value equal to the WORKSPACE slug gets the pointed fix.
    let err =
        ops::check_channel_exists(&*lane.transports.directory, &lane, Some(WS_NAME)).unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err}");
    assert!(
        err.to_string()
            .contains("names the workspace, not a channel"),
        "{err}"
    );
    // An unreadable index refuses retryable rather than risk a silent server-side create.
    dir.set_unavailable(true);
    let err =
        ops::check_channel_exists(&*lane.transports.directory, &lane, Some("backend")).unwrap_err();
    assert_eq!(err.code(), "PLANE_ERROR", "{err}");
}

#[test]
fn an_unreadable_catalog_answers_retryable_never_a_false_miss() {
    // A transient catalog-read failure must NOT race into "not in any connected workspace's
    // catalog" — the deterministic answer is a retryable transport error naming the unread
    // catalogs, for `add` and `remove -g` alike.
    let rig = Rig::new("flaky-read");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    dir.set_unavailable(true);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::add_reference(&ctx, &connect(&plane, &dir), None, "deploy", false).unwrap_err();
    assert_eq!(err.code(), "PLANE_ERROR", "{err}");
    assert_eq!(
        err.outcome(),
        topos_types::TerminalOutcome::RetryableFailure
    );
    assert!(err.to_string().contains("could not be read"), "{err}");
    let err = ops::remove_reference_global(&ctx, &connect(&plane, &dir), "deploy").unwrap_err();
    assert_eq!(err.code(), "PLANE_ERROR", "{err}");
    // Reachable again: the genuine miss keeps the single-template NOT_FOUND.
    dir.set_unavailable(false);
    let err = ops::add_reference(&ctx, &connect(&plane, &dir), None, "deploy", false).unwrap_err();
    assert_eq!(err.code(), "NOT_FOUND", "{err}");
}

#[test]
fn a_transient_channel_index_failure_never_deletes() {
    let (rig, proj, plane, dir, _v) = channel_project("chflaky");
    let ctx = rig.ctx_at(Some(&proj.0));
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.exists());

    // Delivery answers fine but the channel index read fails (a transient 500).
    plane.serve(empty_snapshot());
    dir.set_unavailable(true);
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        placed.exists(),
        "a transient index failure freezes the members: {:?}",
        out.warnings
    );
}

#[test]
fn a_broader_exclude_never_suppresses_a_nearer_channel_member() {
    // Nearest wins per NAME: a PERSONAL-manifest exclude (the broadest local layer) must not
    // withhold a member a NEARER project channel delivers — the project's demand wins whole.
    let (rig, proj, plane, dir, _v) = channel_project("chnear");
    std::fs::create_dir_all(rig.layout().home()).unwrap();
    std::fs::write(
        rig.layout().home().join("topos.toml"),
        "exclude = [\"deploy\"]\n",
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        proj.0.join(".claude/skills/deploy/SKILL.md").exists(),
        "the nearer project channel's member is delivered; the broader exclude is shadowed"
    );
}

#[test]
fn a_dropped_personal_line_cleans_the_written_home_placement() {
    // A DIRECT personal-manifest reference delivers to the home scope; removing the line must
    // retire the placement topos WROTE (snapshot-first, sidecar kept) — not leave it forever.
    let rig = Rig::new("perso-drop");
    rig.seed_session();
    std::fs::create_dir_all(rig.layout().home()).unwrap();
    std::fs::write(
        rig.layout().home().join("topos.toml"),
        format!("[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n" as &[u8])]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.exists(), "the personal line delivered to home scope");

    // Drop the line; the next sweep cleans the WRITTEN placement and keeps the sidecar.
    std::fs::write(rig.layout().home().join("topos.toml"), "").unwrap();
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "deploy" && s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );
    assert!(!placed.exists(), "the written home placement is retired");
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "every sidecar byte stays"
    );
    // The row lapsed with the clean — the sweep after is quiet.
    let out2 = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        !out2
            .data
            .skills
            .iter()
            .any(|s| s.action == PullAction::Withdrawn),
        "{:?}",
        out2.data.skills
    );
}
