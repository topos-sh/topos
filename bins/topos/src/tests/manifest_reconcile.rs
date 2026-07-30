//! The RECONCILE over fakes (no HTTP): two UNBLENDED scopes, each converged on its own recipe.
//!
//! The implicit person recipe adopts every connected workspace's feed; a global FILE is complete
//! (only its rows deliver, and it says so loudly when a workspace's feed is left out); an `"off"`
//! row withholds exactly its bundle; an explicit row's pin beats the feed's and a set's version of
//! the same identity; the NEAREST project file governs whole; the same bundle at both scopes lands
//! twice with two stores; git rows move only on an explicit update; a dropped row cleans
//! snapshot-first; and `--rebuild` absorbs before it drops.

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
/// and a work dir. The cwd each test chooses (a project checkout, or the bare work dir).
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
    /// Write the GLOBAL manifest (`~/.topos/topos.toml`) — the person scope's complete recipe.
    fn write_global(&self, body: &str) {
        let home = self.layout().home().to_path_buf();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(crate::manifest::MANIFEST_FILE), body).unwrap();
    }
}

/// A project checkout carrying `body` as its `topos.toml`.
fn project(tag: &str, body: &str) -> Scratch {
    let proj = Scratch::new(tag);
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), body).unwrap();
    proj
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

/// A one-file skill bundle (the common fixture).
fn one_file(body: &[u8]) -> Version {
    mk_version(&[("SKILL.md", FileMode::Regular, body)])
}

