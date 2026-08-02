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
use crate::fs_seam::{FsOps, RealFs};
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
        // CANONICAL, deliberately: macOS's `$TMPDIR` lives behind the `/var` -> `/private/var`
        // symlink, so a raw temp path and the path the engine RECORDS (which resolves it) differ
        // in spelling — and every reconcile rule that compares a recorded placement against its
        // project dir would silently never match here. A rig that cannot match is a rig that
        // proves nothing; the canonical spelling is also what an ordinary checkout has.
        let dir = dir.canonicalize().unwrap_or(dir);
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
    /// The LAST applied report's rows, `(skill_id, version hex)` — the wire carries exactly one
    /// row per (session, bundle), so this is what the cross-store pick actually reported.
    reported: Arc<Mutex<Vec<(String, String)>>>,
}
impl FakePlane {
    fn new(log: CallLog) -> Self {
        Self {
            delivery: Arc::new(Mutex::new(Ok(empty_snapshot()))),
            versions: HashMap::new(),
            log,
            reported: Arc::new(Mutex::new(Vec::new())),
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
        *self.reported.lock().unwrap() = applied
            .iter()
            .map(|(id, v)| (id.clone(), topos_core::digest::to_hex(v)))
            .collect();
        let mut ids: Vec<&str> = applied.iter().map(|(id, _)| id.as_str()).collect();
        ids.sort_unstable();
        self.log
            .lock()
            .unwrap()
            .push(format!("report {}", ids.join(",")));
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
        // A publish's best-effort post-landing read: absent here (the invite line just omits).
        Err(ClientError::Plane("no me in this fake".into()))
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
    assert!(log.lock().unwrap().iter().any(|l| l == "report s_deploy"));
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

/// An exchange that lands EMPTY says so. Without the line the receipt names only what moved, so a
/// person who was just told the feed would be applied reads the silence as a failed apply.
#[test]
fn an_exchange_that_serves_nothing_says_so_without_counting_as_a_failure() {
    let rig = Rig::new("emptyserve");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    let line = out
        .disclosures
        .iter()
        .find(|d| d.starts_with("NOTHING_ASSIGNED"))
        .unwrap_or_else(|| panic!("the empty-serve line: {:?}", out.disclosures));
    assert!(line.contains(&format!("{HOST}/{WS_NAME}")), "{line}");
    assert!(line.contains("nothing assigned to you yet"), "{line}");
    // A DISCLOSURE: the exchange worked, so nothing here may read as broken.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(out.data.skills.is_empty(), "{:?}", out.data.skills);
}

/// The line is the WORKSPACE's fact, said ONCE however many times a recipe adopts the address.
/// The implicit person recipe holds one feed row per live SESSION, so a workspace whose id was
/// re-minted (the old session row surviving beside the new one) adopts the same address twice and
/// drives the feed reconcile twice — the receipt must still carry exactly one line.
#[test]
fn one_address_adopted_twice_earns_one_empty_exchange_line() {
    let rig = Rig::new("emptytwice");
    rig.seed_session();
    sessions::upsert_session(
        &rig.fs,
        &rig.layout(),
        Session {
            host: HOST.into(),
            base_url: format!("https://{HOST}/api"),
            // A DIFFERENT opaque id for the SAME address — the session file keys on (host, id),
            // so both rows live, and both name `acme.test/eng`.
            workspace_id: "w_eng_reminted".into(),
            workspace_name: WS_NAME.into(),
            display_name: "Engineering".into(),
            session_id: "sn_2".into(),
            credential: "cred-2".into(),
            status: SESSION_ACTIVE.into(),
            logged_in_at: 2,
        },
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert_eq!(
        out.disclosures
            .iter()
            .filter(|d| d.starts_with("NOTHING_ASSIGNED"))
            .count(),
        1,
        "{:?}",
        out.disclosures
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
}

/// Bundles that ARRIVED and were skipped here are a local choice, not an empty workspace — the
/// line must not fire on an `"off"` row's work.
#[test]
fn a_served_feed_never_earns_the_empty_exchange_line() {
    let rig = Rig::new("servedfeed");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let noisy = one_file(b"# noisy\n");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/noisy\" = \"off\"\n"
    ));
    let plane = FakePlane::new(log).with_version("s_noisy", &noisy);
    plane.serves(vec![delivered("s_noisy", "noisy", &noisy)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_noisy", "noisy", &noisy)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert!(
        !rig.work.0.join("skills/noisy").exists(),
        "the switch withholds it"
    );
    assert!(
        !out.disclosures
            .iter()
            .any(|d| d.starts_with("NOTHING_ASSIGNED")),
        "the workspace assigned something: {:?}",
        out.disclosures
    );
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

/// Pass the first-trust gate for a forge reference — the consented `topos add … --yes` an
/// untracked origin's refusal names.
fn gate_add(ctx: &Ctx<'_>, plane: &FakePlane, dir: &FakeDirectory, git: &FakeGit, raw: &str) {
    match ops::add_reference(
        ctx,
        &connect(plane, dir),
        Some(git as &dyn crate::git_source::GitTarballSource),
        raw,
        true,
        true,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
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

    // An UNTRACKED origin never first-installs from `update` — however explicit the run: the row
    // is demand (a repo fact anyone could have committed), never consent. The refusal names the
    // gate.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let w = out
        .warnings
        .iter()
        .find(|w| w.starts_with("FIRST_TRUST"))
        .expect("the gate line");
    assert!(w.contains("topos add -g github.com/o/r --yes"), "{w}");
    assert_eq!(git.fetches(), 0, "nothing is fetched before the gate");
    assert!(out.data.skills.is_empty(), "{:?}", out.data.skills);

    // THROUGH the gate: the consented add installs every skill the repo holds.
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    let alpha = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    let beta = rig.home.0.join(".claude/skills/beta/SKILL.md");
    assert!(alpha.exists() && beta.exists());
    let after_install = git.fetches();

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
        .disclosures
        .iter()
        .find(|w| w.starts_with("GIT_UPDATED"))
        .unwrap_or_else(|| panic!("the moved-source line: {:?}", out.disclosures));
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
fn an_untracked_repo_row_refuses_toward_the_add_gate() {
    // The refusal is the SAME on the quiet sweep (no forge lane) and on a targeted skill row —
    // a network would not change it: trust is the gate, not reachability.
    let rig = Rig::new("repo-gate");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n\"github.com/x/y/tool\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    let lines: Vec<&String> = out
        .warnings
        .iter()
        .filter(|w| w.starts_with("FIRST_TRUST"))
        .collect();
    assert_eq!(lines.len(), 2, "{:?}", out.warnings);
    assert!(
        lines
            .iter()
            .any(|w| w.contains("topos add -g github.com/o/r --yes")),
        "{lines:?}"
    );
    // A four-segment SKILL row names ITS OWN reference as the gate (adding it is what makes the
    // origin tracked).
    assert!(
        lines
            .iter()
            .any(|w| w.contains("topos add -g github.com/x/y/tool --yes")),
        "{lines:?}"
    );
    assert!(
        !out.warnings.iter().any(|w| w.contains("network required")),
        "trust, not reachability: {:?}",
        out.warnings
    );
}

#[test]
fn a_committed_project_store_never_grants_forge_trust() {
    // RED TEAM: a malicious checkout commits a valid-looking `.topos/` project store (real
    // lock.json + origin.json — here minted by a real add on ANOTHER machine) plus the manifest
    // row. On a machine whose OWN registry never granted the origin, the reconcile must refuse
    // toward the add gate and dial nothing — store contents are a checkout fact, never consent.
    let victim = Rig::new("redteam-victim");
    let attacker = Rig::new("redteam-attacker");
    let proj = project("proj-redteam", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // The attacker's machine populates the checkout's own store through a REAL consented add —
    // exactly the bytes a hostile repo could commit.
    let attacker_ctx = attacker.ctx_at(Some(&proj.0));
    match ops::add_reference(
        &attacker_ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        true,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(
        crate::sidecar::existing_project_store(&victim.fs, &proj.0).is_some(),
        "the checkout carries a real project store"
    );
    let fetches_before = git.fetches();

    // The VICTIM machine opens the checkout: its registry holds nothing — the store must not
    // vouch, however valid its documents look.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha EVIL\n")],
    ));
    let victim_ctx = victim.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &victim_ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let w = out
        .warnings
        .iter()
        .find(|w| w.starts_with("FIRST_TRUST"))
        .unwrap_or_else(|| panic!("the gate refuses: {:?}", out.warnings));
    assert!(w.contains("topos add github.com/o/r --yes"), "{w}");
    assert_eq!(
        git.fetches(),
        fetches_before,
        "nothing is fetched on the untrusting machine"
    );
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v1\n",
        "no forge content installs without this machine's own consent"
    );
}

#[test]
fn a_granted_origin_flows_in_both_scopes() {
    // The add's consent moment records the origin in the MACHINE registry; a later project-scope
    // row of the same origin then flows on explicit update — no second ceremony, no refusal —
    // installing into the checkout's own store.
    let rig = Rig::new("trust-both-scopes");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    // The consent is durably in the registry.
    let trust: crate::forge_trust::ForgeTrust =
        crate::doc::read_doc(&rig.fs, &rig.layout().forge_trust_path())
            .unwrap()
            .expect("the registry doc");
    assert!(trust.origins.contains("github.com/o/r"), "{trust:?}");

    // A fresh checkout spells the same repo row: the explicit update installs into the PROJECT's
    // own store with no first-trust refusal (the grant is machine-wide).
    let proj = project("proj-granted", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let pctx = rig.ctx_at(Some(&proj.0));
    let out = ops::manifest_update(
        &pctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        !out.warnings.iter().any(|w| w.starts_with("FIRST_TRUST")),
        "{:?}",
        out.warnings
    );
    assert!(
        proj.0.join(".claude/skills/alpha/SKILL.md").exists(),
        "the granted origin's member lands in the checkout: {:?}",
        out.warnings
    );
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).is_some(),
        "installed through the project's own store"
    );
}

#[test]
fn home_store_imports_seed_the_registry_once() {
    // Legacy machines: origins already imported in the HOME store passed the add gate
    // historically — the first consult seeds them into the registry, once; afterwards the
    // registry alone is the authority (the store evidence may go, the grant stands).
    let rig = Rig::new("trust-seed");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    // A pre-registry machine: real home-store imports, NO registry doc.
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    std::fs::remove_file(rig.layout().forge_trust_path()).unwrap();

    // The consult seeds: the update flows with no first-trust refusal, and the written registry
    // carries the origin with the durable seed marker.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        !out.warnings.iter().any(|w| w.starts_with("FIRST_TRUST")),
        "{:?}",
        out.warnings
    );
    let trust: crate::forge_trust::ForgeTrust =
        crate::doc::read_doc(&rig.fs, &rig.layout().forge_trust_path())
            .unwrap()
            .expect("the seeded registry doc");
    assert!(trust.seeded, "{trust:?}");
    assert!(trust.origins.contains("github.com/o/r"), "{trust:?}");

    // ONCE: the registry now speaks alone — with the home-store evidence gone, the origin stays
    // granted and the explicit update converges the landing afresh.
    for entry in rig.fs.read_dir(&rig.layout().skills_dir()).unwrap() {
        std::fs::remove_dir_all(&entry).unwrap();
    }
    std::fs::remove_dir_all(rig.home.0.join(".claude/skills/alpha")).unwrap();
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        !out.warnings.iter().any(|w| w.starts_with("FIRST_TRUST")),
        "the registry is the authority once seeded: {:?}",
        out.warnings
    );
    assert!(
        rig.home.0.join(".claude/skills/alpha/SKILL.md").exists(),
        "the trusted row converges its landing: {:?}",
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

// =================================================================================================
// Per-scope forge stores, the absent-member clean, and the dropped-row clean.
// =================================================================================================

#[test]
fn a_repo_row_flows_after_the_add_gate_and_the_quiet_sweep_still_never_dials() {
    let rig = Rig::new("repo-postgate");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());

    // Tracked now: the quiet sweep converges in place without dialing; the explicit update flows.
    let fetches = git.fetches();
    let quiet = sweep(&ctx, &plane, &dir);
    assert_eq!(git.fetches(), fetches, "quiet never dials");
    assert!(
        quiet
            .data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?}",
        quiet.data.skills
    );
    assert!(
        !quiet.warnings.iter().any(|w| w.starts_with("FIRST_TRUST")),
        "{:?}",
        quiet.warnings
    );
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?}",
        out.data.skills
    );
}

#[test]
fn project_checkouts_keep_their_own_forge_stores() {
    // TWO checkouts of one repo row: each gets its OWN tracked import + placement, and a move
    // taken in one (a pin move / a new commit) never reaches into the other's placements.
    let rig = Rig::new("repo-two-proj");
    let proj_a = project("proj-fa", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let proj_b = project("proj-fb", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // Gate each checkout separately — trust is per store.
    let ctx_a = rig.ctx_at(Some(&proj_a.0));
    match ops::add_reference(
        &ctx_a,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        true,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    let a_copy = proj_a.0.join(".claude/skills/alpha/SKILL.md");
    assert!(a_copy.exists(), "checkout A holds its own placement");
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj_a.0).is_some(),
        "checkout A has its own store"
    );

    let ctx_b = rig.ctx_at(Some(&proj_b.0));
    match ops::add_reference(
        &ctx_b,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        true,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    let b_copy = proj_b.0.join(".claude/skills/alpha/SKILL.md");
    assert!(b_copy.exists(), "checkout B holds its own placement");

    // The source moves; ONLY checkout B updates. A's placement must be untouched — the refresh
    // operates within B's store alone (never a stash-and-delete of A's copy).
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    let out = ops::manifest_update(
        &ctx_b,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&b_copy).unwrap(),
        "# alpha v2\n",
        "{:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read_to_string(&a_copy).unwrap(),
        "# alpha v1\n",
        "checkout A's placement never moves from a run in checkout B"
    );
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj_a.0)
            .map(|l| rig.fs.read_dir(&l.skills_dir()).unwrap().len())
            .unwrap_or(0)
            > 0,
        "checkout A's store is intact"
    );
}