/// Every file under a dir as sorted `(rel-path, bytes)` rows — the byte-destruction witness.
fn snapshot_dir(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    fn walk(base: &std::path::Path, d: &std::path::Path, out: &mut Vec<(String, Vec<u8>)>) {
        for e in std::fs::read_dir(d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else {
                out.push((
                    p.strip_prefix(base).unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// How many versions a store holds — the snapshot-first witness (base + each absorbed draft).
fn store_versions(layout: &Layout, skill_id: &str) -> usize {
    let sid = crate::id::SkillId::parse(skill_id).unwrap();
    let sp = layout.published(&sid);
    topos_gitstore::Store::open(&sp.store)
        .unwrap()
        .list_versions()
        .unwrap()
        .len()
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
    /// Serve exactly these bundles (the common case).
    fn serves(&self, skills: Vec<DeliverySkill>) {
        self.serve(DeliverySnapshot {
            skills,
            ..empty_snapshot()
        });
    }
    fn serve_not_found(&self) {
        *self.delivery.lock().unwrap() = Err("nf");
    }
    fn serve_unreachable(&self) {
        *self.delivery.lock().unwrap() = Err("unreachable");
    }
    /// The plane ANSWERED, with a failure (a 5xx) — not the same fact as an unreachable server.
    fn serve_unavailable(&self) {
        *self.delivery.lock().unwrap() = Err("unavailable");
    }
    /// The plane answered and the answer got CUT OFF mid-body — the other half of the same
    /// variant, where nothing says the server itself failed.
    fn serve_truncated(&self) {
        *self.delivery.lock().unwrap() = Err("truncated");
    }
    /// The plane ANSWERED, unreadably (garbled bytes) — not a network fault at all.
    fn serve_malformed(&self) {
        *self.delivery.lock().unwrap() = Err("malformed");
    }
}
fn empty_snapshot() -> DeliverySnapshot {
    DeliverySnapshot {
        skills: Vec::new(),
        proposals_awaiting: 0,
        notices: Vec::new(),
        staleness_window_ms: 604_800_000,
        link_status: LinkStatus::Active,
        declined: Vec::new(),
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
        assigned_by: None,
        picked: false,
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
            Err(m) if *m == "unavailable" => Err(PlaneError::Unavailable("HTTP 500".into())),
            // What the real transport reports when the body read faults part-way (the same
            // variant as a 5xx, but the server may have answered perfectly well).
            Err(m) if *m == "truncated" => Err(PlaneError::Unavailable(
                "read body: unexpected end of stream".into(),
            )),
            Err(m) if *m == "malformed" => Err(PlaneError::Malformed(
                "delivery body: expected object".into(),
            )),
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

/// The per-session directory fake: the catalog + channel indexes; everything else unreachable.
#[derive(Clone)]
struct FakeDirectory {
    skills: Vec<WireSkillIndexEntry>,
    channels: Vec<WireChannelEntry>,
    /// When set, the index reads fail (a transport fault) — the freeze suites flip it.
    unavailable: Arc<Mutex<bool>>,
}

impl FakeDirectory {
    fn new(skills: Vec<WireSkillIndexEntry>, channels: Vec<WireChannelEntry>) -> Self {
        Self {
            skills,
            channels,
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

/// The bare sweep (no forge lane — the background posture).
fn sweep(ctx: &Ctx<'_>, plane: &FakePlane, dir: &FakeDirectory) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap()
}

// =================================================================================================
// The person scope: the implicit recipe, and the complete file.
// =================================================================================================

#[test]
fn the_implicit_recipe_adopts_every_connected_feed() {
    let rig = Rig::new("implicit");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    let mut ds = delivered("s_deploy", "deploy", &v);
    ds.assigned_by = Some("Ada".into());
    plane.serve(DeliverySnapshot {
        skills: vec![ds],
        declined: vec![("s_old".into(), "retired".into())],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // No global file: the machine behaves exactly as if it held one feed row per connected
    // workspace — installed silently (the login was the acceptance), into the home dirs.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::FastForwarded, "{:?}", out.warnings);
    assert_eq!(row.scope.as_deref(), Some("person"));
    assert!(rig.work.0.join("skills/deploy/SKILL.md").exists());
    // The applied report went out; the offline cache carries the identity, the attribution, and
    // the caller's declines.
    assert!(log.lock().unwrap().iter().any(|l| l == "report 1"));
    let status = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    let ws = &status.workspaces[WS];
    assert_eq!(ws.host.as_deref(), Some(HOST));
    assert_eq!(ws.workspace_name.as_deref(), Some(WS_NAME));
    assert_eq!(ws.delivered["s_deploy"].name, "deploy");
    assert_eq!(ws.delivered["s_deploy"].assigned_by.as_deref(), Some("Ada"));
    assert!(!ws.delivered["s_deploy"].via_manifest);
    assert_eq!(
        ws.declined.get("s_old").map(String::as_str),
        Some("retired")
    );
    // Nothing is loud: the implicit recipe adopts everything, so there is nothing to disclose.
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.starts_with("GLOBAL_MANIFEST")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn a_global_file_withholds_the_feed_and_says_so_loudly() {
    let rig = Rig::new("filewithholds");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let deploy = one_file(b"# deploy\n");
    let other = one_file(b"# other\n");
    // The file names ONE bundle and no feed row — it is a complete recipe, so the rest of what the
    // workspace assigns does not flow here.
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/other\" = \"*\"\n"));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &deploy)
        .with_version("s_other", &other);
    plane.serves(vec![
        delivered("s_deploy", "deploy", &deploy),
        delivered("s_other", "other", &other),
    ]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &deploy),
            catalog_entry("s_other", "other", &other),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert!(
        rig.work.0.join("skills/other/SKILL.md").exists(),
        "the file's own row delivers: {:?}",
        out.warnings
    );
    assert!(
        !rig.work.0.join("skills/deploy").exists(),
        "no feed row, no feed"
    );
    let loud = out
        .warnings
        .iter()
        .find(|w| w.starts_with("GLOBAL_MANIFEST"))
        .expect("the loud line");
    assert!(loud.contains(&format!("{HOST}/{WS_NAME}")), "{loud}");
    assert!(loud.contains("no feed row"), "{loud}");
    assert!(loud.contains("adopts 1 bundles"), "{loud}");
    assert!(
        loud.contains("1 assigned bundles are not adopted"),
        "{loud}"
    );
    assert!(loud.contains(&format!("topos add -g @{WS_NAME}")), "{loud}");
}

#[test]
fn an_off_row_withholds_exactly_its_bundle_from_a_flowing_feed() {
    let rig = Rig::new("offrow");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let deploy = one_file(b"# deploy\n");
    let noisy = one_file(b"# noisy\n");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/noisy\" = \"off\"\n"
    ));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &deploy)
        .with_version("s_noisy", &noisy);
    plane.serves(vec![
        delivered("s_deploy", "deploy", &deploy),
        delivered("s_noisy", "noisy", &noisy),
    ]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &deploy),
            catalog_entry("s_noisy", "noisy", &noisy),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        rig.work.0.join("skills/deploy/SKILL.md").exists(),
        "the feed flows: {:?}",
        out.warnings
    );
    assert!(
        !rig.work.0.join("skills/noisy").exists(),
        "the one switch is the one exception"
    );
    // A flowing feed is not a withheld one — no loud line here.
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.starts_with("GLOBAL_MANIFEST")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn an_explicit_pinned_row_beats_the_feeds_version() {
    let rig = Rig::new("pinbeats");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let old = one_file(b"# v1\n");
    let new = one_file(b"# v2\n");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"{}\"\n",
        topos_core::digest::to_hex(&old.id)
    ));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &old)
        .with_version("s_deploy", &new);
    plane.serves(vec![delivered("s_deploy", "deploy", &new)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &new)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read_to_string(rig.work.0.join("skills/deploy/SKILL.md")).unwrap(),
        "# v1\n",
        "the row's pin lands, not the feed's current"
    );
    assert_eq!(
        out.data
            .skills
            .iter()
            .filter(|s| s.skill == "deploy")
            .count(),
        1,
        "one identity, one delivery per scope"
    );
}

#[test]
fn a_declined_bundle_a_row_still_delivers_is_disclosed() {
    let rig = Rig::new("declined");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serve(DeliverySnapshot {
        skills: Vec::new(),
        declined: vec![("s_deploy".into(), "deploy".into())],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(rig.work.0.join("skills/deploy/SKILL.md").exists());
    let line = out
        .warnings
        .iter()
        .find(|w| w.starts_with("DECLINED_OVERRIDE"))
        .expect("the honest note");
    assert!(line.contains("declined on the web"), "{line}");
}

// =================================================================================================
// The project scope: nearest wins whole, and the two scopes never blend.
// =================================================================================================

#[test]
fn a_project_manifest_lands_in_the_checkout_self_ignoring() {
    let rig = Rig::new("project");
    rig.seed_session();
    let proj = project(
        "proj",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    // The feed delivers NOTHING — the demand is the project file's.
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);

    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::FastForwarded, "{:?}", out.warnings);
    assert_eq!(row.scope.as_deref(), Some(&*proj.0.display().to_string()));
    // The bytes live INSIDE the checkout, not the home-scope dirs.
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.join("SKILL.md").exists());
    assert!(!rig.work.0.join("skills/deploy").exists());
    // The placed dir SELF-IGNORES (the node_modules model), and NOTHING under `.git/` was written.
    assert_eq!(
        std::fs::read(placed.join(".gitignore")).unwrap(),
        crate::scan::IGNORE_SENTINEL
    );
    assert!(
        std::fs::read_dir(proj.0.join(".git"))
            .unwrap()
            .next()
            .is_none(),
        "nothing under .git/ was written"
    );
    // The engine state lives in the PROJECT's own store, and the store ignores itself whole.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(!rig.layout().skill_dir(&sid).exists());
    let playout =
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).expect("the project store");
    assert!(playout.skill_dir(&sid).exists());
    assert_eq!(
        std::fs::read(proj.0.join(".topos/.gitignore")).unwrap(),
        b"*\n"
    );
    // A second sweep is a clean no-op: the sentinel never reads as an edit.
    let out2 = sweep(&ctx, &plane, &dir);
    assert!(out2.warnings.is_empty(), "{:?}", out2.warnings);
    let row2 = out2
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row2.action, PullAction::UpToDate, "{:?}", out2.data.skills);
}

#[test]
fn the_nearest_project_file_governs_whole() {
    let rig = Rig::new("nearest");
    rig.seed_session();
    let repo = project(
        "proj-outer",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/repo-wide\" = \"*\"\n"),
    );
    let nested = repo.0.join("services/api");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join(crate::manifest::MANIFEST_FILE),
        format!("[bundles]\n\"{HOST}/{WS_NAME}/api-only\" = \"*\"\n"),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let wide = one_file(b"# repo-wide\n");
    let api = one_file(b"# api-only\n");
    let plane = FakePlane::new(log)
        .with_version("s_wide", &wide)
        .with_version("s_api", &api);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_wide", "repo-wide", &wide),
            catalog_entry("s_api", "api-only", &api),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&nested));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        nested.join(".claude/skills/api-only/SKILL.md").exists(),
        "the nearest file governs"
    );
    assert!(
        !repo.0.join(".claude/skills/repo-wide").exists(),
        "the ancestor's rows never blend in from below"
    );
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "repo-wide"),
        "{:?}",
        out.data.skills
    );
}