#[test]
fn a_member_gone_from_the_archive_is_cleaned_snapshot_first() {
    let rig = Rig::new("repo-minus");
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
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    let beta = rig.home.0.join(".claude/skills/beta");
    assert!(beta.join("SKILL.md").exists());
    // An edit rides on the leaving member — it must be absorbed before the dir goes.
    std::fs::write(beta.join("SKILL.md"), b"# my beta edit\n").unwrap();
    let beta_sid = {
        // The tracked import's id, for the snapshot-first witness below.
        let mut hit = None;
        for entry in rig.fs.read_dir(&rig.layout().skills_dir()).unwrap() {
            let id = entry.file_name().unwrap().to_str().unwrap().to_owned();
            let sid = crate::id::SkillId::parse(&id).unwrap();
            let lock: topos_types::persisted::Lock =
                crate::doc::read_doc(&rig.fs, &rig.layout().published(&sid).lock)
                    .unwrap()
                    .unwrap();
            if lock.name == "beta" {
                hit = Some(id);
            }
        }
        hit.expect("beta tracked")
    };
    let before = store_versions(&rig.layout(), &beta_sid);

    // The new archive no longer holds beta: the explicit update renders `-beta` AND retires the
    // copy, snapshot-first; the sidecar bytes stay.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = out
        .disclosures
        .iter()
        .find(|w| w.starts_with("GIT_UPDATED"))
        .expect("the moved-source line");
    assert!(line.contains("-beta"), "{line}");
    assert!(!beta.exists(), "the absent member's copy is retired");
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "beta" && s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );
    assert!(
        store_versions(&rig.layout(), &beta_sid) > before,
        "the edit was snapshotted into the store first"
    );
    let sid = crate::id::SkillId::parse(&beta_sid).unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "the sidecar bytes stay"
    );
}

#[test]
fn a_dropped_repo_row_cleans_its_members_like_any_undemanded_item() {
    let rig = Rig::new("repo-drop");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    let alpha = rig.home.0.join(".claude/skills/alpha");
    assert!(alpha.exists());

    // The row goes; the next BARE sweep (no forge lane needed) retires the members' placements
    // and keeps every sidecar byte.
    rig.write_global("[bundles]\n");
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !alpha.exists(),
        "a dropped repo row's member is undemanded: {:?}",
        out.warnings
    );
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );
    // Idempotent: nothing left to retire on the sweep after.
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

// =================================================================================================
// Per-scope mentions (the person clean is never shielded by a project mention).
// =================================================================================================

#[test]
fn a_project_mention_never_shields_a_person_scope_clean() {
    let rig = Rig::new("scope-mention");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());

    // A project whose manifest mentions an UNRELATED thing of the same NAME (a local folder
    // called `deploy`).
    let proj = project("proj-mention", "[bundles]\n\"./deploy\" = \"*\"\n");
    std::fs::create_dir_all(proj.0.join("deploy")).unwrap();
    std::fs::write(proj.0.join("deploy/SKILL.md"), b"# local deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.exists(), "the person feed installed its copy");

    // The feed withdraws the bundle. The person-scope copy must retire — the PROJECT's mention
    // of the same name is a different scope's business and shields nothing.
    plane.serves(Vec::new());
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !placed.exists(),
        "the person copy retired despite the project mention: {:?}",
        out.warnings
    );
    assert!(
        proj.0.join("deploy/SKILL.md").exists(),
        "the project's own folder is untouched"
    );
}

// =================================================================================================
// The applied report is complete-state: project stores and manifest deliveries included.
// =================================================================================================

#[test]
fn the_report_covers_project_manifest_deliveries() {
    let rig = Rig::new("report-proj");
    rig.seed_session();
    let proj = project(
        "proj-report",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    // The FEED delivers nothing — the demand is the project manifest row alone.
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        proj.0.join(".claude/skills/deploy/SKILL.md").exists(),
        "{:?}",
        out.warnings
    );
    // The applied report names the PROJECT-delivered bundle — held in the project's own store,
    // outside the feed — so the server's fleet state is never falsely empty.
    assert!(
        log.lock().unwrap().iter().any(|l| l == "report s_deploy"),
        "{:?}",
        log.lock().unwrap()
    );
}

#[test]
fn the_report_carries_another_checkouts_holdings() {
    // COMPLETE-state across checkouts: checkout A's project delivery keeps riding the applied
    // report when the update runs from checkout B (the visited-stores index) — and deleting A's
    // store drops it naturally on the next read.
    let rig = Rig::new("report-cross");
    rig.seed_session();
    let proj_a = project(
        "proj-cross-a",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let proj_b = project("proj-cross-b", "[bundles]\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    // The FEED delivers nothing — the demand is checkout A's manifest row alone.
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());

    // Checkout A delivers + reports its project bundle.
    let ctx_a = rig.ctx_at(Some(&proj_a.0));
    sweep(&ctx_a, &plane, &dir);
    assert!(proj_a.0.join(".claude/skills/deploy/SKILL.md").exists());
    let reports = |want: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|l| l.as_str() == want)
            .count()
    };
    assert_eq!(reports("report s_deploy"), 1, "{:?}", log.lock().unwrap());

    // The update runs from checkout B: A's holdings still ride the complete-state report — an
    // omission would make the server delete A's fleet rows.
    let ctx_b = rig.ctx_at(Some(&proj_b.0));
    sweep(&ctx_b, &plane, &dir);
    assert_eq!(
        reports("report s_deploy"),
        2,
        "checkout A's holdings ride the report from checkout B: {:?}",
        log.lock().unwrap()
    );

    // A's store goes (the checkout was deleted): the holding drops out of the report naturally.
    std::fs::remove_dir_all(proj_a.0.join(".topos")).unwrap();
    sweep(&ctx_b, &plane, &dir);
    assert_eq!(
        reports("report s_deploy"),
        2,
        "a deleted store's holdings leave the report: {:?}",
        log.lock().unwrap()
    );
    assert_eq!(
        reports("report "),
        1,
        "the report is honestly empty now: {:?}",
        log.lock().unwrap()
    );
}

#[test]
fn the_report_covers_a_declined_but_locally_added_bundle() {
    let rig = Rig::new("report-declined");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# noisy\n");
    let plane = FakePlane::new(log.clone()).with_version("s_noisy", &v);
    // Declined on the web: the feed omits it; the machine's own row still delivers it.
    plane.serve(DeliverySnapshot {
        declined: vec![("s_noisy".into(), "noisy".into())],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_noisy", "noisy", &v)], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/noisy\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("DECLINED_OVERRIDE")),
        "{:?}",
        out.warnings
    );
    assert!(rig.work.0.join("skills/noisy/SKILL.md").exists());
    // The report includes the declined-but-applied bundle — which is exactly what makes the
    // web's declined-but-applied disclosure real.
    assert!(
        log.lock().unwrap().iter().any(|l| l == "report s_noisy"),
        "{:?}",
        log.lock().unwrap()
    );
}

// =================================================================================================
// The forge add records the demand FIRST; a member failure leaves a convergent state.
// =================================================================================================

#[test]
fn a_failed_member_install_leaves_the_row_and_the_next_update_converges() {
    let rig = Rig::new("add-partial");
    let proj = project("proj-partial", "[bundles]\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v1\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
        ],
    ));
    // beta's destination is OCCUPIED by foreign content — its install will refuse.
    let beta_dest = proj.0.join(".claude/skills/beta");
    std::fs::create_dir_all(&beta_dest).unwrap();
    std::fs::write(beta_dest.join("notes.txt"), b"mine\n").unwrap();

    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        true,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => d,
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    };
    // The DEMAND landed first — the row is in the manifest even though beta did not land.
    let manifest = std::fs::read_to_string(proj.0.join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("github.com/o/r"), "{manifest}");
    assert!(proj.0.join(".claude/skills/alpha/SKILL.md").exists());
    let note = data.note.clone().unwrap_or_default();
    assert!(note.contains("did not land: beta"), "{note}");
    assert!(
        note.contains("`topos update` completes the landing"),
        "{note}"
    );
    assert_eq!(
        std::fs::read(beta_dest.join("notes.txt")).unwrap(),
        b"mine\n",
        "the occupant is never clobbered"
    );

    // The occupation clears; the ordinary explicit update converges the landing — the row was
    // already the demand.
    std::fs::remove_dir_all(&beta_dest).unwrap();
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        beta_dest.join("SKILL.md").exists(),
        "converged: {:?}",
        out.warnings
    );
}

// =================================================================================================
// `write_row` over an existing row: the exact-inverse discipline.
// =================================================================================================

/// The catalog + session fixture the workspace-reference add arms resolve through.
fn add_rig(tag: &str) -> (Rig, FakePlane, FakeDirectory, Version) {
    let rig = Rig::new(tag);
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    (rig, plane, dir, v)
}

fn applied_add(
    ctx: &Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    raw: &str,
) -> topos_types::results::AddData {
    match ops::add_reference(ctx, &connect(plane, dir), None, raw, true, false).unwrap() {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    }
}

#[test]
fn a_replaced_row_value_offers_only_the_exact_inverse() {
    let (rig, plane, dir, _v) = add_rig("row-replace");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let digest = "0123456789abcdef".repeat(4);

    // Replacing a `"*"` row with a pin: applied, the prior value named, the undo re-adds `"*"`.
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let data = applied_add(&ctx, &plane, &dir, &format!("@{WS_NAME}/deploy@{digest}"));
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "add".to_owned(),
            "-g".to_owned(),
            format!("{HOST}/{WS_NAME}/deploy"),
        ],
        "the undo restores the prior `\"*\"`, never a bare remove"
    );
    assert!(
        data.note.as_deref().unwrap_or("").contains("prior value"),
        "{:?}",
        data.note
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains(&digest), "the replace applied: {text}");

    // Replacing a PIN with `"*"`: the undo re-adds the exact pin.
    let data = applied_add(&ctx, &plane, &dir, &format!("@{WS_NAME}/deploy"));
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "add".to_owned(),
            "-g".to_owned(),
            format!("{HOST}/{WS_NAME}/deploy@{digest}"),
        ],
        "the undo restores the prior pin"
    );

    // The SAME value again: the redundancy disclosure — nothing written, and no undo (a remove
    // would delete a row that predates this add).
    let before =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let data = applied_add(&ctx, &plane, &dir, &format!("@{WS_NAME}/deploy"));
    assert!(data.undo.is_empty(), "{:?}", data.undo);
    assert!(
        data.note
            .as_deref()
            .unwrap_or("")
            .contains("already recorded"),
        "{:?}",
        data.note
    );
    let after =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert_eq!(before, after, "a same-value re-add writes nothing");

    // A prior FIELDS value: no single command restores it — no undo, and the note says why.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ harness = [\"codex\"] }}\n"
    ));
    let data = applied_add(&ctx, &plane, &dir, &format!("@{WS_NAME}/deploy@{digest}"));
    assert!(
        data.undo.is_empty(),
        "no undo beats a wrong one: {:?}",
        data.undo
    );
    assert!(
        data.note.as_deref().unwrap_or("").contains("no undo"),
        "{:?}",
        data.note
    );
}

// =================================================================================================
// The remove loss-guard covers EVERY placement-retiring arm: feed drop and the `"off"` switch.
// =================================================================================================

/// Install the feed's one bundle and return the placed dir.
fn install_feed_deploy(rig: &Rig, plane: &FakePlane, dir: &FakeDirectory) -> PathBuf {
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, plane, dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.join("SKILL.md").exists());
    placed
}

#[test]
fn an_off_switch_over_a_draft_is_describe_first() {
    let rig = Rig::new("off-gate");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // CLEAN: the off switch is a reversible file edit — applies immediately.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Applied(data) => {
            assert!(data.applied);
            let text =
                std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE))
                    .unwrap();
            assert!(text.contains("\"off\""), "{text}");
        }
        other => panic!("a clean off switch applies immediately: {other:?}"),
    }
    // Lift the switch back for the drafted arm.
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);

    // DRAFTED: local edits make the same act loss-shaped — describe-first, applied under --yes.
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { data, yes_argv } => {
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("local edits"), "{note}");
            assert!(yes_argv.contains(&"--yes".to_owned()), "{yes_argv:?}");
        }
        other => panic!("a drafted off switch describes first: {other:?}"),
    }
    let out =
        ops::remove_global(&ctx, &connect(&plane, &dir), &["deploy".into()], None, true).unwrap();
    assert!(
        matches!(out, ops::RemoveOutcome::Applied(_)),
        "--yes applies the described act"
    );
}

#[test]
fn a_feed_drop_over_a_draft_is_describe_first() {
    let rig = Rig::new("feeddrop-gate");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // DRAFTED: dropping the feed row retires ALL its bundles' placements — with a draft among
    // them the act is loss-shaped and describes first, naming the drafted bundle.
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    let token = format!("@{WS_NAME}");
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        std::slice::from_ref(&token),
        None,
        false,
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { data, yes_argv } => {
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("local edits"), "{note}");
            assert!(note.contains("deploy"), "names the drafted bundle: {note}");
            assert!(yes_argv.contains(&"--yes".to_owned()));
        }
        other => panic!("a drafted feed drop describes first: {other:?}"),
    }
    // --yes applies: the row goes.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        std::slice::from_ref(&token),
        None,
        true,
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains(&format!("{HOST}/{WS_NAME}")), "{text}");

    // CLEAN: with no draft the same drop applies immediately. (The edited copy survived the
    // drop — restore its bytes to the served version so nothing is loss-shaped.)
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    std::fs::write(placed.join("SKILL.md"), b"# deploy\n").unwrap();
    let out = ops::remove_global(&ctx, &connect(&plane, &dir), &[token], None, false).unwrap();
    assert!(
        matches!(out, ops::RemoveOutcome::Applied(_)),
        "a clean feed drop applies immediately"
    );
}

// =================================================================================================
// The set split carries the line's fields onto each surviving member.
// =================================================================================================

#[test]
fn a_set_split_carries_the_lines_fields_onto_survivors() {
    let rig = Rig::new("split-fields");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let o = one_file(b"# other\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &d)
        .with_version("s_other", &o);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &d),
            catalog_entry("s_other", "other", &o),
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
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = {{ harness = [\"codex\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The describe discloses BOTH the split and the field carriage.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { data, .. } => {
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("harness settings carry"), "{note}");
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    let out =
        ops::remove_global(&ctx, &connect(&plane, &dir), &["deploy".into()], None, true).unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap();
    let survivor = doc
        .rows
        .iter()
        .find(|r| r.reference == format!("{HOST}/{WS_NAME}/other"))
        .unwrap_or_else(|| panic!("the survivor keeps its own row: {text}"));
    match &survivor.value {
        crate::manifest::document::EntryValue::Fields(f) => {
            assert_eq!(
                f.harness.as_deref(),
                Some(&["codex".to_owned()][..]),
                "{text}"
            );
        }
        other => panic!("the set line's fields ride the member row, got {other:?}: {text}"),
    }
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference.contains("channels/backend")),
        "the set line is gone: {text}"
    );
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/deploy")),
        "the removed member gets no row: {text}"
    );
}

#[test]
fn a_set_split_never_overwrites_a_survivors_explicit_row() {
    // Explicit beats set: a survivor that already has its OWN row keeps it untouched (its pin is
    // a stronger fact than the set's fields); a survivor without one gets the new carried row.
    let rig = Rig::new("split-explicit");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let o = one_file(b"# other\n");
    let p = one_file(b"# pinned\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &d)
        .with_version("s_other", &o)
        .with_version("s_pinned", &p);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &d),
            catalog_entry("s_other", "other", &o),
            catalog_entry("s_pinned", "pinned", &p),
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
                WireChannelSkill {
                    skill_id: "s_pinned".into(),
                    name: "pinned".into(),
                },
            ],
        }],
    );
    let pin = topos_core::digest::to_hex(&p.id);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = {{ harness = [\"codex\"] }}\n\
         \"{HOST}/{WS_NAME}/pinned\" = \"{pin}\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The describe names which survivors get new rows and which keep their own.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { data, .. } => {
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("new rows are written for other"), "{note}");
            assert!(
                note.contains("pinned already has its own row here, kept untouched"),
                "{note}"
            );
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    let out =
        ops::remove_global(&ctx, &connect(&plane, &dir), &["deploy".into()], None, true).unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap();
    // The explicit survivor KEEPS its pin — never replaced by the set's fields value.
    let kept = doc
        .rows
        .iter()
        .find(|r| r.reference == format!("{HOST}/{WS_NAME}/pinned"))
        .unwrap_or_else(|| panic!("the explicit survivor keeps its row: {text}"));
    assert!(
        matches!(&kept.value, crate::manifest::document::EntryValue::Pin(v) if *v == pin),
        "the explicit pin survives the split, got {:?}: {text}",
        kept.value
    );
    // The row-less survivor gets the NEW carried row; the set line and the removed member go.
    let fresh = doc
        .rows
        .iter()
        .find(|r| r.reference == format!("{HOST}/{WS_NAME}/other"))
        .unwrap_or_else(|| panic!("the row-less survivor gets a new row: {text}"));
    assert!(
        matches!(&fresh.value, crate::manifest::document::EntryValue::Fields(f)
            if f.harness.as_deref() == Some(&["codex".to_owned()][..])),
        "{text}"
    );
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference.contains("channels/backend")),
        "the set line is gone: {text}"
    );
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/deploy")),
        "the removed member gets no row: {text}"
    );
}

// =================================================================================================
// A landed publish is never failed by its LOCAL rewrite half; the retry/update converge it.
// =================================================================================================

/// A contribute fake that lands every publish: it re-derives the candidate's commit id exactly
/// as the plane would (server rehash) and answers OK with the moved pointer.
struct OkPublish;
impl crate::plane::ContributeSource for OkPublish {
    fn publish(
        &self,
        b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        use base64::Engine as _;
        let entries: Vec<ManifestEntry> = b
            .candidate
            .files
            .iter()
            .map(|f| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&f.content_base64)
                    .unwrap();
                ManifestEntry {
                    path: f.path.clone(),
                    mode: match f.mode {
                        topos_types::requests::WireFileMode::Regular => FileMode::Regular,
                        topos_types::requests::WireFileMode::Executable => FileMode::Executable,
                    },
                    content_sha256: digest::sha256(&bytes),
                }
            })
            .collect();
        let tree = digest::bundle_digest(&entries).unwrap();
        let parents: Vec<[u8; 32]> = b
            .candidate
            .parents
            .iter()
            .map(|p| {
                let mut a = [0u8; 32];
                hex::decode_to_slice(p, &mut a).unwrap();
                a
            })
            .collect();
        let id = topos_core::identity::commit_id(&Commit {
            parents: &parents,
            tree,
            author: &b.candidate.author,
            message: &b.candidate.message,
        })
        .unwrap();
        let record = topos_types::WireCurrentRecord {
            schema_version: topos_types::WIRE_SCHEMA_VERSION,
            scope: topos_types::PointerScope {
                workspace_id: b.workspace_id.clone(),
                skill_id: b.skill_id.clone(),
            },
            record: topos_types::CurrentRecord {
                version_id: topos_core::digest::to_hex(&id),
                generation: 1,
            },
        };
        Ok(crate::plane::WriteReceipt {
            receipt: Some(topos_types::Receipt {
                schema_version: 1,
                op_id: b.op_id.clone(),
                command: "publish".to_owned(),
                outcome: topos_types::TerminalOutcome::Ok,
                workspace_id: b.workspace_id,
                skill_id: Some(b.skill_id),
                version_id: Some(record.record.version_id.clone()),
                bundle_digest: None,
                expected_generation: None,
                current_generation: Some(1),
                created_at: "2026-07-30T00:00:00Z".to_owned(),
                details: None,
            }),
            error: None,
            wire_record: Some(record),
        })
    }
    fn propose(
        &self,
        _b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no propose in this flow")
    }
    fn revert(
        &self,
        _b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no revert in this flow")
    }
    fn review(
        &self,
        _b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no review in this flow")
    }
}

#[test]
fn a_landed_publish_survives_a_failed_rewrite_and_the_next_update_converges_it() {
    let rig = Rig::new("rewrite-pending");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());

    // A local skill adopted by path.
    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();

    // The catalog serves the governed copy (for the later converge).
    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(
        vec![catalog_entry(&added.skill_id, "deploy", &v)],
        Vec::new(),
    );

    // The GLOBAL manifest is UNPARSEABLE: the rewrite half will fail; the publish must not.
    rig.write_global("this is [[ not toml\n");

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let cc = |_base: &str, _tok: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        Box::new(NoContribute)
    };
    let outcome = ops::publish(
        &ctx,
        &cc,
        None,
        Some(&session_connect),
        None,
        "deploy",
        false,
        None,
        None,
        None,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    let pending = data
        .rewrite_pending
        .expect("the receipt carries the pending rewrite");
    assert!(pending.contains("could not be rewritten"), "{pending}");
    assert!(pending.contains("topos update"), "{pending}");
    assert!(
        data.manifest.is_none() && data.converted_from.is_none(),
        "the receipt never claims a rewrite that did not land"
    );

    // The manifest heals (a fixed file still spelling the PATH line — exactly what the failed
    // rewrite left behind); the NEXT update converges the transfer, idempotently, disclosed.
    let canonical_src = src.canonicalize().unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = \"*\"\n",
        canonical_src.display()
    ));
    let out = ops::manifest_update(
        &ctx,
        &session_connect,
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = out
        .warnings
        .iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.warnings));
    assert!(line.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{line}");
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "the canonical reference stands: {text}"
    );
    assert!(
        !text.contains(&canonical_src.display().to_string()),
        "the path line is gone: {text}"
    );

    // Idempotent: the next sweep finds nothing left to converge.
    let out2 = ops::manifest_update(
        &ctx,
        &session_connect,
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        !out2
            .warnings
            .iter()
            .any(|w| w.starts_with("GOVERNANCE_CONVERGED")),
        "{:?}",
        out2.warnings
    );
}

#[test]
fn a_project_scope_pending_rewrite_converges_from_the_projects_own_store() {
    // The pending governance transfer of a bundle tracked in the PROJECT's own store: the
    // converge must search the owning store across the cwd chain — a home-only search would
    // leave a project-scope publish's failed rewrite pending forever.
    let rig = Rig::new("rewrite-pending-proj");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let proj = project("proj-pending", "[bundles]\n");

    // A local skill adopted into the PROJECT's own store (the checkout's engine state).
    let src = proj.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let playout = crate::sidecar::ensure_project_store(&rig.fs, &proj.0).unwrap();
    let added = {
        let mut pctx = rig.ctx_at(Some(&proj.0));
        pctx.layout = playout.clone();
        ops::add(&pctx, &src).unwrap()
    };
    let ctx = rig.ctx_at(Some(&proj.0));

    // The catalog serves the governed copy (for the later converge).
    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(
        vec![catalog_entry(&added.skill_id, "deploy", &v)],
        Vec::new(),
    );

    // The PROJECT manifest is UNPARSEABLE: the rewrite half will fail; the publish must not.
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        "this is [[ not toml\n",
    )
    .unwrap();
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let cc = |_base: &str, _tok: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        Box::new(NoContribute)
    };
    let outcome = ops::publish(
        &ctx,
        &cc,
        None,
        Some(&session_connect),
        None,
        "deploy",
        false,
        None,
        None,
        None,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    assert!(
        data.rewrite_pending.is_some(),
        "the failed rewrite rides the receipt"
    );

    // The manifest heals, still spelling the PATH line; the NEXT update converges the transfer
    // from the PROJECT store, idempotently, disclosed.
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        "[bundles]\n\"./deploy\" = \"*\"\n",
    )
    .unwrap();
    let out = ops::manifest_update(
        &ctx,
        &session_connect,
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = out
        .warnings
        .iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.warnings));
    assert!(line.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{line}");
    let text = std::fs::read_to_string(proj.0.join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "the canonical reference stands in the PROJECT file: {text}"
    );
    assert!(!text.contains("./deploy"), "the path line is gone: {text}");
}

#[test]
fn a_removal_that_lands_mid_publish_is_never_silently_undone() {
    // LOCK, THEN RESOLVE. The rewrite's row is a decision read from a file, and it is only true of
    // the file the writer lock guards — so the unlocked probe only picks WHICH file, and the row is
    // re-resolved under the lock. A `topos remove` that completed in that window leaves the person
    // with a state they chose: either serialization order they could have observed ends with the
    // row gone, and "removed, then quietly re-added" is neither. The publish still LANDS remotely
    // (the plane holds it); the receipt discloses that the local half wrote nothing.
    let rig = Rig::new("rewrite-raced");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());

    // A local skill adopted by path, and the manifest row that references it.
    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();
    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(
        vec![catalog_entry(&added.skill_id, "deploy", &v)],
        Vec::new(),
    );
    let canonical_src = src.canonicalize().unwrap();
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = \"*\"\n",
        canonical_src.display()
    ));

    // The RACER: a concurrent removal completes between the probe that chose the file (read 1) and
    // the re-resolve under the lock (read 2). topos's own writers serialize on that lock; this one
    // stands for the removal that already finished before the lock was taken.
    let racing = manifest.clone();
    let fs = crate::fs_seam::HookFs::before_nth_read(&manifest, 2, move || {
        std::fs::write(&racing, "[bundles]\n").unwrap();
    });
    let hooked = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let cc = |_base: &str, _tok: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        Box::new(NoContribute)
    };
    let outcome = ops::publish(
        &hooked,
        &cc,
        None,
        Some(&session_connect),
        None,
        "deploy",
        false,
        None,
        None,
        None,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    let skipped = data
        .rewrite_skipped
        .expect("the receipt discloses the transfer it did not make");
    assert!(
        skipped.contains(&manifest.display().to_string()),
        "{skipped}"
    );
    assert!(
        data.manifest.is_none() && data.reference.is_none() && data.converted_from.is_none(),
        "the receipt never claims a rewrite that did not land"
    );

    // The removal STANDS: no canonical row was written back over it, and the path row it dropped
    // stays dropped.
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        !text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "no workspace row was written: {text}"
    );
    assert!(
        !text.contains(&canonical_src.display().to_string()),
        "the racer's removal stands: {text}"
    );
}

/// A contribute fake for the `--propose` arm: the proposal LANDS remotely (NEEDS_REVIEW);
/// `current` does not move, so the local sync stays GENESIS-observed.
struct OkPropose;
impl crate::plane::ContributeSource for OkPropose {
    fn publish(
        &self,
        _b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("this flow proposes")
    }
    fn propose(
        &self,
        b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        Ok(crate::plane::WriteReceipt {
            receipt: Some(topos_types::Receipt {
                schema_version: 1,
                op_id: b.op_id.clone(),
                command: "propose".to_owned(),
                outcome: topos_types::TerminalOutcome::NeedsReview,
                workspace_id: b.workspace_id,
                skill_id: Some(b.skill_id),
                version_id: None,
                bundle_digest: None,
                expected_generation: None,
                current_generation: None,
                created_at: "2026-07-30T00:00:00Z".to_owned(),
                details: None,
            }),
            error: None,
            wire_record: None,
        })
    }
    fn revert(
        &self,
        _b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no revert in this flow")
    }
    fn review(
        &self,
        _b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!("no review in this flow")
    }
}

#[test]
fn a_genesis_propose_pending_rewrite_still_converges() {
    // A genesis `--propose` never moves `current`, so the local sync stays GENESIS-observed —
    // the converge's safe condition is the single-catalog-home check (the landed proposal minted
    // the catalog entry), never a blanket GENESIS refusal.
    let rig = Rig::new("rewrite-pending-genesis");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());

    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();

    // The catalog holds the proposal's entry (the server-side half of the landed propose).
    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(
        vec![catalog_entry(&added.skill_id, "deploy", &v)],
        Vec::new(),
    );

    // The GLOBAL manifest is UNPARSEABLE: the rewrite half fails; the proposal still lands.
    rig.write_global("this is [[ not toml\n");
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPropose),
        governance: Box::new(NoGovernance),
    };
    let cc = |_base: &str, _tok: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        Box::new(NoContribute)
    };
    let outcome = ops::publish(
        &ctx,
        &cc,
        None,
        Some(&session_connect),
        None,
        "deploy",
        true,
        None,
        None,
        None,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Proposed(d) => d,
        other => panic!("the genesis propose opens a proposal: {other:?}"),
    };
    assert!(
        data.rewrite_pending.is_some(),
        "the failed rewrite rides the proposal receipt"
    );

    // The manifest heals with the PATH line; the update converges the transfer despite the
    // GENESIS-observed local sync.
    let canonical_src = src.canonicalize().unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = \"*\"\n",
        canonical_src.display()
    ));
    let out = ops::manifest_update(
        &ctx,
        &session_connect,
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = out
        .warnings
        .iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.warnings));
    assert!(line.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{line}");
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "the canonical reference stands: {text}"
    );
}