#[test]
fn the_same_bundle_at_both_scopes_lands_twice() {
    let rig = Rig::new("unblended");
    rig.seed_session();
    let proj = project(
        "proj-two",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    // The feed delivers it too — no shadowing: each scope takes what its own recipe says.
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let person_copy = rig.work.0.join("skills/deploy");
    let project_copy = proj.0.join(".claude/skills/deploy");
    assert!(person_copy.join("SKILL.md").exists(), "the feed's copy");
    assert!(project_copy.join("SKILL.md").exists(), "the project's copy");
    // TWO rows, one per scope label.
    let mut scopes: Vec<&str> = out
        .data
        .skills
        .iter()
        .filter(|s| s.skill == "deploy")
        .filter_map(|s| s.scope.as_deref())
        .collect();
    scopes.sort_unstable();
    let proj_label = proj.0.display().to_string();
    let mut want = vec!["person", proj_label.as_str()];
    want.sort_unstable();
    assert_eq!(scopes, want, "{:?}", out.data.skills);
    // TWO state trees, each recording only its own scope's placements.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(rig.layout().skill_dir(&sid).exists());
    assert!(playout.skill_dir(&sid).exists());
    let home_map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert!(
        home_map
            .placements
            .iter()
            .all(|p| !std::path::Path::new(p).starts_with(&proj.0)),
        "{:?}",
        home_map.placements
    );
    let proj_map = crate::doc::read_map(&rig.fs, &playout.published(&sid).map)
        .unwrap()
        .unwrap();
    assert!(
        proj_map
            .placements
            .iter()
            .all(|p| std::path::Path::new(p).starts_with(&proj.0)),
        "{:?}",
        proj_map.placements
    );

    // A draft in the PROJECT copy stays that scope's business.
    std::fs::write(project_copy.join("SKILL.md"), b"# project edit\n").unwrap();
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read(project_copy.join("SKILL.md")).unwrap(),
        b"# project edit\n"
    );
    assert_eq!(
        std::fs::read(person_copy.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the person copy never sees the project draft"
    );
}

#[test]
fn a_channel_expands_and_an_explicit_row_of_the_same_identity_wins() {
    let rig = Rig::new("channel");
    let old = one_file(b"# v1\n");
    let new = one_file(b"# v2\n");
    let other = one_file(b"# other\n");
    rig.seed_session();
    let proj = project(
        "proj-ch",
        &format!(
            "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n\
             \"{HOST}/{WS_NAME}/deploy\" = \"{}\"\n",
            topos_core::digest::to_hex(&old.id)
        ),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &old)
        .with_version("s_deploy", &new)
        .with_version("s_other", &other);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &new),
            catalog_entry("s_other", "other", &other),
        ],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![
                WireChannelSkill {
                    skill_id: "s_deploy".into(),
                    name: "deploy".into(),
                },
                WireChannelSkill {
                    skill_id: "s_other".into(),
                    name: "other".into(),
                },
            ],
        }],
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // The channel delivered its other member …
    assert!(proj.0.join(".claude/skills/other/SKILL.md").exists());
    // … and the explicit row's PIN decided the shared identity, not the channel's current.
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v1\n"
    );
    assert_eq!(
        out.data
            .skills
            .iter()
            .filter(|s| s.skill == "deploy")
            .count(),
        1
    );
}