// =================================================================================================
// Self-ignore breadth: the forge IMPORT path stages the sentinel; a shipped ignore discloses.
// =================================================================================================

/// `git init` + `git status --porcelain` — the real visibility witness (`None` = no git binary;
/// the caller skips that half).
fn git_status(repo: &std::path::Path) -> Option<String> {
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !init.status.success() {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-uall"])
        .current_dir(repo)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn a_project_import_stages_the_sentinel_and_a_shipped_ignore_discloses() {
    let rig = Rig::new("import-sentinel");
    let proj = project("proj-sentinel", "[bundles]\n");
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha\n")],
    ));
    let ctx = rig.ctx_at(Some(&proj.0));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(proj.0.clone()),
    };
    let spec = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: "o".into(),
        repo: "r".into(),
        git_ref: None,
        subdir: None,
    };
    let data = ops::add_remote(
        &ctx,
        &git,
        &spec,
        &roots,
        &ops::AddRemoteOpts {
            skill: Some("alpha".into()),
            harness: None,
            global: false,
        },
    )
    .unwrap();
    let alpha = proj.0.join(".claude/skills/alpha");
    assert_eq!(
        std::fs::read(alpha.join(crate::scan::IGNORE_FILE)).unwrap(),
        crate::scan::IGNORE_SENTINEL,
        "the import path stages the sentinel exactly like the materializer"
    );
    assert!(
        data.note.is_none(),
        "a sentinel placement needs no disclosure: {:?}",
        data.note
    );

    // A repo shipping its OWN root ignore that does NOT self-ignore: placed verbatim, disclosed.
    git.serve(build_repo_targz(
        "o-r2-cccccccccccc3",
        &[
            ("skills/beta/SKILL.md", b"# beta\n"),
            ("skills/beta/.gitignore", b"*.log\n"),
        ],
    ));
    let spec2 = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: "o".into(),
        repo: "r2".into(),
        git_ref: None,
        subdir: None,
    };
    let data2 = ops::add_remote(
        &ctx,
        &git,
        &spec2,
        &roots,
        &ops::AddRemoteOpts {
            skill: Some("beta".into()),
            harness: None,
            global: false,
        },
    )
    .unwrap();
    let beta = proj.0.join(".claude/skills/beta");
    assert_eq!(
        std::fs::read(beta.join(".gitignore")).unwrap(),
        b"*.log\n",
        "a shipped root ignore is content, never overlaid"
    );
    let note = data2.note.clone().unwrap_or_default();
    assert!(note.contains("visible to git"), "{note}");

    // The REAL git witness: the sentinel placement is invisible; the shipped-ignore one shows.
    match git_status(&proj.0) {
        Some(status) => {
            assert!(!status.contains("alpha"), "{status}");
            assert!(status.contains("beta"), "{status}");
        }
        None => eprintln!("skipping git-visibility half: no usable git binary"),
    }
}

#[test]
fn a_delivered_bundle_shipping_a_non_self_ignoring_gitignore_warns_on_the_sweep() {
    let rig = Rig::new("sweep-gitvisible");
    rig.seed_session();
    let proj = project(
        "proj-gitvisible",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[
        ("SKILL.md", FileMode::Regular, b"# deploy\n"),
        (".gitignore", FileMode::Regular, b"*.log\n"),
    ]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    let placed = proj.0.join(".claude/skills/deploy");
    assert_eq!(
        std::fs::read(placed.join(".gitignore")).unwrap(),
        b"*.log\n",
        "bundle content is never edited: {:?}",
        out.warnings
    );
    let w = out
        .warnings
        .iter()
        .find(|w| w.starts_with("GIT_VISIBLE"))
        .unwrap_or_else(|| panic!("the visibility disclosure: {:?}", out.warnings));
    assert!(w.contains("deploy"), "{w}");
}

#[test]
fn a_manifest_row_alone_never_trusts_the_origin() {
    // Trust in a forge origin is a STORE fact. A VCS-delivered manifest row is demand, never
    // consent: a bare `add` of that exact reference still gets the member-listing describe
    // (noting the row exists), `--yes` is the consent, and only then — with the origin tracked
    // in the scope's store — do further adds flow ungated.
    let rig = Rig::new("row-no-trust");
    let proj = project("proj-rowtrust", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n"),
            ("skills/beta/SKILL.md", b"# beta\n"),
        ],
    ));

    // The bare add DESCRIBES — the row's existence changes the wording, never the gate.
    let outcome = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        false,
    )
    .unwrap();
    match outcome {
        ops::AddRefOutcome::Described { data, yes_argv } => {
            assert_eq!(data.members, vec!["alpha".to_owned(), "beta".to_owned()]);
            let note = data.note.expect("the describe names the standing row");
            assert!(note.contains("already records this row"), "{note}");
            assert!(note.contains("demand, not consent"), "{note}");
            assert!(yes_argv.contains(&"--yes".to_owned()));
        }
        ops::AddRefOutcome::Applied(_) => {
            panic!("a manifest row must never skip the first-trust describe")
        }
    }
    assert!(
        !proj.0.join(".claude/skills/alpha").exists(),
        "the describe installs nothing"
    );

    // `--yes` is the consent: the origin becomes a store fact.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        true,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(proj.0.join(".claude/skills/alpha/SKILL.md").exists());

    // Tracked now: a further BARE add of the same origin flows ungated.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r/beta",
        false,
        false,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => {
            panic!("a tracked origin's adds apply immediately")
        }
    }
}

// =================================================================================================
// The SELECTOR import (`-s`/`-a`) — the same first-trust gate, the same per-scope store.
// =================================================================================================

#[test]
fn a_selector_import_describes_first_then_grants_and_installs() {
    // A selector narrows WHICH members land and WHERE; it is not a way around whose bytes these
    // are. So `add owner/repo -s alpha` on an origin this machine has never granted DESCRIBES,
    // fetches nothing into place, and applies only under `--yes` — which grants the origin.
    let rig = Rig::new("sel-gate");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n"),
            ("skills/beta/SKILL.md", b"# beta\n"),
        ],
    ));
    let skills = vec!["alpha".to_owned()];

    let described = ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &skills,
        &[],
        true,
        false,
    )
    .unwrap();
    match described {
        ops::AddManyOutcome::Described { data, yes_argv } => {
            assert_eq!(data.source, "github.com/o/r");
            assert_eq!(data.members, vec!["alpha".to_owned()]);
            assert!(yes_argv.contains(&"--yes".to_owned()), "{yes_argv:?}");
            assert!(yes_argv.contains(&"-s".to_owned()), "{yes_argv:?}");
        }
        ops::AddManyOutcome::Applied(_) => panic!("an untracked origin describes first"),
    }
    assert!(
        !rig.home.0.join(".claude/skills/alpha").exists(),
        "a describe installs nothing"
    );

    // `--yes` GRANTS the origin, then installs.
    let applied = ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &skills,
        &[],
        true,
        true,
    )
    .unwrap();
    match applied {
        ops::AddManyOutcome::Applied(items) => assert_eq!(items.len(), 1),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());
    assert!(
        !rig.home.0.join(".claude/skills/beta").exists(),
        "the selector narrowed the landing"
    );
    // The grant is a MACHINE fact now: a later bare reference add of the same origin is ungated.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r/beta",
        true,
        false,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("the origin is granted"),
    }
}

#[test]
fn a_project_scope_selector_import_converges_on_a_later_update() {
    // The bug this pins: a selector import that wrote its engine state into the HOME store while
    // the row lived in a PROJECT manifest — the project reconcile reads the checkout's own store,
    // so it could never see the import and would re-install it forever.
    let rig = Rig::new("sel-project");
    let proj = project("sel-proj", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha\n")],
    ));
    match ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &["alpha".to_owned()],
        &[],
        false,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(items) => assert_eq!(items.len(), 1),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    // The import lives in the CHECKOUT's own store — the one the project reconcile reads.
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the selector import minted the project store");
    let pctx = ops_ctx_with_layout(&ctx, &playout);
    assert_eq!(
        crate::ops::forge_imports(&pctx).len(),
        1,
        "the project store tracks the import"
    );
    let fetches_before = git.fetches();

    // The explicit update therefore CONVERGES it — no second fetch, no re-install.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?} / {:?}",
        out.data.skills,
        out.warnings
    );
    assert_eq!(
        git.fetches(),
        fetches_before + 1,
        "one fetch to compare the commit, and nothing re-installed"
    );
}

/// The store-routing helper the reconcile uses, reachable from the suite.
fn ops_ctx_with_layout<'a>(ctx: &'a Ctx<'a>, layout: &Layout) -> Ctx<'a> {
    Ctx {
        layout: layout.clone(),
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        harness: ctx.harness,
        plane: ctx.plane,
        follow: ctx.follow,
        roots: ctx.roots.clone(),
    }
}

#[test]
fn a_partially_landed_pinned_repo_set_converges_on_the_next_update() {
    // A pinned set that landed 1 of 2 members reads "every tracked member is at the pin" — which
    // is true and useless: the MISSING member never arrives. The recorded member set (what the
    // archive held at that commit) is what makes the gap visible.
    let rig = Rig::new("pin-partial");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n"),
            ("skills/beta/SKILL.md", b"# beta\n"),
        ],
    ));
    // A PARTIAL landing: only `alpha` is imported (a member failure, a crash — the shape the
    // gate's own receipt warns about).
    match ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &["alpha".to_owned()],
        &[],
        true,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(items) => assert_eq!(items.len(), 1),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());
    assert!(!rig.home.0.join(".claude/skills/beta").exists());

    // The recipe is now the PINNED SET at exactly the commit that landing sits on — every tracked
    // member satisfies the pin, and only the recorded member set can tell that one is missing.
    // (The pin is the fake archive's own `TOP/` commit suffix.)
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"aaaaaaaaaaaa1\"\n");

    // The explicit update sees the gap against the RECORDED member set and lands the rest.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        rig.home.0.join(".claude/skills/beta/SKILL.md").exists(),
        "the missing member converges: {:?} / {:?}",
        out.data.skills,
        out.warnings
    );
}

// =================================================================================================
// Removing SEVERAL members of one set is ONE split; a bare name that names two rows refuses.
// =================================================================================================

#[test]
fn removing_two_members_of_one_set_leaves_neither() {
    // The bug this pins: two SetSplit arms applied in sequence each rebuilt the set line from its
    // FULL member list, so the second one wrote the first one's removal straight back.
    let rig = Rig::new("split-multi");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let c = one_file(b"# gamma\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b)
        .with_version("s_c", &c);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
            catalog_entry("s_c", "gamma", &c),
        ],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![
                WireChannelSkill {
                    skill_id: "s_a".into(),
                    name: "alpha".into(),
                },
                WireChannelSkill {
                    skill_id: "s_b".into(),
                    name: "beta".into(),
                },
                WireChannelSkill {
                    skill_id: "s_c".into(),
                    name: "gamma".into(),
                },
            ],
        }],
    );
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let targets = vec!["alpha".to_owned(), "beta".to_owned()];

    // The describe reflects the COMBINED split: one line, both names, one survivor.
    match ops::remove_global(&ctx, &connect(&plane, &dir), &targets, None, false).unwrap() {
        ops::RemoveOutcome::Described { data, .. } => {
            assert_eq!(data.items.len(), 1, "one line split once: {:?}", data.items);
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("alpha") && note.contains("beta"), "{note}");
            assert!(note.contains("new rows are written for gamma"), "{note}");
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    assert!(matches!(
        ops::remove_global(&ctx, &connect(&plane, &dir), &targets, None, true).unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap();
    let refs: Vec<&str> = doc.rows.iter().map(|r| r.reference.as_str()).collect();
    assert!(
        !refs.contains(&format!("{HOST}/{WS_NAME}/alpha").as_str()),
        "alpha is gone: {text}"
    );
    assert!(
        !refs.contains(&format!("{HOST}/{WS_NAME}/beta").as_str()),
        "beta is gone too — the second split must not resurrect the first: {text}"
    );
    assert!(
        refs.contains(&format!("{HOST}/{WS_NAME}/gamma").as_str()),
        "the survivor keeps flowing: {text}"
    );
}

#[test]
fn a_bare_name_two_rows_answer_to_is_refused_not_guessed() {
    // Two rows deliver a `deploy` — one from a workspace, one from a repo. Taking the first is a
    // coin flip with someone's row; the refusal names both qualified references.
    let rig = Rig::new("ambig-remove");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n\"github.com/o/deploy\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{msg}");
    assert!(msg.contains("github.com/o/deploy"), "{msg}");
    // Nothing moved.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("github.com/o/deploy"), "{text}");

    // Spelled in full, exactly one row answers — and it applies.
    let one = format!("{HOST}/{WS_NAME}/deploy");
    assert!(matches!(
        ops::remove_global(&ctx, &connect(&plane, &dir), &[one], None, true).unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        !text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "{text}"
    );
    assert!(
        text.contains("github.com/o/deploy"),
        "the other row stands: {text}"
    );
}

// =================================================================================================
// The project containment rail — a committed symlink never aims managed bytes out of the checkout.
// =================================================================================================

#[test]
fn a_committed_topos_symlink_refuses_the_project_store() {
    // A repo can commit `.topos` as a symlink exactly as easily as `.claude/skills`. The store is
    // REFUSED, never followed — and a plain directory still works.
    let rig = Rig::new("store-escape");
    let proj = project("store-escape-proj", "[bundles]\n");
    let outside = Scratch::new("store-escape-outside");
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".topos")).unwrap();
    let err = crate::sidecar::ensure_project_store(&rig.fs, &proj.0).unwrap_err();
    assert_eq!(err.code(), "PLACEMENT_UNSUPPORTED", "{err:?}");
    assert!(
        err.to_string().contains("PLACEMENT_ESCAPES_PROJECT"),
        "{err}"
    );
    assert!(
        !outside.0.join("state").exists(),
        "nothing was written through the symlink"
    );
    // The read-side probe refuses it too, so no report or clean ever visits it.
    assert!(crate::sidecar::existing_project_store(&rig.fs, &proj.0).is_none());

    // A NORMAL checkout still mints its store.
    let ok = project("store-ok-proj", "[bundles]\n");
    let layout = crate::sidecar::ensure_project_store(&rig.fs, &ok.0).unwrap();
    assert!(layout.home().exists());
    assert!(crate::sidecar::existing_project_store(&rig.fs, &ok.0).is_some());
}

#[test]
fn a_committed_skills_symlink_is_refused_as_a_placement_root() {
    // The DEFAULT project root gets the override's rail: `.claude/skills` committed as a symlink
    // out of the checkout places nothing, and the sweep says so.
    let rig = Rig::new("root-escape");
    let proj = project("root-escape-proj", "[bundles]\n");
    let outside = Scratch::new("root-escape-outside");
    std::fs::create_dir_all(proj.0.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".claude/skills")).unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    let plan = crate::placement::project_plan(
        &ctx,
        &proj.0,
        "topos_deadbeef",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: Some(WS_NAME),
        },
        None,
        None,
        None,
    );
    assert!(
        plan.refused
            .iter()
            .any(|r| r.starts_with("PLACEMENT_ESCAPES_PROJECT")),
        "the escaping root is refused: {:?}",
        plan.refused
    );
    assert!(
        plan.targets.iter().all(|t| !t.dir.starts_with(&outside.0)),
        "nothing is aimed outside the checkout: {:?}",
        plan.targets
    );

    // A NORMAL checkout still plans its in-repo dirs.
    let ok = project("root-ok-proj", "[bundles]\n");
    let ok_ctx = rig.ctx_at(Some(&ok.0));
    let plan = crate::placement::project_plan(
        &ok_ctx,
        &ok.0,
        "topos_deadbeef",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: Some(WS_NAME),
        },
        None,
        None,
        None,
    );
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    assert!(
        plan.targets.iter().all(|t| t.dir.starts_with(&ok.0)),
        "{:?}",
        plan.targets
    );
}

// =================================================================================================
// The applied report's cross-store pick is deterministic, and a version split is disclosed.
// =================================================================================================

#[test]
fn a_bundle_held_at_two_versions_reports_the_person_copy_and_discloses_the_split() {
    // The wire carries ONE row per (session, bundle). Which store answers must not depend on which
    // checkout the update happened to run from — the PERSON store answers whenever it holds the
    // bundle — and the version the OTHER store holds must not simply vanish from the person's view.
    let rig = Rig::new("split-report");
    rig.seed_session();
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let v1_hex = topos_core::digest::to_hex(&v1.id);
    let proj = project(
        "proj-split",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"{v1_hex}\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    // The FEED serves the current version; the project row is PINNED to the older one.
    plane.serves(vec![delivered("s_deploy", "deploy", &v2)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v2)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);

    assert_eq!(
        std::fs::read(rig.work.0.join("skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the person scope takes the served current"
    );
    assert_eq!(
        std::fs::read(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the project scope holds its pin"
    );
    // The reported row is the PERSON store's — deterministically, not by iteration luck.
    let reported = plane.reported.lock().unwrap().clone();
    let row = reported
        .iter()
        .find(|(id, _)| id == "s_deploy")
        .unwrap_or_else(|| panic!("the bundle is reported: {reported:?}"));
    assert_eq!(row.1, topos_core::digest::to_hex(&v2.id), "{reported:?}");
    // And the split the single row cannot carry is said out loud.
    let line = out
        .disclosures
        .iter()
        .find(|w| w.starts_with("VERSION_SPLIT"))
        .unwrap_or_else(|| panic!("the split is disclosed: {:?}", out.disclosures));
    assert!(line.contains("s_deploy"), "{line}");
    assert!(
        line.contains(&v1_hex[..12]),
        "names the other version: {line}"
    );
    assert!(
        line.contains(&proj.0.display().to_string()),
        "names which store holds it: {line}"
    );
}

// =================================================================================================
// The machine-local registries fail CLOSED on a document this build cannot decipher.
// =================================================================================================

/// A `state/<doc>` written at a schema version FROM THE FUTURE — what a newer build leaves behind
/// when someone downgrades, or runs two versions side by side.
fn write_newer_schema_doc(layout: &Layout, path: &std::path::Path, body: &str) -> Vec<u8> {
    std::fs::create_dir_all(layout.state_dir()).unwrap();
    let bytes = body.as_bytes().to_vec();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

#[test]
fn a_newer_forge_registry_grants_nothing_and_is_never_written_over() {
    let rig = Rig::new("trust-newer");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let path = rig.layout().forge_trust_path();
    let bytes = write_newer_schema_doc(
        &rig.layout(),
        &path,
        "{\n  \"schema_version\": 9999,\n  \"seeded\": true,\n  \"origins\": [\"github.com/o/r\"]\n}\n",
    );
    // (a) A trust question over an undecipherable registry answers NO — never "empty, so re-seed".
    assert!(
        !crate::forge_trust::is_trusted(&ctx, "github.com/o/r"),
        "an unreadable registry grants nothing"
    );
    // (b) …and the write REFUSES rather than replacing a document it could not read.
    let err = crate::forge_trust::grant(&ctx, "github.com/o/r").unwrap_err();
    assert_eq!(err.code(), "UPGRADE_REQUIRED", "{err:?}");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "the newer document is byte-untouched"
    );
}

#[test]
fn a_newer_visited_store_index_contributes_nothing_and_is_never_written_over() {
    let rig = Rig::new("visited-newer");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let path = rig.layout().visited_stores_path();
    let bytes = write_newer_schema_doc(
        &rig.layout(),
        &path,
        "{\n  \"schema_version\": 9999,\n  \"stores\": [\"/nowhere\"]\n}\n",
    );
    let layouts = crate::visited_stores::recall_and_record(&ctx, &[]);
    assert!(layouts.is_empty(), "no recorded store is recalled");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "the newer document is byte-untouched"
    );
}

// =================================================================================================
// Manifest mutations serialize on the file's own writer lock.
// =================================================================================================

#[test]
fn two_manifest_edits_through_the_locked_path_both_land() {
    // The lock is about SERIALIZATION, not detection: what must hold is that two edits of one file
    // — each a full read-modify-write — leave BOTH rows standing.
    let rig = Rig::new("manifest-lock");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    for name in ["alpha", "beta"] {
        match ops::add_reference(
            &ctx,
            &connect(&plane, &dir),
            None,
            &format!("@{WS_NAME}/{name}"),
            true,
            false,
        )
        .unwrap()
        {
            ops::AddRefOutcome::Applied(_) => {}
            ops::AddRefOutcome::Described { .. } => panic!("a workspace ref applies immediately"),
        }
    }
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("alpha"), "the first row survived: {text}");
    assert!(text.contains("beta"), "the second row landed: {text}");
    // The whole file still parses — a lock that let two writers interleave would not guarantee it.
    crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap_or_else(|e| panic!("{e}: {text}"));
}

#[test]
fn a_copy_edited_between_the_scan_and_the_delete_is_snapshotted_before_it_goes() {
    // The retiring sweep scans every placement, snapshots the edited ones, and only THEN deletes.
    // An edit that lands in that gap was captured by nothing — so the delete re-scans and
    // snapshots what is actually there. Nothing unsnapshotted disappears.
    let rig = Rig::new("clean-race");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let before = store_versions(&rig.layout(), "s_deploy");

    // The recipe stops asking for it — the next sweep retires the placement.
    rig.write_global("[bundles]\n");
    // The racer edits the (clean) copy between the sweep's scan and its delete.
    let racing = placed.clone();
    // The seam: the retiring loop probes `exists(dir)` immediately before it acts on the dir —
    // AFTER the scan that classified every placement as clean. An edit landing there was captured
    // by no snapshot, so only a fresh read at the retirement can save it.
    let fs = crate::fs_seam::HookFs::before_nth_exists(&placed, 1, move || {
        std::fs::write(racing.join("SKILL.md"), b"# raced edit\n").unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    sweep(&ctx, &plane, &dir);
    assert!(!placed.exists(), "the undemanded placement is retired");
    assert_eq!(
        store_versions(&rig.layout(), "s_deploy"),
        before + 1,
        "the raced edit was committed into the store before the dir went"
    );
}

#[test]
fn a_copy_edited_in_the_instant_before_the_park_is_snapshotted_too() {
    // The sharper seam: an edit landing between the LAST check and the mutation itself. A
    // verify-then-delete has nothing left to check with there; park-then-verify does — the rename
    // takes the tree out of reach and the read that decides happens on bytes nobody can still be
    // writing. The hook fires immediately before that rename.
    let rig = Rig::new("clean-park-race");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let before = store_versions(&rig.layout(), "s_deploy");

    rig.write_global("[bundles]\n");
    let racing = placed.clone();
    let fs = crate::fs_seam::HookFs::before_first_move_of(&placed, move || {
        std::fs::write(racing.join("SKILL.md"), b"# raced at the park\n").unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    sweep(&ctx, &plane, &dir);
    assert!(!placed.exists(), "the undemanded placement is retired");
    assert_eq!(
        store_versions(&rig.layout(), "s_deploy"),
        before + 1,
        "the edit that landed in the last instant was parked, read, and committed"
    );
    // No park is left behind once its bytes are safe.
    let leftovers: Vec<String> = std::fs::read_dir(placed.parent().unwrap())
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".topos-retiring-"))
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_copy_edited_in_the_instant_before_the_swap_is_snapshotted_too() {
    // The materializer's own park: an update lands new bytes over a clean copy, and the edit
    // arrives after the pre-swap re-stat — the one window a stat can never close. The swap PARKS
    // the old tree instead of deleting it, so the bytes that were really there are read and
    // committed before anything is dropped.
    let rig = Rig::new("swap-park-race");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let before = store_versions(&rig.layout(), "s_deploy");

    // v2 is served (a real pointer move — the next generation); the copy on disk is CLEAN, so the
    // sweep will overwrite it.
    let mut moved = delivered("s_deploy", "deploy", &v2);
    moved.generation = 2;
    plane.serves(vec![moved]);
    let racing = placed.clone();
    // The materializer operates on the CANONICAL dir (a symlinked temp prefix is resolved), which
    // is the path the swap actually names.
    let swapped = placed.canonicalize().unwrap();
    let fs = crate::fs_seam::HookFs::before_first_move_of(&swapped, move || {
        std::fs::write(racing.join("SKILL.md"), b"# raced at the swap\n").unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    sweep(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the new bytes landed"
    );
    assert_eq!(
        store_versions(&rig.layout(), "s_deploy"),
        before + 2,
        "the served version AND the raced edit are both in the store"
    );
}

// =================================================================================================
// A bare name that several LINES answer to is refused — one candidate set, gathered before any
// decision (row vs set, set vs set, feed vs set).
// =================================================================================================

/// A channel entry carrying `skills`.
fn channel(name: &str, skills: &[(&str, &str)]) -> WireChannelEntry {
    WireChannelEntry {
        name: name.into(),
        mode: "open".into(),
        builtin: false,
        included: true,
        skills: skills
            .iter()
            .map(|(id, n)| WireChannelSkill {
                skill_id: (*id).into(),
                name: (*n).into(),
            })
            .collect(),
    }
}

#[test]
fn a_bare_name_a_row_and_a_set_both_answer_to_is_refused_not_guessed() {
    // ROW vs SET. Dropping the row leaves the channel delivering `deploy`; splitting the channel
    // leaves the row delivering it. Picking either silently is a removal that does not remove.
    let rig = Rig::new("ambig-row-set");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &d);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &d)],
        vec![channel("backend", &[("s_deploy", "deploy")])],
    );
    rig.write_global(&format!(
        "[bundles]\n\"github.com/o/r/deploy\" = \"*\"\n\"{HOST}/{WS_NAME}/channels/backend\" = \
         \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::remove_global(&ctx, &connect(&plane, &dir), &["deploy".into()], None, true)
        .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("github.com/o/r/deploy"), "{msg}");
    assert!(msg.contains("channels/backend"), "{msg}");

    // The EXACT spelling is an answer, never an ambiguity — otherwise the refusal would be a dead
    // end (a set member has no spelling of its own).
    let one = "github.com/o/r/deploy".to_owned();
    assert!(matches!(
        ops::remove_global(&ctx, &connect(&plane, &dir), &[one], None, true).unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("github.com/o/r/deploy"), "{text}");
    assert!(
        text.contains("channels/backend"),
        "the set line stands: {text}"
    );
}

#[test]
fn a_bare_name_two_sets_carry_is_refused_not_split_at_random() {
    // SET vs SET — the fault the old first-match resolution hid completely: whichever channel
    // expanded first got split, and the other went on delivering the bundle.
    let rig = Rig::new("ambig-set-set");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &d);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &d)],
        vec![
            channel("backend", &[("s_deploy", "deploy")]),
            channel("platform", &[("s_deploy", "deploy")]),
        ],
    );
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n\
         \"{HOST}/{WS_NAME}/channels/platform\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::remove_global(&ctx, &connect(&plane, &dir), &["deploy".into()], None, true)
        .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("channels/backend"), "{msg}");
    assert!(msg.contains("channels/platform"), "{msg}");
    // And the candidates are PASTE-READY. A set member has no spelling of its own, so listing the
    // two references alone would be a dead end — each candidate is the EXACT `--via` invocation
    // that selects that one line's rewrite.
    assert!(
        msg.contains(&format!(
            "{HOST}/{WS_NAME}/deploy --via {HOST}/{WS_NAME}/channels/backend"
        )),
        "{msg}"
    );
    assert!(
        msg.contains(&format!(
            "{HOST}/{WS_NAME}/deploy --via {HOST}/{WS_NAME}/channels/platform"
        )),
        "{msg}"
    );
    // Neither line was touched.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains("channels/backend") && text.contains("channels/platform"),
        "{text}"
    );
}