#[test]
fn a_workspace_row_without_a_session_is_an_honest_local_line() {
    let rig = Rig::new("nosession");
    // NO session at all — the file references a workspace this install never logged into.
    let proj = project(
        "proj-ns",
        "[bundles]\n\"elsewhere.dev/ops/deploy\" = \"*\"\n",
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    let w = out
        .warnings
        .iter()
        .find(|w| w.starts_with("NOT_AVAILABLE"))
        .expect("the honest line");
    assert!(w.contains("topos login elsewhere.dev/ops"), "{w}");
}

#[test]
fn an_unparsable_manifest_freezes_its_scope() {
    let rig = Rig::new("badfile");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.exists());

    // A typo the grammar refuses: the scope delivers nothing AND cleans nothing — the failure mode
    // of a mistake must be keeping bytes.
    rig.write_global("[bundles]\n\"not a reference\" = \"*\"\n");
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("MANIFEST_INVALID")),
        "{:?}",
        out.warnings
    );
    assert!(placed.exists(), "a frozen scope never cleans");
}

// =================================================================================================
// The forge arms.
// =================================================================================================

/// Build a real `.tar.gz` with a `TOP/` prefix over `(repo-relative path, bytes)` entries.
fn build_repo_targz(top: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
    let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut tar = tar::Builder::new(gz);
    for (name, bytes) in entries {
        let mut h = tar::Header::new_ustar();
        h.set_entry_type(tar::EntryType::Regular);
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(0);
        tar.append_data(&mut h, format!("{top}/{name}"), *bytes)
            .unwrap();
    }
    tar.into_inner().unwrap().finish().unwrap()
}