#[test]
fn a_via_reference_splits_exactly_the_line_it_names() {
    // The ANSWER the set-versus-set refusal offers. `--via` names the line, so the removal is a
    // SELECTION, not a search: that one line's member-minus-one rewrite lands, and the other line
    // — never the target — stands whole, still delivering what it carries.
    let rig = Rig::new("via-picks");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &d)
        .with_version("s_beta", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &d),
            catalog_entry("s_beta", "beta", &b),
        ],
        vec![
            channel("backend", &[("s_deploy", "deploy"), ("s_beta", "beta")]),
            channel("platform", &[("s_deploy", "deploy")]),
        ],
    );
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n\
         \"{HOST}/{WS_NAME}/channels/platform\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let via = format!("{HOST}/{WS_NAME}/channels/backend");

    // A split is GATED whichever way it was selected — it rewrites a curated line — so the bare
    // run describes, and the re-run it prints carries the `--via` that made it unambiguous.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        Some(&via),
        false,
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { yes_argv, .. } => {
            assert!(yes_argv.iter().any(|a| a == "--via"), "{yes_argv:?}");
            assert!(yes_argv.contains(&via), "{yes_argv:?}");
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    // Nothing was written by the describe.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("channels/backend"), "{text}");

    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        Some(&via),
        true,
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)), "{out:?}");

    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap();
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference.contains("channels/backend")),
        "the NAMED line split: {text}"
    );
    assert!(
        doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/beta")),
        "its surviving member got its own row: {text}"
    );
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/deploy")),
        "the removed member gets no row: {text}"
    );
    assert!(
        doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/channels/platform")),
        "the line `--via` did NOT name is untouched: {text}"
    );
}

#[test]
fn a_via_that_names_no_line_or_no_member_refuses_typed() {
    // `--via` is a selection, so each MISS is its own typed refusal — never a fall-through to the
    // arms the flag does not select (a bare resolution would happily answer something else, and
    // the person would watch a removal land somewhere they did not name).
    let rig = Rig::new("via-misses");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let o = one_file(b"# other\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &d)
        .with_version("s_other", &o);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &d),
            catalog_entry("s_other", "other", &o),
        ],
        vec![
            channel("backend", &[("s_deploy", "deploy")]),
            channel("platform", &[("s_other", "other")]),
        ],
    );
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n\
         \"{HOST}/{WS_NAME}/channels/platform\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let backend = format!("{HOST}/{WS_NAME}/channels/backend");

    // (i) A line this file does not carry — named, so the refusal names it back.
    let absent = format!("{HOST}/{WS_NAME}/channels/nosuch");
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        Some(&absent),
        true,
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err:?}");
    assert!(err.to_string().contains(&absent), "{err}");

    // (ii) A real line the token does not come from. `other` IS removable here — the platform
    // line carries it — which is exactly why the flag must refuse rather than quietly split that
    // other line; the refusal lists the named line's current members so the retry is obvious.
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["other".into()],
        Some(&backend),
        true,
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("'other'"), "{msg}");
    assert!(msg.contains(&backend), "{msg}");
    assert!(msg.contains("current members: deploy"), "{msg}");

    // Both misses wrote NOTHING.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains("channels/backend") && text.contains("channels/platform"),
        "{text}"
    );
}

#[test]
fn a_bare_name_the_feed_and_a_repo_set_both_deliver_is_refused() {
    // FEED vs SET: the feed's `"off"` switch used to answer first, so a repo set carrying the same
    // name never even got looked at.
    let rig = Rig::new("ambig-feed-set");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &d);
    plane.serves(vec![delivered("s_deploy", "deploy", &d)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &d)], Vec::new());
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/deploy/SKILL.md", b"# repo deploy\n")],
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // The repo import is a real tracked member of the home store (the set's expansion reads it).
    gate_add(&ctx, &plane, &dir, &git, "o/r");
    // Both lines stand: the workspace feed AND the repo set.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"github.com/o/r\" = \"*\"\n"
    ));

    let err = ops::remove_global(&ctx, &connect(&plane, &dir), &["deploy".into()], None, true)
        .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{msg}");
    assert!(msg.contains("github.com/o/r/deploy"), "{msg}");
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("\"off\""), "no switch was written: {text}");
}

// =================================================================================================
// A manifest that moves between the decision and the write refuses — no plan built from bytes
// that are gone.
// =================================================================================================

#[test]
fn a_manifest_edited_between_the_decision_and_the_write_refuses() {
    // The set split is the sharpest case: its survivor rows are computed FROM the file (which
    // members already have their own row), so a row someone else adds in between would be
    // overwritten by a line rebuilt from the older reading.
    let rig = Rig::new("changed-underfoot");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
        ],
        vec![channel("backend", &[("s_a", "alpha"), ("s_b", "beta")])],
    );
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let line = format!("\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n");
    rig.write_global(&format!("[bundles]\n{line}"));

    // The racer: an outside editor writes beta's own row AFTER the arms were resolved and before
    // they are re-proven (topos's own writers serialize on the file's lock; a person's editor does
    // not).
    let racing = manifest.clone();
    let raced = format!("[bundles]\n{line}\"{HOST}/{WS_NAME}/beta\" = \"*\"\n");
    let fs = crate::fs_seam::HookFs::before_nth_read(&manifest, 2, move || {
        std::fs::write(&racing, &raced).unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let err = ops::remove_global(&ctx, &connect(&plane, &dir), &["alpha".into()], None, true)
        .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_CHANGED", "{err:?}");

    // NOTHING was written: the racer's row stands, the set line stands, no split landed.
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(text.contains("channels/backend"), "{text}");
    assert!(text.contains(&format!("{HOST}/{WS_NAME}/beta")), "{text}");
    assert!(
        !text.contains(&format!("{HOST}/{WS_NAME}/alpha")),
        "no stale survivor row was written: {text}"
    );

    // Re-run against the file as it now is: the split honours beta's own row and applies.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    assert!(matches!(
        ops::remove_global(&ctx, &connect(&plane, &dir), &["alpha".into()], None, true).unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(!text.contains("channels/backend"), "the line split: {text}");
    assert!(text.contains(&format!("{HOST}/{WS_NAME}/beta")), "{text}");
}

#[test]
fn the_reproof_and_the_editor_read_one_document() {
    // The STRUCTURAL half of the refusal above. The reproof is only worth anything if it holds for
    // the EXACT document instance the write then emits — proving against one read and editing a
    // second leaves a window where an outside edit is proven against but not edited (or edited but
    // not proven against). So the apply path reads the manifest ONCE and hands that text to both;
    // the only read after it is the editor write's own PRE-RENAME COMPARE (the CAS — proven at
    // the boundary by the test below). The witness is an absence: after the plan's read there is
    // the apply read and the compare read, never a fourth.
    let rig = Rig::new("one-read");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
        ],
        vec![channel("backend", &[("s_a", "alpha"), ("s_b", "beta")])],
    );
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n"
    ));

    // The tripwire sits on the read that would OPEN that window — the fourth. It never fires.
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&fired);
    let fs = crate::fs_seam::HookFs::before_nth_read(&manifest, 4, move || {
        flag.store(true, Ordering::Relaxed);
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let out =
        ops::remove_global(&ctx, &connect(&plane, &dir), &["alpha".into()], None, true).unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)), "{out:?}");
    assert!(
        !fired.load(Ordering::Relaxed),
        "the apply path reads the manifest ONCE for the reproof AND the editor, plus the write's \
         one pre-rename compare — a fourth read IS the window"
    );

    // And the split it proved is the split it wrote.
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(!text.contains("channels/backend"), "the line split: {text}");
    assert!(text.contains(&format!("{HOST}/{WS_NAME}/beta")), "{text}");
}

#[test]
fn an_edit_landing_at_the_write_rename_boundary_is_refused_by_the_compare_and_swap() {
    // The reproof (the test two above) closes the decision half; this closes the WRITE half. The
    // outside edit lands at the LATEST catchable instant — after the arms were re-proven and the
    // editor's document was STAGED (the temp is already on disk when the hook fires: the proof
    // this injection sits at the write/rename boundary, not earlier), immediately before the
    // pre-rename compare. The compare-and-swap must refuse: typed MANIFEST_CHANGED, the staged
    // document discarded, the outside writer's bytes untouched on disk.
    let rig = Rig::new("cas-boundary");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
        ],
        vec![channel("backend", &[("s_a", "alpha"), ("s_b", "beta")])],
    );
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let line = format!("\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n");
    rig.write_global(&format!("[bundles]\n{line}"));

    // Read 1 = the plan/arms, read 2 = the apply's one document (reproof + editor), read 3 = the
    // write's pre-rename compare. The racer fires immediately before read 3.
    let tmp = crate::atomic::temp_path(&manifest);
    let staged_when_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let staged_flag = std::sync::Arc::clone(&staged_when_fired);
    let racing = manifest.clone();
    let raced = format!("[bundles]\n{line}\"{HOST}/{WS_NAME}/beta\" = \"*\"\n");
    let tmp_probe = tmp.clone();
    let fs = crate::fs_seam::HookFs::before_nth_read(&manifest, 3, move || {
        staged_flag.store(tmp_probe.exists(), Ordering::Relaxed);
        std::fs::write(&racing, &raced).unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let err = ops::remove_global(&ctx, &connect(&plane, &dir), &["alpha".into()], None, true)
        .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_CHANGED", "{err:?}");
    assert!(
        staged_when_fired.load(Ordering::Relaxed),
        "the staged temp must already exist when the edit lands — the injection sits at the \
         write/rename boundary, after every earlier rail has passed"
    );

    // NOTHING was overwritten: the outside writer's document stands byte-for-byte, and the
    // staged temp was discarded.
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(text.contains("channels/backend"), "{text}");
    assert!(text.contains(&format!("{HOST}/{WS_NAME}/beta")), "{text}");
    assert!(!tmp.exists(), "the staged document was discarded");

    // The re-run reads the file as it now is and applies (honouring beta's own row).
    let ctx = rig.ctx_at(Some(&rig.work.0));
    assert!(matches!(
        ops::remove_global(&ctx, &connect(&plane, &dir), &["alpha".into()], None, true).unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
}

#[test]
fn a_manifest_birth_racing_an_outside_writer_refuses_manifest_exists() {
    // The BIRTH half of the outside-writer window: `add -g` on a machine with NO global file
    // stages the materialized seed, and an outside editor lands its own file at the last
    // catchable instant — after the absence check, immediately before the no-replace rename. A
    // birth is a claim the file does not exist; the exclusive create refuses typed
    // MANIFEST_EXISTS, the outside document stands byte-for-byte, and the staged seed is
    // discarded — never an overwrite.
    let rig = Rig::new("birth-race");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    assert!(!manifest.exists(), "the race needs a birth, not an edit");
    let tmp = crate::atomic::temp_path(&manifest);
    let outside = "# an outside editor's file\n[bundles]\n\"./mine\" = \"*\"\n";
    let racing = manifest.clone();
    let fs = crate::fs_seam::HookFs::before_first_move_of(&tmp, move || {
        std::fs::write(&racing, outside).unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let err = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
    ) {
        Err(e) => e,
        Ok(_) => panic!("the racing birth must refuse, never land over the outside file"),
    };
    assert_eq!(err.code(), "MANIFEST_EXISTS", "{err:?}");
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        outside,
        "the outside document stands byte-for-byte"
    );
    assert!(!tmp.exists(), "the staged birth seed was discarded");
}

// =================================================================================================
// The visited-store index unions under its own lock.
// =================================================================================================

#[test]
fn the_visited_store_index_unions_under_its_own_lock() {
    // Two sweeps from two checkouts are the ordinary case. Each one's union is built from the
    // bytes it read, so without a lock across read → union → write the second writer's document
    // simply does not contain the first writer's checkout, and that checkout's holdings drop out
    // of every later applied report.
    let rig = Rig::new("visited-lock");
    let a = project("visited-lock-a", "[bundles]\n");
    let b = project("visited-lock-b", "[bundles]\n");
    crate::sidecar::ensure_project_store(&rig.fs, &a.0).unwrap();
    crate::sidecar::ensure_project_store(&rig.fs, &b.0).unwrap();

    // Run one records checkout A.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    assert_eq!(
        crate::visited_stores::recall_and_record(&ctx, std::slice::from_ref(&a.0)).len(),
        1
    );

    // Run two records checkout B — and a would-be concurrent writer probing the lock at the moment
    // between the read and the write finds it HELD.
    let lock_path = rig.layout().visited_stores_lock_file();
    let free_in_the_window = std::cell::Cell::new(true);
    let fs =
        crate::fs_seam::HookFs::before_nth_create_dir_all(&rig.layout().state_dir(), 1, || {
            let taken = crate::fs_seam::FsOps::try_lock_exclusive(&RealFs, &lock_path)
                .map(|g| g.is_none())
                .unwrap_or(true);
            free_in_the_window.set(!taken);
        });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let layouts = crate::visited_stores::recall_and_record(&ctx, std::slice::from_ref(&b.0));
    assert!(
        !free_in_the_window.get(),
        "the index's writer lock is held across its whole read-union-write"
    );
    assert_eq!(layouts.len(), 2, "both checkouts survive the union");

    // …and the document itself holds both, for every later report.
    let doc: crate::visited_stores::VisitedStores =
        crate::doc::read_doc(&rig.fs, &rig.layout().visited_stores_path())
            .unwrap()
            .unwrap();
    assert!(doc.stores.contains(&a.0.display().to_string()), "{doc:?}");
    assert!(doc.stores.contains(&b.0.display().to_string()), "{doc:?}");
}

// =================================================================================================
// A pasted subtree URL records a SKILL row — and nothing is granted when the row would refuse.
// =================================================================================================

#[test]
fn a_subtree_url_records_a_skill_row_carrying_the_literal_path() {
    // `/tree/<ref>/<path>` canonicalizes to the REPO, and a repo-set row cannot legally carry
    // `subdir` or `version` — so the write refused AFTER the origin had been trusted. The path
    // names one skill: it records the 4-segment row whose fields take both.
    let rig = Rig::new("tree-url");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("tools/find-skills/SKILL.md", b"# find\n"),
            ("skills/other/SKILL.md", b"# other\n"),
        ],
    ));
    let url = "https://github.com/o/r/tree/main/tools/find-skills";
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        url,
        true,
        true,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap_or_else(|e| panic!("{e}: {text}"));
    let row = doc
        .rows
        .iter()
        .find(|r| r.reference == "github.com/o/r/find-skills")
        .unwrap_or_else(|| panic!("the subtree records a skill row: {text}"));
    match &row.value {
        crate::manifest::document::EntryValue::Fields(f) => {
            assert_eq!(f.subdir.as_deref(), Some("tools/find-skills"), "{text}");
        }
        other => panic!("the literal path rides the subdir field, got {other:?}: {text}"),
    }
    assert!(
        rig.home
            .0
            .join(".claude/skills/find-skills/SKILL.md")
            .exists(),
        "the selected subtree landed"
    );
}

#[test]
fn a_subtree_url_naming_several_skills_grants_nothing() {
    // The row is PROVEN before the consent is recorded: a subtree that names no single skill
    // refuses with the names, and the machine is not left trusting a source whose row never landed.
    let rig = Rig::new("tree-url-many");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n"),
            ("skills/beta/SKILL.md", b"# beta\n"),
        ],
    ));
    let err = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "https://github.com/o/r/tree/main/skills",
        true,
        true,
    ) {
        Err(e) => e,
        Ok(_) => panic!("a subtree naming several skills names none of them"),
    };
    assert_eq!(err.code(), "AMBIGUOUS_SKILL", "{err:?}");
    assert!(
        !crate::forge_trust::is_trusted(&ctx, "github.com/o/r"),
        "nothing was granted"
    );
    assert!(
        !rig.layout()
            .home()
            .join(crate::manifest::MANIFEST_FILE)
            .exists(),
        "and no file was born for a row that cannot exist"
    );
}

// =================================================================================================
// A prior placement record is a memory, not a permission.
// =================================================================================================

#[test]
fn a_prior_placement_that_no_longer_resolves_inside_the_checkout_is_refused() {
    // The record was written when `.agents` was a plain directory; the checkout has since
    // committed it as a symlink out of the tree. A lexical `starts_with` still says "inside" —
    // only the containment proof catches it, and it must catch it on REUSED paths too.
    let rig = Rig::new("prior-escape");
    let proj = project("prior-escape-proj", "[bundles]\n");
    let outside = Scratch::new("prior-escape-outside");
    std::fs::create_dir_all(outside.0.join("skills/deploy")).unwrap();
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".claude")).unwrap();
    let recorded = proj.0.join(".claude/skills/deploy");

    let prior = topos_types::persisted::PlacementMap {
        schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
        placements: vec![recorded.display().to_string()],
        applied_commit: "b".repeat(64),
        placement_state: vec![topos_types::persisted::PlacementState {
            kind: topos_types::persisted::PlacementKind::Native,
            agent: Some("claude-code".to_owned()),
            materialized_sha: Some("a".repeat(64)),
            pre_existing_sha: None,
            swap_capability: topos_types::persisted::SwapCapability::AtomicExchange,
        }],
        materialized_sha: "a".repeat(64),
        pre_existing_sha: None,
        swap_capability: topos_types::persisted::SwapCapability::AtomicExchange,
        harness: None,
        harness_layer: None,
        harness_slug: None,
    };

    let ctx = rig.ctx_at(Some(&proj.0));
    let plan = crate::placement::project_plan(
        &ctx,
        &proj.0,
        "topos_deadbeef",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: Some(WS_NAME),
        },
        None,
        Some(&prior),
        None,
    );
    assert!(
        plan.refused
            .iter()
            .any(|r| r.starts_with("PLACEMENT_ESCAPES_PROJECT")),
        "the escaping record is refused: {:?}",
        plan.refused
    );
    assert!(
        plan.targets.iter().all(|t| !t.dir.starts_with(&outside.0)),
        "the symlink is never followed: {:?}",
        plan.targets
    );
}

// =================================================================================================
// The forge refresh: under the skill's own lock, and its stash is a PARK it reads before deleting.
// =================================================================================================

#[test]
fn a_forge_refresh_holds_the_lock_and_keeps_an_edit_that_lands_at_the_stash() {
    // The refresh reads the map, classifies every placement, then moves those dirs and the sidecar
    // record aside — a sequence a second writer must not cross, and a classification that is only
    // a claim about a directory anyone could still be editing. So: the lock is held for the whole
    // replacement, and the stash is re-read AS A PARK before anything is dropped.
    let rig = Rig::new("refresh-park");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/deploy/SKILL.md", b"# deploy v1\n")],
    ));
    gate_add(&ctx, &plane, &dir, &git, "o/r");
    let placed = rig.home.0.join(".claude/skills/deploy");
    assert!(placed.join("SKILL.md").exists());
    let imports = crate::ops::forge_imports(&ctx);
    let sid = imports
        .first()
        .map(|i| i.sid.clone())
        .expect("the import is tracked");
    let recorded = std::fs::read_to_string(
        rig.layout()
            .home()
            .join("skills")
            .join(sid.as_str())
            .join("origin.json"),
    )
    .unwrap();

    // The source moved — the refresh is what this update would run.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/deploy/SKILL.md", b"# deploy v2\n")],
    ));
    let lock_path = rig.layout().lock_file(&sid);
    let lock_free = std::cell::Cell::new(true);
    let racing = placed.canonicalize().unwrap();
    let editing = racing.clone();
    let fs = crate::fs_seam::HookFs::before_first_move_of(&racing, || {
        // (a) the replacement runs under the skill's writer lock…
        let taken = crate::fs_seam::FsOps::try_lock_exclusive(&RealFs, &lock_path)
            .map(|g| g.is_none())
            .unwrap_or(true);
        lock_free.set(!taken);
        // (b) …and the person's edit lands in the last instant before the stash.
        std::fs::write(editing.join("SKILL.md"), b"# my local edit\n").unwrap();
    });
    let hooked = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let out = ops::manifest_update(
        &hooked,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();

    assert!(
        !lock_free.get(),
        "the whole refresh replacement runs under the skill's writer lock"
    );
    assert!(
        out.warnings.iter().any(|w| w.contains("was edited while")),
        "the refresh refuses and says why: {:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# my local edit\n",
        "the edit that arrived after the scan is restored, never deleted"
    );
    assert_eq!(
        std::fs::read_to_string(
            rig.layout()
                .home()
                .join("skills")
                .join(sid.as_str())
                .join("origin.json"),
        )
        .unwrap(),
        recorded,
        "the old import is intact — a refused refresh replaces nothing"
    );
}

// =================================================================================================
// A `-a` selector is a standing placement decision: the row carries it, and the update honours it.
// =================================================================================================

#[test]
fn a_selector_imports_harness_choice_rides_the_row_into_the_next_update() {
    // Without the field, the next commit move re-lands the copy through the DEFAULT agent dir and
    // the person's `-a` choice quietly evaporates.
    let rig = Rig::new("harness-row");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/deploy/SKILL.md", b"# deploy v1\n")],
    ));
    match ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &["deploy".to_owned()],
        &["codestudio".to_owned()],
        true,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(items) => assert_eq!(items.len(), 1),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    let chosen = rig.home.0.join(".codestudio/skills/deploy");
    assert!(chosen.join("SKILL.md").exists(), "the selector's dir");

    // The ROW carries the selection.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap_or_else(|e| panic!("{e}: {text}"));
    let row = doc
        .rows
        .iter()
        .find(|r| r.reference == "github.com/o/r/deploy")
        .unwrap_or_else(|| panic!("the import records its row: {text}"));
    match &row.value {
        crate::manifest::document::EntryValue::Fields(f) => assert_eq!(
            f.harness.as_deref(),
            Some(&["codestudio".to_owned()][..]),
            "{text}"
        ),
        other => panic!("the harness selection rides the row, got {other:?}: {text}"),
    }

    // The source moves: the copy converges WHERE IT WAS ASKED FOR, and no default-dir copy appears.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/deploy/SKILL.md", b"# deploy v2\n")],
    ));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        out.warnings.is_empty(),
        "a landed refresh fails nothing: {:?}",
        out.warnings
    );
    assert!(
        out.disclosures.iter().all(|w| w.starts_with("GIT_UPDATED")),
        "the moved source is disclosed and nothing else: {:?}",
        out.disclosures
    );
    assert_eq!(
        std::fs::read(chosen.join("SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the selected harness keeps the copy: {:?}",
        out.data.skills
    );
    assert!(
        !rig.home.0.join(".claude/skills/deploy").exists(),
        "and nothing was re-imported into the default agent dir"
    );
}

// =================================================================================================
// A pinned set whose imports recorded no member list is UNSETTLED — it converges once, then rests.
// =================================================================================================

#[test]
fn a_pinned_set_with_no_recorded_members_converges_on_the_next_update() {
    // "Nothing is missing" and "nothing is known" are different answers. Reading the second as the
    // first pinned a legacy-shaped import to whatever partial landing it held, forever.
    let rig = Rig::new("legacy-members");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n"),
            ("skills/beta/SKILL.md", b"# beta\n"),
        ],
    ));
    // Land ONLY alpha, then age the import into the legacy shape: no recorded member set.
    match ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &["alpha".to_owned()],
        &[],
        true,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(items) => assert_eq!(items.len(), 1),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    for import in crate::ops::forge_imports(&ctx) {
        let path = rig.layout().published(&import.sid).origin;
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc.as_object_mut().unwrap().remove("members");
        std::fs::write(&path, serde_json::to_vec_pretty(&doc).unwrap()).unwrap();
    }
    assert!(
        crate::ops::forge_imports(&ctx)
            .iter()
            .all(|i| i.members.is_empty()),
        "the legacy shape: no member list"
    );

    // The recipe is the PINNED set at exactly that commit — every tracked member satisfies the pin.
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"aaaaaaaaaaaa1\"\n");
    let before = git.fetches();

    // The explicit update refetches ONCE, records what the archive holds, and lands the gap.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        rig.home.0.join(".claude/skills/beta/SKILL.md").exists(),
        "the missing member converges: {:?} / {:?}",
        out.data.skills,
        out.warnings
    );
    assert_eq!(git.fetches(), before + 1, "one fetch");
    assert!(
        crate::ops::forge_imports(&ctx)
            .iter()
            .all(|i| i.members == vec!["alpha".to_owned(), "beta".to_owned()]),
        "the member set is written down, so the pin can rest"
    );

    // …and it RESTS: the settled pin never dials again.
    let after = git.fetches();
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert_eq!(git.fetches(), after, "a settled pin makes no forge call");
}

#[test]
fn a_pinned_whole_repo_row_keeps_its_pin_and_says_the_selection_did_not_fit() {
    // The one shape that cannot carry both: a whole-repo row takes `harness` and nothing else, so a
    // PINNED repo-root import has no legal spelling for the `-a` choice. The pin wins — it decides
    // which bytes, the selection only decides where they sit — and the receipt says so, rather than
    // writing a row the file would refuse or dropping the pin in silence.
    let rig = Rig::new("pin-vs-harness");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // A repo whose SKILL.md is at the ROOT — its row key is the repo itself.
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("SKILL.md", b"# whole repo\n")],
    ));
    let data = match ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r#aaaaaaaaaaaa1",
        &[],
        &["codestudio".to_owned()],
        true,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(mut items) => items.remove(0),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    };
    let note = data.note.unwrap_or_default();
    assert!(note.contains("not recorded"), "{note}");
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap_or_else(|e| panic!("{e}: {text}"));
    let row = doc
        .rows
        .iter()
        .find(|r| r.reference == "github.com/o/r")
        .unwrap_or_else(|| panic!("the repo row: {text}"));
    assert_eq!(
        row.value,
        crate::manifest::document::EntryValue::Pin("aaaaaaaaaaaa1".into()),
        "the pin is kept whole: {text}"
    );
}

// =================================================================================================
// The pre-1.0 legacy handover: a home-map row pointing into a checkout retires ONLY once that
// checkout's own store verifiably tracks it — and its state dir is PARKED, never deleted.
// =================================================================================================

/// Seed ONE skill into `layout`'s store: a `skills/<id>/` dir whose map records exactly
/// `placement`, as a `native` row attributed to an agent — the shape the pre-per-scope blended map
/// wrote for a copy that lives inside a project checkout.
fn seed_store_row(layout: &Layout, id: &str, placement: &std::path::Path) {
    use topos_types::persisted::{PlacementKind, PlacementMap, PlacementState, SwapCapability};
    let sid = crate::id::SkillId::parse(id).unwrap();
    std::fs::create_dir_all(layout.skill_dir(&sid)).unwrap();
    crate::doc::write_map(
        &RealFs,
        &layout.published(&sid).map,
        &PlacementMap {
            schema_version: 2,
            placements: vec![placement.to_string_lossy().into_owned()],
            applied_commit: "0".repeat(64),
            materialized_sha: "0".repeat(64),
            pre_existing_sha: None,
            swap_capability: SwapCapability::RenameDance,
            placement_state: vec![PlacementState {
                kind: PlacementKind::Native,
                agent: Some("claude-code".to_owned()),
                materialized_sha: None,
                pre_existing_sha: None,
                swap_capability: SwapCapability::RenameDance,
            }],
            harness: None,
            harness_layer: None,
            harness_slug: Some("claude-code".to_owned()),
        },
    )
    .unwrap();
}