/// The forge fake: ONE archive at a time, and a fetch counter (the "never dialed" witness).
#[derive(Clone)]
struct FakeGit {
    archive: Arc<Mutex<Vec<u8>>>,
    fetches: Arc<Mutex<u32>>,
}
impl FakeGit {
    fn new(targz: Vec<u8>) -> Self {
        Self {
            archive: Arc::new(Mutex::new(targz)),
            fetches: Arc::new(Mutex::new(0)),
        }
    }
    fn serve(&self, targz: Vec<u8>) {
        *self.archive.lock().unwrap() = targz;
    }
    fn fetches(&self) -> u32 {
        *self.fetches.lock().unwrap()
    }
}
impl crate::git_source::GitTarballSource for FakeGit {
    fn fetch(&self, _spec: &crate::source::RemoteSpec) -> Result<Vec<u8>, ClientError> {
        *self.fetches.lock().unwrap() += 1;
        Ok(self.archive.lock().unwrap().clone())
    }
}

#[test]
fn a_star_repo_row_moves_only_on_an_explicit_update() {
    let rig = Rig::new("repo");
    // No session: a pure forge recipe.
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v1\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
        ],
    ));

    // The first EXPLICIT update installs every skill the repo holds.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let alpha = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    let beta = rig.home.0.join(".claude/skills/beta/SKILL.md");
    assert!(alpha.exists() && beta.exists(), "{:?}", out.data.skills);
    let after_install = git.fetches();
    assert_eq!(after_install, 1, "one fetch per repo per sweep");

    // The BACKGROUND sweep passes no forge lane: tracked members converge in place, and the forge
    // is never dialed — a session start must never depend on github.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v2\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
            ("skills/gamma/SKILL.md", b"# gamma v1\n"),
        ],
    ));
    let quiet = sweep(&ctx, &plane, &dir);
    assert_eq!(git.fetches(), after_install, "the quiet sweep never dials");
    assert_eq!(
        std::fs::read_to_string(&alpha).unwrap(),
        "# alpha v1\n",
        "no forge lane, no move"
    );
    assert!(
        quiet
            .data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?}",
        quiet.data.skills
    );

    // The EXPLICIT update moves it — and says exactly what moved.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = out
        .warnings
        .iter()
        .find(|w| w.starts_with("GIT_UPDATED"))
        .unwrap_or_else(|| panic!("the moved-source line: {:?}", out.warnings));
    assert!(line.contains("github.com/o/r"), "{line}");
    assert!(
        line.contains("aaaaaaaaaaaa"),
        "names the old commit: {line}"
    );
    assert!(
        line.contains("bbbbbbbbbbbb"),
        "names the new commit: {line}"
    );
    assert!(line.contains("+gamma"), "names the new member: {line}");
    assert!(line.contains("~alpha"), "names the carried member: {line}");
    assert_eq!(
        std::fs::read_to_string(&alpha).unwrap(),
        "# alpha v2\n",
        "the explicit update lands the new bytes"
    );
    assert!(rig.home.0.join(".claude/skills/gamma/SKILL.md").exists());
}

#[test]
fn an_untracked_repo_row_says_it_needs_the_network() {
    let rig = Rig::new("repo-offline");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("NOT_INSTALLED") && w.contains("network required")),
        "{:?}",
        out.warnings
    );
}

// =================================================================================================
// Cleaning.
// =================================================================================================

#[test]
fn a_dropped_feed_row_cleans_the_person_placements_snapshot_first() {
    let rig = Rig::new("feeddrop");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.join("SKILL.md").exists());
    let before = store_versions(&rig.layout(), "s_deploy");

    // An edit rides along, then the feed row goes: the edit is ABSORBED before the dir is removed.
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    rig.write_global("[bundles]\n");
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "deploy" && s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );
    assert!(!placed.exists(), "the person-scope copy is retired");
    assert!(
        store_versions(&rig.layout(), "s_deploy") > before,
        "the edit was snapshotted into the store first"
    );
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "every sidecar byte stays"
    );

    // Idempotent: the sweep after has nothing left to retire.
    let out2 = sweep(&ctx, &plane, &dir);
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

#[test]
fn a_new_off_row_cleans_its_bundles_placements_and_keeps_the_bytes() {
    let rig = Rig::new("offclean");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# noisy\n");
    let plane = FakePlane::new(log).with_version("s_noisy", &v);
    plane.serves(vec![delivered("s_noisy", "noisy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_noisy", "noisy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/noisy");
    assert!(placed.exists());

    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/noisy\" = \"off\"\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    assert!(!placed.exists(), "the switch retires the copy");
    let sid = crate::id::SkillId::parse("s_noisy").unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "an off switch keeps the bytes: {:?}",
        out.warnings
    );
}

#[test]
fn a_dropped_project_row_cleans_inside_the_checkout() {
    let rig = Rig::new("projdrop");
    rig.seed_session();
    let proj = project(
        "proj-drop",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.exists());

    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !placed.exists(),
        "the dropped row retires the in-checkout copy: {:?}",
        out.warnings
    );
    // The project's own store still holds the bundle's bytes.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(playout.skill_dir(&sid).exists());
}