#[test]
fn an_unadopted_project_placement_never_retires_the_home_row() {
    // CUSTODY FIRST. A home row pointing into the active checkout is legacy — but until the
    // project's OWN store records that exact placement, retiring it would leave those bytes
    // tracked by nobody. An empty (or not-yet-reconciled) project hands NOTHING over; the next
    // sweep, after the project pass adopts, does.
    let rig = Rig::new("handover-unadopted");
    let proj = project("handover-unadopted-proj", "[bundles]\n");
    let placed = proj.0.join(".claude/skills/legacy");
    std::fs::create_dir_all(&placed).unwrap();
    std::fs::write(placed.join("SKILL.md"), b"# legacy\n").unwrap();
    seed_store_row(&rig.layout(), "s_legacy", &placed);
    let ctx = rig.ctx_at(Some(&proj.0));

    let mut warnings = Vec::new();
    ops::handover_legacy_project_rows(&ctx, std::slice::from_ref(&proj.0), &mut warnings);

    assert!(warnings.is_empty(), "nothing to disclose: {warnings:?}");
    let sid = crate::id::SkillId::parse("s_legacy").unwrap();
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .expect("the home map is still there");
    assert_eq!(map.placements.len(), 1, "the row stands: {map:?}");
    assert!(
        rig.layout().skill_dir(&sid).is_dir(),
        "and so does its state dir"
    );
    assert!(
        !rig.layout()
            .skills_dir()
            .join(".topos-handover-s_legacy")
            .exists(),
        "nothing was parked"
    );
}

#[test]
fn an_adopted_project_placement_hands_over_and_parks_the_home_store() {
    // Custody established: the project store records that EXACT placement, so the home row lets
    // go. What it leaves behind is not deleted — the embedded history and draft snapshots under it
    // were never carried into the project's fresh baseline, so the dir is PARKED to a
    // `.topos-handover-*` sibling and NAMED on a warning; a person deletes it deliberately.
    let rig = Rig::new("handover-adopted");
    let proj = project("handover-adopted-proj", "[bundles]\n");
    let placed = proj.0.join(".claude/skills/legacy");
    std::fs::create_dir_all(&placed).unwrap();
    std::fs::write(placed.join("SKILL.md"), b"# legacy\n").unwrap();
    seed_store_row(&rig.layout(), "s_legacy", &placed);
    // The ADOPTION WITNESS: the checkout's own store tracks a skill recording the same path.
    let playout = crate::sidecar::ensure_project_store(&rig.fs, &proj.0).unwrap();
    seed_store_row(&playout, "s_proj", &placed);
    let ctx = rig.ctx_at(Some(&proj.0));

    let mut warnings = Vec::new();
    ops::handover_legacy_project_rows(&ctx, std::slice::from_ref(&proj.0), &mut warnings);

    let sid = crate::id::SkillId::parse("s_legacy").unwrap();
    let parked = rig.layout().skills_dir().join(".topos-handover-s_legacy");
    assert!(
        !rig.layout().skill_dir(&sid).exists(),
        "the home entry let go"
    );
    assert!(parked.is_dir(), "…by PARKING it: {}", parked.display());
    let kept = crate::doc::read_map(&rig.fs, &parked.join("map.json"))
        .unwrap()
        .expect("the parked store's bytes are intact");
    assert_eq!(kept.placements.len(), 1, "{kept:?}");
    let line = warnings
        .iter()
        .find(|w| w.starts_with("STATE_HANDOVER"))
        .unwrap_or_else(|| panic!("the disclosure line: {warnings:?}"));
    assert!(line.contains(&parked.display().to_string()), "{line}");
    // The bytes in the checkout were never the subject — they stay exactly where they are.
    assert!(placed.join("SKILL.md").exists());
}

// =================================================================================================
// The BARE-NAME ladder: one namespace, made of the local inventory AND the connected catalogs.
// =================================================================================================

/// An untracked skill a discovery walk will find: `<root>/.claude/skills/<name>/SKILL.md` (the
/// rig's home already carries `.claude/`, so claude-code reads as present at both scopes).
fn untracked_skill(root: &std::path::Path, name: &str, body: &[u8]) -> PathBuf {
    let dir = root.join(".claude/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
    dir
}

/// Plan a bare `add <target>` exactly as the composition root does: the machine's discovery roots
/// (home + the working directory) against every connected session's catalog.
fn bare_plan(
    rig: &Rig,
    plane: &FakePlane,
    dir: &FakeDirectory,
    target: &str,
    subscribe: bool,
) -> Result<ops::BareAddPlan, ClientError> {
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(rig.work.0.clone()),
    };
    ops::plan_bare_add(&ctx, &connect(plane, dir), &roots, target, subscribe)
}

/// A SECOND connected workspace, on its own server — what makes a bare name ambiguous across
/// teams rather than across harnesses.
fn seed_second_session(rig: &Rig) {
    sessions::upsert_session(
        &rig.fs,
        &rig.layout(),
        Session {
            host: "beta.test".into(),
            base_url: "https://beta.test/api".into(),
            workspace_id: "w_ops".into(),
            workspace_name: "ops".into(),
            display_name: "Operations".into(),
            session_id: "sn_2".into(),
            credential: "cred-2".into(),
            status: SESSION_ACTIVE.into(),
            logged_in_at: 1,
        },
    )
    .unwrap();
}

#[test]
fn a_name_only_a_workspace_publishes_resolves_to_its_reference() {
    let (rig, plane, dir, _v) = add_rig("bare-ws-only");
    match bare_plan(&rig, &plane, &dir, "deploy", true).unwrap() {
        ops::BareAddPlan::Subscribe {
            reference,
            workspace,
        } => {
            // The CANONICAL host-qualified spelling — unambiguous however many servers this
            // machine is logged into.
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/deploy"));
            assert_eq!(workspace, WS_NAME);
        }
        other => panic!("the workspace's copy is the only thing that name can mean: {other:?}"),
    }
    // A name NOBODY has stays the plain not-found — the workspace half never invents a match.
    let err = bare_plan(&rig, &plane, &dir, "ghost", true).unwrap_err();
    assert_eq!(err.code(), "NO_UNTRACKED_SKILL");
}

#[test]
fn a_local_directory_wins_and_the_receipt_names_the_workspace_spelling() {
    let (rig, plane, dir, _v) = add_rig("bare-both");
    // The SAME bytes the workspace serves, sitting untracked in the home agent dir.
    let path = untracked_skill(&rig.home.0, "deploy", b"# deploy\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let published = match bare_plan(&rig, &plane, &dir, "deploy", true).unwrap() {
        ops::BareAddPlan::Adopt {
            path: p,
            name,
            published,
        } => {
            assert_eq!(p, path, "the bytes in front of you are what you asked for");
            assert_eq!(name, "deploy");
            published.expect("the one workspace publishing the name is disclosed")
        }
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    // The receipt judges the workspace's current version against the bytes that just landed.
    let data = ops::add_with_name(&ctx, &path, Some("deploy")).unwrap();
    let same = published.suggestion(&data.bundle_digest);
    assert_eq!(same.reference, format!("{HOST}/{WS_NAME}/deploy"));
    assert_eq!(same.workspace, WS_NAME);
    assert!(same.identical, "byte-identical to what the catalog serves");

    // A local copy that has DRIFTED from the team's version is disclosed just the same — only the
    // identical claim goes away.
    let rig2 = Rig::new("bare-both-drift");
    rig2.seed_session();
    let drifted = untracked_skill(&rig2.home.0, "deploy", b"# deploy (mine)\n");
    let ctx2 = rig2.ctx_at(Some(&rig2.work.0));
    let published2 = match bare_plan(&rig2, &plane, &dir, "deploy", true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => published.expect("still disclosed"),
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    let data2 = ops::add_with_name(&ctx2, &drifted, Some("deploy")).unwrap();
    assert!(!published2.suggestion(&data2.bundle_digest).identical);
}

#[test]
fn a_name_several_workspaces_publish_refuses_naming_every_spelling() {
    let (rig, plane, dir, _v) = add_rig("bare-two-ws");
    seed_second_session(&rig);

    let err = bare_plan(&rig, &plane, &dir, "deploy", true).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_WORKSPACE");
    let message = err.to_string();
    for reference in [
        format!("{HOST}/{WS_NAME}/deploy"),
        "beta.test/ops/deploy".to_owned(),
    ] {
        assert!(message.contains(&reference), "{message}");
    }
    // The machine-readable half: one runnable subscribe per spelling, and the references in `data`.
    let envelope = crate::render::err_envelope("add", &err);
    assert_eq!(
        envelope.next_actions.len(),
        2,
        "{:?}",
        envelope.next_actions
    );
    assert_eq!(
        envelope.data["references"],
        serde_json::json!([format!("{HOST}/{WS_NAME}/deploy"), "beta.test/ops/deploy"]),
        "sorted by the spelling the message prints"
    );

    // With a LOCAL copy in hand the adopt still wins — and carries no suggestion, because naming
    // one of two teams' copies beside an adopt that already landed would be a guess.
    untracked_skill(&rig.home.0, "deploy", b"# deploy\n");
    match bare_plan(&rig, &plane, &dir, "deploy", true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => assert!(published.is_none()),
        other => panic!("a local copy adopts in place: {other:?}"),
    }
}

#[test]
fn a_local_ambiguity_discloses_the_team_copy_and_judges_the_bytes() {
    let (rig, plane, dir, _v) = add_rig("bare-scope");
    // The same name in the home AND project dirs of ONE harness — `@harness` cannot split them.
    untracked_skill(&rig.home.0, "deploy", b"# deploy\n");
    untracked_skill(&rig.work.0, "deploy", b"# deploy\n");

    let err = bare_plan(&rig, &plane, &dir, "deploy", true).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_SCOPE");
    let message = err.to_string();
    assert!(
        message.contains("byte-identical") && message.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "every copy matches the served version, so the message collapses toward the subscribe: \
         {message}"
    );
    // The subscribe rides beside the inventory read as a runnable action.
    let envelope = crate::render::err_envelope("add", &err);
    assert_eq!(
        envelope.next_actions.last().map(|a| a.argv.clone()),
        Some(vec![
            "topos".to_owned(),
            "add".to_owned(),
            format!("{HOST}/{WS_NAME}/deploy"),
            "--json".to_owned(),
        ]),
        "{:?}",
        envelope.next_actions
    );

    // One copy DRIFTS: the disclosure stands, the identical claim does not.
    untracked_skill(&rig.work.0, "deploy", b"# deploy (mine)\n");
    let message = bare_plan(&rig, &plane, &dir, "deploy", true)
        .unwrap_err()
        .to_string();
    assert!(!message.contains("byte-identical"), "{message}");
    assert!(message.contains("is also published in"), "{message}");
}

#[test]
fn a_selector_or_harness_form_never_subscribes() {
    let (rig, plane, dir, _v) = add_rig("bare-gated");
    // `-s`/`-a` narrow a LOCAL adopt, so the fully-bare gate is closed: today's answer, exactly.
    let err = bare_plan(&rig, &plane, &dir, "deploy", false).unwrap_err();
    assert_eq!(err.code(), "NO_UNTRACKED_SKILL");
    // Same for a second workspace publishing it — the gate is about the FORM, not the count.
    seed_second_session(&rig);
    assert_eq!(
        bare_plan(&rig, &plane, &dir, "deploy", false)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );
    // A `@harness` suffix names a local harness's dir — it cannot reach the subscribe arm at all.
    let err = bare_plan(&rig, &plane, &dir, "deploy@claude-code", true).unwrap_err();
    assert_eq!(err.code(), "HARNESS_NOT_FOUND");
}

#[test]
fn a_cache_only_match_subscribes_and_an_unanswering_session_is_skipped() {
    let (rig, plane, dir, _v) = add_rig("bare-offline");
    // The catalog cannot be read at all — a transport fault, never an existence claim.
    dir.set_unavailable(true);
    assert_eq!(
        bare_plan(&rig, &plane, &dir, "deploy", true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL",
        "an unreachable directory answers nothing; it never fabricates a match"
    );

    // The offline delivery cache remembers what this workspace last delivered here — enough to
    // spell the reference, never enough to claim the bytes agree.
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                delivered: [(
                    "s_deploy".to_owned(),
                    sync_status::DeliveredSkill {
                        name: "deploy".to_owned(),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )],
    )
    .unwrap();
    match bare_plan(&rig, &plane, &dir, "deploy", true).unwrap() {
        ops::BareAddPlan::Subscribe { reference, .. } => {
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/deploy"));
        }
        other => panic!("the cache alone is enough to spell the reference: {other:?}"),
    }
    // And with a local copy beside it, the disclosure carries no identical claim: a cached row
    // holds a served VERSION id, which is a different hash from a bundle digest.
    let path = untracked_skill(&rig.home.0, "deploy", b"# deploy\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let published = match bare_plan(&rig, &plane, &dir, "deploy", true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => published.expect("disclosed"),
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    let data = ops::add_with_name(&ctx, &path, Some("deploy")).unwrap();
    assert!(!published.suggestion(&data.bundle_digest).identical);
}

#[test]
fn a_bare_name_subscribe_records_the_canonical_row_and_its_inverse() {
    let (rig, plane, dir, _v) = add_rig("bare-e2e");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let ops::BareAddPlan::Subscribe {
        reference,
        workspace,
    } = bare_plan(&rig, &plane, &dir, "deploy", true).unwrap()
    else {
        panic!("nothing local carries the name");
    };

    // The composition root's own hand-off: the resolved reference goes through the ORDINARY
    // reference arm, so the row, the delivery, and the receipt shape are the spelled-out ones.
    let mut data =
        match ops::add_reference(&ctx, &connect(&plane, &dir), None, &reference, false, false)
            .unwrap()
        {
            ops::AddRefOutcome::Applied(d) => *d,
            ops::AddRefOutcome::Described { .. } => {
                panic!("a workspace reference applies immediately")
            }
        };
    ops::push_note(
        &mut data,
        format!("resolved 'deploy' to {reference} — {workspace} publishes it"),
    );

    let manifest = rig.work.0.join(crate::manifest::MANIFEST_FILE);
    assert_eq!(data.manifest.as_deref(), Some(manifest.to_str().unwrap()));
    assert_eq!(data.reference.as_deref(), Some(reference.as_str()));
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(text.contains(&format!("\"{reference}\"")), "{text}");
    assert_eq!(
        data.undo,
        vec!["topos".to_owned(), "remove".to_owned(), reference.clone()],
        "the inverse is the project-scope remove of the same key"
    );
    let note = data.note.expect("the resolution is disclosed");
    assert!(
        note.contains("deploy") && note.contains(&reference),
        "{note}"
    );
}