#[test]
fn an_offline_sweep_freezes_and_never_cleans() {
    let rig = Rig::new("offline");
    rig.seed_session();
    let proj = project(
        "proj-off",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
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
    sweep(&ctx, &plane, &dir);
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.exists());

    // The whole plane is down (the session-start hook's world): NOTHING may be deleted.
    plane.serve_unreachable();
    dir.set_unavailable(true);
    let out = sweep(&ctx, &plane, &dir);
    assert!(placed.exists(), "an offline sweep freezes, never cleans");
    assert!(
        !out.data
            .skills
            .iter()
            .any(|s| s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );

    // Back online with the index still failing: the unknowable member set stays frozen.
    plane.serve(empty_snapshot());
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        placed.exists(),
        "a transient index failure freezes the members: {:?}",
        out.warnings
    );

    // Fully back: the member is still delivered.
    dir.set_unavailable(false);
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(placed.exists());
}

// =================================================================================================
// `--rebuild`.
// =================================================================================================

#[test]
fn rebuild_absorbs_the_edit_before_it_re_projects() {
    let rig = Rig::new("rebuild");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.join("SKILL.md").exists());
    let before = store_versions(&rig.layout(), "s_deploy");

    // A local edit, plus a stray file the served version never had.
    std::fs::write(placed.join("SKILL.md"), b"# hand-edited\n").unwrap();
    std::fs::write(placed.join("stray.md"), b"junk\n").unwrap();

    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            rebuild: true,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // ABSORBED first: the edit is in the store, so nothing was lost to the repair.
    assert!(
        store_versions(&rig.layout(), "s_deploy") > before,
        "the edit was snapshotted before the dir was dropped"
    );
    // Then RE-PROJECTED, pristine: the served bytes, and no stray file.
    assert_eq!(
        snapshot_dir(&placed),
        vec![("SKILL.md".to_owned(), b"# deploy\n".to_vec())],
        "the copy is re-materialized from the store"
    );
}

// =================================================================================================
// The session-level facts: the ended freeze, and the quiet hook's staleness line.
// =================================================================================================

#[test]
fn an_ended_session_freezes_and_prints_once() {
    let rig = Rig::new("ended");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serve_not_found();
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.warnings.iter().any(|w| w.starts_with("SESSION_ENDED")),
        "{:?}",
        out.warnings
    );
    assert_eq!(out.access_gone, vec![WS_NAME.to_owned()]);
    let all = sessions::read_sessions(&rig.fs, &rig.layout()).unwrap();
    assert_eq!(all.sessions[0].status, SESSION_ENDED);
    // The second run skips the ended session — the line printed once.
    let out2 = sweep(&ctx, &plane, &dir);
    assert!(out2.warnings.is_empty(), "{:?}", out2.warnings);
}

/// The rig's fixed wall clock, as the millis the freshness cache stamps.
fn rig_now(rig: &Rig) -> i64 {
    i64::try_from(rig.clock.0).expect("the rig clock fits an i64")
}

const DAY_MS: i64 = 86_400_000;

#[test]
fn an_unreachable_and_stale_workspace_warns_by_name() {
    let rig = Rig::new("stalewarn");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // One good sweep stamps the freshness row — under the workspace ID, with the served window.
    sweep(&ctx, &plane, &dir);
    let status = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    assert_eq!(status.workspaces[WS].last_delivery_at, Some(rig_now(&rig)));
    assert_eq!(status.workspaces[WS].staleness_window_ms, 604_800_000);
    assert!(
        !status.workspaces.contains_key(WS_NAME),
        "the freshness cache is keyed by id, never by name — the warning must look it up that way"
    );

    // Now the server is gone.
    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert_eq!(out.unreachable[0].workspace_id, WS);
    assert_eq!(out.unreachable[0].workspace_name, WS_NAME);

    // Past the recorded 7-day window: the ONE line, naming the workspace a person knows.
    let stale_now = rig_now(&rig) + 8 * DAY_MS;
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out);
    assert_eq!(
        lines,
        vec![format!(
            "topos: {WS_NAME} last synced 8d ago — the server could not be reached"
        )]
    );

    // INSIDE the window: silence — a transient blip must not spam every session start.
    let fresh_now = rig_now(&rig) + 3_600_000;
    assert!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), fresh_now, &out).is_empty(),
        "a fresh miss stays quiet"
    );
}

#[test]
fn an_answering_server_never_gets_blamed_on_the_network() {
    let rig = Rig::new("stalekind");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // One good sweep stamps the freshness row the staleness check reads.
    sweep(&ctx, &plane, &dir);
    let stale_now = rig_now(&rig) + 8 * DAY_MS;

    // The plane ANSWERED with a failure status. The nudge is just as true — but the network is fine.
    plane.serve_unavailable();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("PLANE_UNAVAILABLE")),
        "{:?}",
        out.warnings
    );
    let unavailable_line =
        format!("topos: {WS_NAME} last synced 8d ago — the server did not answer successfully");
    assert_eq!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out),
        vec![unavailable_line.clone()]
    );

    // The OTHER half of the same variant: the answer got cut off part-way.
    plane.serve_truncated();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out),
        vec![unavailable_line],
        "a truncated body is the same variant and reads the same"
    );

    // The plane ANSWERED unreadably. Pointing a person at their network here sends them the wrong
    // way entirely — the signal is about the bytes.
    plane.serve_malformed();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.warnings.iter().any(|w| w.starts_with("WIRE_INVALID")),
        "the MALFORMED arm ran, not the transport one: {:?}",
        out.warnings
    );
    assert_eq!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out),
        vec![format!(
            "topos: {WS_NAME} last synced 8d ago — the server's answer could not be read"
        )]
    );

    // All three still stay quiet inside the window — the reason never overrides the threshold.
    let fresh_now = rig_now(&rig) + 3_600_000;
    assert!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), fresh_now, &out).is_empty(),
        "a fresh miss stays quiet whatever the reason"
    );
}

#[test]
fn a_never_delivered_workspace_stays_silent_while_unreachable() {
    let rig = Rig::new("stalenever");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serve_unreachable();
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Unreachable from the very first sweep: nothing was ever delivered, so there is no freshness
    // row — and nothing to be stale FROM.
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), i64::MAX, &out).is_empty(),
        "no record, no warning"
    );
}

#[test]
fn a_zero_staleness_window_never_warns() {
    let rig = Rig::new("stalezero");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serve(DeliverySnapshot {
        staleness_window_ms: 0,
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    assert_eq!(
        sync_status::read(&rig.fs, &rig.layout())
            .unwrap()
            .workspaces[WS]
            .staleness_window_ms,
        0
    );

    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), i64::MAX, &out).is_empty(),
        "a zero window opts the workspace out of the warning entirely"
    );
}

// =================================================================================================
// Targeted updates.
// =================================================================================================

#[test]
fn a_targeted_update_narrows_the_sweep_and_names_a_miss() {
    let rig = Rig::new("targeted");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let deploy = one_file(b"# deploy\n");
    let other = one_file(b"# other\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &deploy)
        .with_version("s_other", &other);
    plane.serves(vec![
        delivered("s_deploy", "deploy", &deploy),
        delivered("s_other", "other", &other),
    ]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &deploy),
            catalog_entry("s_other", "other", &other),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["deploy".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(rig.work.0.join("skills/deploy/SKILL.md").exists());
    assert!(
        !rig.work.0.join("skills/other").exists(),
        "a targeted update touches only what was named"
    );

    // A target nothing answers refuses typed, naming the way back.
    let refused = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["nonesuch".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    );
    let Err(err) = refused else {
        panic!("an unmatched target must refuse");
    };
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err}");
    assert!(err.to_string().contains("connected feeds"), "{err}");
}
