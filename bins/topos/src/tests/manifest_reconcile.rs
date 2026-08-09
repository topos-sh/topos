//! The RECONCILE over fakes (no HTTP): two UNBLENDED scopes, each converged on its own recipe.
//!
//! The global FILE is the machine's complete recipe (only its rows deliver — a feed row, written
//! by login on the first connection, is what makes a workspace's feed flow; with no file nothing
//! is demanded machine-wide, and it says so loudly when a workspace's feed is left out); an
//! `"off"` row withholds exactly its bundle; an explicit row's pin beats the feed's and a set's
//! version of the same identity; the NEAREST project file governs whole; the same bundle at both
//! scopes lands twice with two stores; git rows move only on an explicit update; a dropped row
//! cleans snapshot-first; and `--force` absorbs before it drops.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::Commit;
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireMe, WireProposalIndex,
    WireSkillIndex, WireSkillIndexEntry, WireSkillLog,
};
use topos_types::results::{ExchangeFault, PullAction};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::{ClientError, FetchFault};
use crate::fs_seam::{FsOps, RealFs};
use crate::git_source::RepoHead;
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
            progress: crate::progress::silent(),
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
    /// The ordinary machine recipe after a login: the seeded session's feed row — exactly the
    /// line `topos login` writes on this machine's first connection to the workspace.
    fn seed_feed(&self) {
        self.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
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

/// One reported applied row as the fake records it: `(skill_id, version hex, mcp states)`.
type ReportedRow = (String, String, Vec<topos_types::results::McpAgentState>);

/// The per-session plane fake: a delivery script + versions keyed by `(skill, version-hex)`.
#[derive(Clone)]
struct FakePlane {
    delivery: Arc<Mutex<Result<DeliverySnapshot, &'static str>>>,
    versions: HashMap<(String, String), FetchedVersion>,
    log: CallLog,
    /// The LAST applied report's rows, `(skill_id, version hex)` — the wire carries exactly one
    /// row per (session, bundle), so this is what the cross-store pick actually reported —
    /// `(skill_id, version hex, per-agent mcp states)`.
    reported: Arc<Mutex<Vec<ReportedRow>>>,
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
        kind: "skill".into(),
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
    fn report_applied(
        &self,
        _ws: &str,
        applied: &[crate::plane::AppliedSkillReport],
    ) -> Result<(), PlaneError> {
        *self.reported.lock().unwrap() = applied
            .iter()
            .map(|r| {
                (
                    r.skill_id.clone(),
                    topos_core::digest::to_hex(&r.version_id),
                    r.harnesses.clone(),
                )
            })
            .collect();
        let mut ids: Vec<&str> = applied.iter().map(|r| r.skill_id.as_str()).collect();
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
pub(super) struct FakeDirectory {
    skills: Vec<WireSkillIndexEntry>,
    channels: Vec<WireChannelEntry>,
    /// When set, the index reads fail (a transport fault) — the freeze suites flip it.
    unavailable: Arc<Mutex<bool>>,
}

impl FakeDirectory {
    pub(super) fn new(skills: Vec<WireSkillIndexEntry>, channels: Vec<WireChannelEntry>) -> Self {
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
        mcp_server_name: None,
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
pub(super) struct NoGovernance;
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

/// A bare hand-run `topos update` (no forge lane — the background posture): the scope rule applies,
/// so it converges the PROJECT when a manifest covers the cwd and the MACHINE otherwise.
fn sweep(ctx: &Ctx<'_>, plane: &FakePlane, dir: &FakeDirectory) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap()
}

/// The reconcile under an explicit scope selector — `-g` (machine), or the hook sweep's BOTH.
fn sweep_scoped(
    ctx: &Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    scope: ops::UpdateScope,
) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        None,
        &ops::ManifestUpdateOpts {
            scope,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap()
}

/// The BACKGROUND sweep (`update --quiet`, the auto-update trigger): both scopes, always — silent
/// delivery is the promise, so it may never narrow to the folder the session happened to start in.
fn sweep_both(ctx: &Ctx<'_>, plane: &FakePlane, dir: &FakeDirectory) -> ops::PullOutcome {
    sweep_scoped(ctx, plane, dir, ops::UpdateScope::Both)
}

/// THE HOOK'S ACTUAL POSTURE — what `topos update --quiet` really runs: both scopes, the forge
/// lane present, and the forge dialed only when its own clock says the interval has elapsed.
fn quiet_sweep(
    ctx: &Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    git: &FakeGit,
) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        Some(git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Both,
            forge: ops::ForgeCadence::Scheduled,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap()
}

/// A hand-run `topos update` WITH the forge lane — the person-typed posture, which never waits for
/// the clock.
fn update_now(
    ctx: &Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    git: &FakeGit,
) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        Some(git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap()
}

/// Wind EVERY recorded question's clock back so the next scheduled sweep is due — the
/// injected-clock equivalent of waiting out the interval (the rig's clock is fixed, so the DUE
/// TIMES are what move).
fn forge_interval_elapsed(rig: &Rig) {
    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    let outcomes: Vec<(String, crate::forge_check::SourceCheck)> = doc
        .sources
        .iter()
        .map(|(k, v)| {
            let mut v = v.clone();
            for at in v.next_check_at.values_mut() {
                *at = rig_now(rig);
            }
            (k.clone(), v)
        })
        .collect();
    crate::forge_check::record_round(&rig.fs, &rig.layout(), None, &outcomes).unwrap();
}

/// When ONE scope's turn at a question next falls due (0 = it has never taken one, i.e. "now").
fn forge_next_due(rig: &Rig, source: &str, git_ref: &str, scope: &str) -> i64 {
    crate::forge_check::read(&rig.fs, &rig.layout())
        .sources
        .get(&crate::forge_check::question(source, git_ref))
        .and_then(|c| c.next_check_at.get(scope).copied())
        .unwrap_or(0)
}

// =================================================================================================
// The person scope: the complete file (and the nothing an absent file demands).
// =================================================================================================

#[test]
fn the_feed_row_adopts_the_workspaces_feed() {
    let rig = Rig::new("feedrow");
    rig.seed_session();
    rig.seed_feed();
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

    // The feed row (the line login wrote on the first connection) takes the workspace's whole
    // feed — installed silently (the login was the acceptance), into the home dirs.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    // The FIRST materialization reads `installed`, leads with the workspace-qualified name, and
    // names its destinations — never an agent.
    assert_eq!(row.action, PullAction::Installed, "{:?}", out.warnings);
    assert_eq!(row.scope.as_deref(), Some("person"));
    assert_eq!(
        row.display.as_deref(),
        Some(&format!("@{WS_NAME}/deploy")[..])
    );
    assert_eq!(row.destinations.len(), 1, "{:?}", row.destinations);
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
    // Nothing is loud: the feed row adopts everything, so there is nothing to disclose.
    assert!(
        !out.advisories
            .iter()
            .any(|w| w.starts_with("GLOBAL_MANIFEST")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn no_global_file_delivers_nothing_and_cleans_what_was_delivered() {
    // The old contract ("no file behaves as one feed row per connected workspace") is DEAD: with
    // no file nothing is demanded machine-wide — nothing lands, and a copy an earlier recipe
    // placed retires exactly as a deleted feed row's would (bytes kept: the absence is this
    // machine's own choice, not a feed withdrawal).
    let rig = Rig::new("nofile");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Delivered while the feed row stood…
    rig.seed_feed();
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.exists());

    // …then the whole file is deleted. The next sweep delivers nothing and RETIRES the
    // placement — cleanup semantics do not depend on the file's existence.
    std::fs::remove_file(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.data
            .skills
            .iter()
            .any(|s| s.skill == "deploy" && !matches!(s.action, PullAction::Removed)),
        "nothing is delivered person-scope with no file: {:?}",
        out.data.skills
    );
    assert!(
        !placed.exists(),
        "the placement retired: {:?}",
        out.warnings
    );
    // The bytes stay — this machine's own choice keeps them, like an `"off"` row's clean.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(rig.layout().skill_dir(&sid).exists());

    // A machine that never had the file delivers nothing at all.
    let fresh = Rig::new("nofile-fresh");
    fresh.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = fresh.ctx_at(Some(&fresh.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !fresh.work.0.join("skills/deploy").exists(),
        "{:?}",
        out.data.skills
    );
    assert!(
        out.data.skills.is_empty(),
        "nothing demanded, nothing moved: {:?}",
        out.data.skills
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
        .advisories
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
    rig.seed_feed();
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
/// One feed row names an ADDRESS, and a workspace whose id was re-minted (the old session row
/// surviving beside the new one) has two live sessions on that address — the row drives the feed
/// reconcile once per session, and the receipt must still carry exactly one line.
#[test]
fn one_address_adopted_twice_earns_one_empty_exchange_line() {
    let rig = Rig::new("emptytwice");
    rig.seed_session();
    rig.seed_feed();
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
        !out.advisories
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
        .advisories
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
    assert_eq!(row.action, PullAction::Installed, "{:?}", out.warnings);
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

/// Both scopes at once is the BACKGROUND sweep's shape (a hand-run update converges only where it
/// stands), so the unblended property is proven through it.
#[test]
fn the_same_bundle_at_both_scopes_lands_twice() {
    let rig = Rig::new("unblended");
    rig.seed_session();
    rig.seed_feed();
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
    let out = sweep_both(&ctx, &plane, &dir);
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
    let out = sweep_both(&ctx, &plane, &dir);
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

/// A manifest the run would DRIVE that fails to load refuses the run WHOLE — never a
/// success-claiming receipt over a no-op. The refusal is the MANIFEST family (verbatim message
/// naming the file; the TTY closes with `nothing changed`), and the bytes stay: the failure mode
/// of a mistake must be keeping bytes.
#[test]
fn a_driven_manifest_that_fails_to_load_refuses_the_run_whole() {
    let rig = Rig::new("badfile");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.exists());

    // A typo the grammar refuses: the driven scope's recipe is unreadable, so the run refuses —
    // no receipt, no partial sweep claims, and nothing cleaned.
    rig.write_global("[bundles]\n\"not a reference\" = \"*\"\n");
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("topos.toml"),
        "the refusal names the file: {msg}"
    );
    let tty = crate::render::err_tty(&err);
    assert_eq!(tty.lines().last(), Some("nothing changed"), "{tty}");
    assert!(
        !tty.contains("updated machine-wide"),
        "a refusal never claims a sweep: {tty}"
    );
    assert!(placed.exists(), "a refused run never cleans");
}

/// A broken manifest in a scope the run does NOT drive never blocks it: the failure degrades to
/// the freeze warning, the driven scope still converges, and the frozen scope's bytes stay.
#[test]
fn a_broken_manifest_in_an_undriven_scope_warns_and_freezes_it() {
    let rig = Rig::new("badfile-undriven");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let api = one_file(b"# api\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v)
        .with_version("s_api", &api);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &v),
            catalog_entry("s_api", "api", &api),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.exists());

    // Break the GLOBAL file, then drive the PROJECT scope only: the machine scope is not this
    // run's to claim, so its fault is a warning and its bytes freeze in place.
    rig.write_global("[bundles]\n\"not a reference\" = \"*\"\n");
    let proj = project(
        "badfile-undriven-proj",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/api\" = \"*\"\n"),
    );
    let pctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&pctx, &plane, &dir);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("MANIFEST_INVALID")),
        "{:?}",
        out.warnings
    );
    assert!(
        proj.0.join(".claude/skills/api/SKILL.md").exists(),
        "the driven project scope still converged"
    );
    assert!(placed.exists(), "a frozen scope never cleans");
}

// =================================================================================================
// The scope rule: an `update` converges where the invocation STANDS (`-g` = the machine); only the
// background hook sweep covers both, because silent delivery may never narrow to one folder.
// =================================================================================================

/// Two populated scopes on one machine: the project file demands `api`, and the connected feed
/// delivers `deploy`. Every test below asserts WHICH of the two a given invocation converged.
struct TwoScopes {
    proj: Scratch,
    plane: FakePlane,
    dir: FakeDirectory,
}

fn two_scopes(tag: &str) -> TwoScopes {
    let api = one_file(b"# api\n");
    let deploy = one_file(b"# deploy\n");
    let proj = project(
        tag,
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/api\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_api", &api)
        .with_version("s_deploy", &deploy);
    // The FEED carries `deploy` alone — `api` is the project row's demand and nothing else's.
    plane.serves(vec![delivered("s_deploy", "deploy", &deploy)]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_api", "api", &api),
            catalog_entry("s_deploy", "deploy", &deploy),
        ],
        Vec::new(),
    );
    TwoScopes { proj, plane, dir }
}

#[test]
fn a_bare_update_inside_a_project_converges_only_that_project() {
    let rig = Rig::new("scope-here-proj");
    rig.seed_session();
    rig.seed_feed();
    let t = two_scopes("scope-here-proj");
    let ctx = rig.ctx_at(Some(&t.proj.0));
    let out = sweep(&ctx, &t.plane, &t.dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    // The folder's own demand landed, inside the checkout, and the receipt names the scope.
    assert!(t.proj.0.join(".claude/skills/api/SKILL.md").exists());
    assert_eq!(
        out.data.scope,
        Some(format!("project {}", t.proj.0.display()))
    );
    // The machine scope was never DRIVEN: no bytes in the home agent dirs, no home store entry.
    assert!(
        !rig.work.0.join("skills").exists(),
        "the feed's copy stayed away from the machine's agent dirs"
    );
    let deploy_id = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(
        !rig.layout().skill_dir(&deploy_id).exists(),
        "the home store gained no entry"
    );
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "deploy"),
        "{:?}",
        out.data.skills
    );
    // The machine's demand is PENDING, not dropped — the machine-scoped run still delivers it.
    let out = sweep_scoped(&ctx, &t.plane, &t.dir, ops::UpdateScope::Machine);
    assert!(
        rig.work.0.join("skills/deploy/SKILL.md").exists(),
        "{:?}",
        out.warnings
    );
}

#[test]
fn a_bare_update_outside_a_project_converges_the_machine() {
    let rig = Rig::new("scope-here-machine");
    rig.seed_session();
    rig.seed_feed();
    let t = two_scopes("scope-here-machine");
    // Standing in the plain work dir — no `topos.toml` covers it, so the machine is where you are.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &t.plane, &t.dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    assert_eq!(out.data.scope.as_deref(), Some("machine"));
    assert!(rig.work.0.join("skills/deploy/SKILL.md").exists());
    // A project the invocation never stood in is not reconciled from outside it.
    assert!(!t.proj.0.join(".claude/skills/api").exists());
    assert!(crate::sidecar::existing_project_store(&rig.fs, &t.proj.0).is_none());
}

#[test]
fn update_g_inside_a_project_converges_only_the_machine() {
    let rig = Rig::new("scope-global");
    rig.seed_session();
    rig.seed_feed();
    let t = two_scopes("scope-global");
    let ctx = rig.ctx_at(Some(&t.proj.0));
    let out = sweep_scoped(&ctx, &t.plane, &t.dir, ops::UpdateScope::Machine);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    assert_eq!(out.data.scope.as_deref(), Some("machine"));
    assert!(rig.work.0.join("skills/deploy/SKILL.md").exists());
    // The checkout the invocation was standing in is untouched: no bytes, and no store minted.
    assert!(
        !t.proj.0.join(".claude/skills/api").exists(),
        "{:?}",
        out.data.skills
    );
    assert!(crate::sidecar::existing_project_store(&rig.fs, &t.proj.0).is_none());
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "api"),
        "{:?}",
        out.data.skills
    );
}

/// The property the hook sweep exists for: `update --quiet` fires on a session start in SOME
/// folder, and everything the machine holds must still converge — silent delivery is the promise,
/// so the folder the agent happened to open may never narrow what auto-update reaches.
#[test]
fn the_hook_sweep_converges_both_scopes_from_inside_a_project() {
    let rig = Rig::new("scope-both");
    rig.seed_session();
    rig.seed_feed();
    let t = two_scopes("scope-both");
    let ctx = rig.ctx_at(Some(&t.proj.0));
    let out = sweep_both(&ctx, &t.plane, &t.dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    assert_eq!(out.data.scope.as_deref(), Some("both"));
    assert!(
        t.proj.0.join(".claude/skills/api/SKILL.md").exists(),
        "the project scope converged"
    );
    assert!(
        rig.work.0.join("skills/deploy/SKILL.md").exists(),
        "the machine scope converged in the same run"
    );
}

/// A project file the grammar REFUSES still covers the folder — so a bare `update` standing in
/// it DRIVES that scope, and the broken file refuses the run whole. Falling back to the machine
/// would answer a typo by converging a tree nobody asked about — and would land bytes the person
/// is at that moment being told their manifest is broken.
#[test]
fn a_frozen_project_manifest_refuses_and_never_falls_back_to_the_machine() {
    let rig = Rig::new("scope-frozen");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let deploy = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &deploy);
    plane.serves(vec![delivered("s_deploy", "deploy", &deploy)]);
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &deploy)],
        Vec::new(),
    );
    let proj = project(
        "scope-frozen-proj",
        "[bundles]\n\"not a reference\" = \"*\"\n",
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap_err();

    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains(&proj.0.display().to_string()),
        "the refusal names the file this run stood under: {msg}"
    );
    assert_eq!(
        crate::render::err_tty(&err).lines().last(),
        Some("nothing changed")
    );
    assert!(
        !rig.work.0.join("skills").exists(),
        "a broken project file never hands the run to the machine"
    );
}

/// `update -g` over a global file spelling a RETIRED placement field refuses with the exact
/// rewrite teaching — nonzero, `nothing changed`, and NO success-claiming receipt — instead of
/// the old downgrade (a `MANIFEST_INVALID` warning under an "updated machine-wide" summary that
/// claimed a sweep which never read the file).
#[test]
fn update_g_over_a_retired_spelling_file_refuses_with_the_rewrite() {
    let rig = Rig::new("retired-refuses");
    rig.seed_session();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ path = \"~/.claude/skills\" }}\n"
    ));
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Machine,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_FIELD_RETIRED");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("is now written as dest — use dest = [\"~/.claude/skills\"]"),
        "the exact rewrite teaching reaches the surfaces verbatim: {msg}"
    );
    assert!(msg.contains("topos.toml"), "the file is named: {msg}");
    let tty = crate::render::err_tty(&err);
    assert_eq!(tty.lines().last(), Some("nothing changed"), "{tty}");
    assert!(!tty.contains("updated machine-wide"), "{tty}");
}

/// The dest DIALECT faults refuse as MANIFEST refusals in both directions — never as
/// CORRUPT_STATE, whose fixed TTY line blames topos's own state for a hand-edit and buries the
/// real teaching in the log. The message names the file, the offending entry, and the rule, and
/// the run they would drive refuses whole.
#[test]
fn a_dest_dialect_fault_refuses_as_a_manifest_refusal_not_corrupt_state() {
    // Machine direction: a bare-relative entry in the machine-wide file.
    let rig = Rig::new("dialect-machine");
    rig.seed_session();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"skills\"] }}\n"
    ));
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Machine,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("dest entry `skills` is relative — the machine-wide file names machine"),
        "the rule reaches the surfaces verbatim, not just log.jsonl: {msg}"
    );
    assert!(msg.contains("topos.toml"), "the file is named: {msg}");
    assert_eq!(
        crate::render::err_tty(&err).lines().last(),
        Some("nothing changed")
    );

    // Project direction: an absolute (checkout-escaping) entry in a project file.
    let rig = Rig::new("dialect-project");
    rig.seed_session();
    let proj = project(
        "dialect-project-proj",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/api\" = {{ dest = [\"/abs\"] }}\n"),
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("dest entry `/abs` leaves the checkout"),
        "{msg}"
    );
    assert!(
        msg.contains(&proj.0.display().to_string()),
        "the project file is named: {msg}"
    );
}

/// The QUIET sweep must not claim success over a manifest it could not read either: the refusal
/// is a HARD failure (never the auth/transport soft-skip that exits 0), so the hook surfaces a
/// nonzero exit instead of a silent success stamp.
#[test]
fn the_quiet_sweep_refuses_a_broken_manifest_hard() {
    let rig = Rig::new("quiet-refuses");
    rig.seed_session();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ path = \"~/.claude/skills\" }}\n"
    ));
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Both,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_FIELD_RETIRED");
    assert!(
        !ops::quiet_soft_failure(&err),
        "a broken manifest is a local fault — the hook path must exit nonzero, not warn-and-0"
    );
}

/// A named target narrows WITHIN the driven scope — so a name that is perfectly real one scope
/// over is a miss, and the refusal has to say which scope it searched rather than claim the name
/// is nowhere (which would send someone to re-add what they already have).
#[test]
fn a_target_from_the_other_scope_refuses_naming_the_scope_it_searched() {
    let rig = Rig::new("scope-target");
    rig.seed_session();
    let t = two_scopes("scope-target");

    // Standing in the project: `deploy` is the FEED's, and the feed is not this folder's scope.
    let ctx = rig.ctx_at(Some(&t.proj.0));
    let refused = ops::manifest_update(
        &ctx,
        &connect(&t.plane, &t.dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["deploy".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    );
    let Err(err) = refused else {
        panic!("a target from the other scope must refuse");
    };
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err}");
    let msg = err.to_string();
    assert!(
        msg.contains(&format!(
            "'deploy' is not demanded by {}/topos.toml",
            t.proj.0.display()
        )),
        "{msg}"
    );
    assert!(msg.contains("`topos update -g deploy`"), "{msg}");
    assert!(
        !rig.work.0.join("skills").exists(),
        "the refusal moved no bytes: {msg}"
    );

    // The mirror image, standing outside the project: `api` is the project file's demand.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let refused = ops::manifest_update(
        &ctx,
        &connect(&t.plane, &t.dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["api".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    );
    let Err(err) = refused else {
        panic!("a target from the other scope must refuse");
    };
    let msg = err.to_string();
    assert!(
        msg.contains("'api' is not in your machine-wide set"),
        "{msg}"
    );
    assert!(msg.contains("`topos add -g api`"), "{msg}");
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

/// The forge fake: ONE archive at a time, separate counters for the two transport calls (the
/// "never dialed" and "never downloaded" witnesses), and an injectable fault so a test can make
/// the forge fail exactly the way it wants to.
#[derive(Clone)]
struct FakeGit {
    archive: Arc<Mutex<Vec<u8>>>,
    fetches: Arc<Mutex<u32>>,
    probes: Arc<Mutex<u32>>,
    /// What every call answers with instead of the archive, once set.
    fault: Arc<Mutex<Option<FetchFault>>>,
    /// What the probe reports the repo has been renamed to.
    renamed: Arc<Mutex<Option<(String, String)>>>,
}
impl FakeGit {
    fn new(targz: Vec<u8>) -> Self {
        Self {
            archive: Arc::new(Mutex::new(targz)),
            fetches: Arc::new(Mutex::new(0)),
            probes: Arc::new(Mutex::new(0)),
            fault: Arc::new(Mutex::new(None)),
            renamed: Arc::new(Mutex::new(None)),
        }
    }
    fn serve(&self, targz: Vec<u8>) {
        *self.archive.lock().unwrap() = targz;
    }
    fn fetches(&self) -> u32 {
        *self.fetches.lock().unwrap()
    }
    fn probes(&self) -> u32 {
        *self.probes.lock().unwrap()
    }
    /// Every call from now on fails this way.
    fn fail_with(&self, fault: FetchFault) {
        *self.fault.lock().unwrap() = Some(fault);
    }
    fn rename_to(&self, owner: &str, repo: &str) {
        *self.renamed.lock().unwrap() = Some((owner.to_owned(), repo.to_owned()));
    }
    /// The commit the current archive carries — what an honest probe of it reports.
    fn head(&self) -> String {
        crate::git_source::extract_tree(&self.archive.lock().unwrap().clone())
            .ok()
            .and_then(|t| t.commit)
            .unwrap_or_default()
    }
    fn injected(&self) -> Option<ClientError> {
        self.fault
            .lock()
            .unwrap()
            .map(|fault| ClientError::RemoteFetch {
                msg: "o/r — the fake forge was told to fail".to_owned(),
                fault,
            })
    }
}
impl crate::git_source::GitTarballSource for FakeGit {
    fn fetch(&self, _spec: &crate::source::RemoteSpec) -> Result<Vec<u8>, ClientError> {
        *self.fetches.lock().unwrap() += 1;
        match self.injected() {
            Some(e) => Err(e),
            None => Ok(self.archive.lock().unwrap().clone()),
        }
    }
    fn probe(&self, _spec: &crate::source::RemoteSpec) -> Result<RepoHead, ClientError> {
        *self.probes.lock().unwrap() += 1;
        match self.injected() {
            Some(e) => Err(e),
            None => Ok(RepoHead {
                commit: self.head(),
                renamed_to: self.renamed.lock().unwrap().clone(),
                retry_after_ms: None,
            }),
        }
    }
}

/// Apply a forge reference through `add --yes` — the accepted describe an interactive add always
/// shows first.
fn gate_add(ctx: &Ctx<'_>, plane: &FakePlane, dir: &FakeDirectory, git: &FakeGit, raw: &str) {
    match ops::add_reference(
        ctx,
        &connect(plane, dir),
        Some(git as &dyn crate::git_source::GitTarballSource),
        raw,
        true,
        true,
        &Default::default(),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
}

#[test]
fn a_floating_repo_row_advances_through_the_silent_sweep_alone() {
    // ACCEPTANCE 1. No human command anywhere in this test after the row is in place: the sweep
    // that runs at a session start is what carries a GitHub row to a new upstream commit, exactly
    // as it carries a workspace-delivered one.
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

    // The FIRST sweep installs what the row demands — no prior `add`, no consent moment, no
    // ceremony: a committed row is the demand, and the automatic update honors it.
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let alpha = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    let beta = rig.home.0.join(".claude/skills/beta/SKILL.md");
    assert!(
        alpha.exists() && beta.exists(),
        "the sweep installs a cloned project's row: {:?}",
        out.warnings
    );

    // Upstream moves. Inside the interval the sweep does not even ask.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v2\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
            ("skills/gamma/SKILL.md", b"# gamma v1\n"),
        ],
    ));
    let (probes, fetches) = (git.probes(), git.fetches());
    let quiet = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        (git.probes(), git.fetches()),
        (probes, fetches),
        "inside the interval the silent sweep asks the forge nothing"
    );
    assert_eq!(
        std::fs::read_to_string(&alpha).unwrap(),
        "# alpha v1\n",
        "and nothing moves"
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

    // Once the interval has elapsed, the NEXT session start lands it — and says what moved.
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
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
        "the silent sweep lands the new bytes"
    );
    assert!(rig.home.0.join(".claude/skills/gamma/SKILL.md").exists());
}

#[test]
fn an_unchanged_repo_is_probed_and_never_downloaded() {
    // ACCEPTANCE 2, asserted on the TRANSPORT SEAM rather than by timing: a repo that has not
    // moved costs one probe and zero archives. This is the whole reason the lane can run on a
    // clock at all — the check has to be cheap enough to make automatic.
    let rig = Rig::new("repo-cheap");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());

    let fetches = git.fetches();
    let probes = git.probes();
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.fetches(),
        fetches,
        "an unchanged repo downloads NOTHING: {:?}",
        out.warnings
    );
    assert_eq!(git.probes(), probes + 1, "it costs exactly one probe");
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
fn the_forge_clock_is_separate_from_the_workspace_cadence() {
    // ACCEPTANCE 3. In ONE run: the workspace lane dials (its own throttle governs it, and it is
    // not this one), while the forge lane — inside its much longer interval — asks nothing.
    let rig = Rig::new("repo-two-clocks");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    plane.serve(DeliverySnapshot {
        skills: vec![delivered("s_deploy", "deploy", &v)],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"github.com/o/r\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    quiet_sweep(&ctx, &plane, &dir, &git);
    let (probes, fetches) = (git.probes(), git.fetches());
    log.lock().unwrap().clear();

    // A second session start, inside the forge interval.
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        (git.probes(), git.fetches()),
        (probes, fetches),
        "zero forge requests inside the interval"
    );
    assert!(
        log.lock().unwrap().iter().any(|l| l == "report s_deploy"),
        "the workspace lane keeps its own cadence in the same run: {:?}",
        log.lock().unwrap()
    );
    assert!(
        out.data.skills.iter().any(|s| s.skill == "deploy"),
        "{:?}",
        out.data.skills
    );
}

#[test]
fn a_failed_check_advances_the_clock_like_a_successful_one() {
    // ACCEPTANCE 4 — the highest-risk detail in the whole change. A check that FAILED has still
    // had its turn. If the clock only moved on success, one unreachable forge would be re-dialed
    // at every single session start, which is precisely the traffic the interval exists to
    // prevent. So: the lane ran, therefore it waits.
    let rig = Rig::new("repo-clock-on-failure");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // A round that reaches nothing at all.
    git.fail_with(FetchFault::Unreachable);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let attempted = git.probes() + git.fetches();
    assert!(attempted > 0, "the round really did try");
    assert!(out.data.skills.is_empty(), "{:?}", out.data.skills);

    // The clock moved anyway — a whole interval out, exactly as a successful round would leave it.
    assert_eq!(
        forge_next_due(&rig, "github.com/o/r", "", "person"),
        rig_now(&rig) + crate::forge_check::CHECK_INTERVAL_MS,
        "a failed round waits exactly as long as one that worked"
    );

    // THE REGRESSION: the next session start inside the window asks nothing and says nothing.
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        attempted,
        "the failure is not re-dialed at the next session start"
    );
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), rig_now(&rig), &out);
    assert!(lines.is_empty(), "and emits no line: {lines:?}");
}

#[test]
fn a_dead_network_costs_one_timeout_for_the_whole_round() {
    // ACCEPTANCE 5. Five rows, one dead forge: the breaker short-circuits after the first fault
    // that never reached it, so a session start pays ONE connect timeout rather than five. The
    // clock still advances, and every row is recorded as checked — a skipped source had its turn.
    let rig = Rig::new("repo-breaker");
    rig.write_global(
        "[bundles]\n\"github.com/o/a\" = \"*\"\n\"github.com/o/b\" = \"*\"\n\
         \"github.com/o/c\" = \"*\"\n\"github.com/o/d\" = \"*\"\n\"github.com/o/e\" = \"*\"\n",
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::Unreachable);

    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        1,
        "ONE dial for five rows behind one dead forge"
    );
    // Every source is nonetheless recorded as checked this round, so none of them comes straight
    // back at the next session start.
    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    assert_eq!(doc.sources.len(), 5, "{:?}", doc.sources);
    assert!(
        doc.sources
            .values()
            .all(|c| c.checked_at_ms == rig_now(&rig)),
        "{:?}",
        doc.sources
    );
    assert!(
        doc.sources.values().all(|c| c
            .next_check_at
            .values()
            .all(|at| *at == rig_now(&rig) + crate::forge_check::CHECK_INTERVAL_MS)),
        "each skipped source schedules its own next turn: {:?}",
        doc.sources
    );
    // The skipped rows say nothing of their own: the source that actually failed already did, and
    // "we skipped this because something else broke" is noise about someone else's problem.
    assert!(
        !out.warnings
            .iter()
            .any(|w| w.contains("already unreachable")),
        "{:?}",
        out.warnings
    );

    // A forge that ANSWERS about one repo never trips it — the answer says nothing about the next.
    let rig = Rig::new("repo-breaker-http");
    rig.write_global("[bundles]\n\"github.com/o/a\" = \"*\"\n\"github.com/o/b\" = \"*\"\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::unavailable());
    quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        2,
        "an HTTP-level answer about one repo must not short-circuit the other"
    );
}

#[test]
fn a_machine_with_no_prior_grant_auto_updates_a_cloned_projects_row() {
    // ACCEPTANCE 6. The first-trust registry is gone: a committed row IS the demand, and a fresh
    // machine that has never seen the source converges it automatically. The interactive `add`
    // keeps its describe — that is where a person is present to read the answer.
    let rig = Rig::new("repo-no-grant");
    let proj = project("proj-cloned", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let pctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    // Nothing has ever granted this origin, and no such registry exists to grant it.
    assert!(!rig.layout().state_dir().join("forge_trust.json").exists());

    let out = quiet_sweep(&pctx, &plane, &dir, &git);
    assert!(
        proj.0.join(".claude/skills/alpha/SKILL.md").exists(),
        "the clone's row converges on its own: {:?}",
        out.warnings
    );
    assert!(
        !out.warnings.iter().any(|w| w.contains("FIRST_TRUST")),
        "{:?}",
        out.warnings
    );
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).is_some(),
        "installed through the project's own store"
    );

    // The interactive add still DESCRIBES first — including for this now-tracked source.
    let outcome = ops::add_reference(
        &pctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        false,
        &Default::default(),
    )
    .unwrap();
    match outcome {
        ops::AddRefOutcome::Described { data, yes_argv } => {
            assert_eq!(data.source, "github.com/o/r");
            assert!(yes_argv.contains(&"--yes".to_owned()), "{yes_argv:?}");
        }
        ops::AddRefOutcome::Applied(_) => {
            panic!("an interactive add of a git source always describes first")
        }
    }
}

#[test]
fn a_deleted_repo_is_reported_once_and_then_left_alone() {
    // ACCEPTANCE 7a. A repo the forge says is gone is a fact about the ROW, not about the
    // network: saying it every session would train a person to stop reading. Said once, then the
    // lane stops asking until the row that names it changes.
    let rig = Rig::new("repo-deleted");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    let alpha = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    assert!(alpha.exists());

    // The repo is deleted upstream.
    git.fail_with(FetchFault::Gone);
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let dialed = git.probes() + git.fetches();
    assert!(
        out.warnings.iter().any(|w| w.starts_with("REMOTE_FETCH")),
        "the refusal is said: {:?}",
        out.warnings
    );
    assert!(alpha.exists(), "the bytes already here keep working");

    // Round two: not asked, not said.
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        dialed,
        "a settled verdict stops the dialing"
    );
    assert!(
        !out.warnings.iter().any(|w| w.starts_with("REMOTE_FETCH")),
        "and is not repeated: {:?}",
        out.warnings
    );
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "the row still converges in place: {:?}",
        out.data.skills
    );

    // EDITING the row re-opens the question — the verdict was about what the row said. (The pin
    // names a commit nothing here holds, so the row is genuinely unsettled and does want an
    // answer; a pin already satisfied would rightly never dial at all.)
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"ffffffffffff9\"\n");
    forge_interval_elapsed(&rig);
    quiet_sweep(&ctx, &plane, &dir, &git);
    assert!(
        git.probes() + git.fetches() > dialed,
        "a changed row asks again"
    );
}

#[test]
fn a_renamed_repo_keeps_working_and_says_where_it_went() {
    // ACCEPTANCE 7b. The forge redirects a renamed repo and the check follows it, so the row goes
    // on resolving. A rename is never reported as a missing repo — and never as a failure at all:
    // the person is simply told the canonical spelling, on the channel for things that WORKED.
    let rig = Rig::new("repo-renamed");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    git.rename_to("newowner", "newrepo");
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "the renamed repo keeps delivering: {:?}",
        out.warnings
    );
    let line = out
        .disclosures
        .iter()
        .find(|d| d.starts_with("GIT_RENAMED"))
        .unwrap_or_else(|| panic!("the rename note: {:?}", out.disclosures));
    assert!(line.contains("github.com/newowner/newrepo"), "{line}");
    assert!(
        !out.warnings.iter().any(|w| w.contains("not found")),
        "a rename is never reported as a missing repo: {:?}",
        out.warnings
    );
}

#[test]
fn five_rows_behind_one_quiet_forge_produce_one_line_and_only_when_stale() {
    // ACCEPTANCE 8. The silent sweep's only channel is text injected into an agent's context
    // window, so its budget is a person's attention: five rows behind one dead forge is ONE thing
    // that happened. And it is said only once the silence has run long enough to mean something —
    // a blip must never interrupt a session.
    let rig = Rig::new("repo-one-line");
    rig.write_global(
        "[bundles]\n\"github.com/o/a\" = \"*\"\n\"github.com/o/b\" = \"*\"\n\
         \"github.com/o/c\" = \"*\"\n\"github.com/o/d\" = \"*\"\n\"github.com/o/e\" = \"*\"\n",
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let archive = build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    );
    // All five answer once, so each has a last-answered time to be stale FROM.
    for name in ["a", "b", "c", "d", "e"] {
        let git = FakeGit::new(archive.clone());
        forge_interval_elapsed(&rig);
        update_now(&ctx, &plane, &dir, &git);
        let _ = name;
    }
    let git = FakeGit::new(archive);
    git.fail_with(FetchFault::Unreachable);
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(out.stale_forge.len(), 1, "one host: {:?}", out.stale_forge);
    assert_eq!(out.stale_forge[0].host, "github.com");
    assert_eq!(out.stale_forge[0].sources, 5);

    // A fresh miss says NOTHING — the copies still work, and a transient blip is not news.
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), rig_now(&rig), &out);
    assert!(lines.is_empty(), "not stale yet: {lines:?}");

    // Long after the last answer, ONE line — naming the host, the count, and the consequence.
    let stale_now = rig_now(&rig) + crate::forge_check::STALE_AFTER_MS + DAY_MS;
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("github.com"), "{:?}", lines[0]);
    // SOURCES, not skills: one repository row can hold any number of skills, so counting the
    // repositories and calling them skills would be a number about the wrong thing.
    assert!(lines[0].contains("5 sources"), "{:?}", lines[0]);
    assert!(
        lines[0].contains("they still work"),
        "the consequence, so `unreachable` does not read as breakage: {:?}",
        lines[0]
    );
}

#[test]
fn a_repo_set_member_reads_as_current_in_list_not_detached() {
    // ACCEPTANCE 9 — the two commands must not contradict each other. `update` keeps a
    // GitHub-sourced skill current; `list` used to render the very same copy as
    // "[detached] … removed from the skill list", because the set row's expansion was never
    // itemized and the ghost walk therefore read a live, managed copy as an abandoned leftover.
    // Asserted against BOTH commands in one test, because the bug WAS the disagreement.
    let rig = Rig::new("repo-list-current");
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

    // What `update` says: both members are managed and current.
    let out = update_now(&ctx, &plane, &dir, &git);
    for name in ["alpha", "beta"] {
        assert!(
            out.data.skills.iter().any(|s| s.skill == name),
            "update manages {name}: {:?}",
            out.data.skills
        );
        assert!(
            rig.home
                .0
                .join(format!(".claude/skills/{name}/SKILL.md"))
                .exists(),
            "{name}'s bytes are placed"
        );
    }

    // What `list` says: the SAME thing. A member is a live row of the set that delivers it —
    // never a ghost, never detached, never "removed from the skill list".
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    let rows: Vec<&topos_types::results::SkillEntry> = listed
        .data
        .scopes
        .iter()
        .flat_map(|sc| sc.rows.iter())
        .collect();
    for name in ["alpha", "beta"] {
        let row = rows
            .iter()
            .find(|r| r.skill == name)
            .unwrap_or_else(|| panic!("list itemizes {name}: {rows:?}"));
        assert_eq!(
            row.status,
            Some(topos_types::results::SkillStatus::Current),
            "{name} is managed by update, so list must call it current: {row:?}"
        );
        // The source column names WHERE THE BYTES COME FROM — the repository, exactly as for
        // every other kind of line. That is the whole point: the member is an ordinary delivered
        // row now, and it says its own origin instead of leaning on a block below the list.
        assert_eq!(row.source.as_deref(), Some("github.com/o/r"), "{row:?}");
    }

    // And the rendered text carries no contradiction either.
    let text = crate::render::list_tty(&listed);
    assert!(!text.contains("[detached]"), "{text}");
    assert!(!text.contains("removed from the skill list"), "{text}");
    // ONE list: the repository is named on the rows it delivers, never as a trailing block of
    // its own — the listing has exactly one place a bundle can be read from.
    assert!(text.contains("alpha  alpha@"), "{text}");
    assert!(text.contains("from github.com/o/r"), "{text}");
    assert!(!text.contains("external sources:"), "{text}");
}

#[test]
fn two_rows_over_one_gone_repo_ask_once_and_say_it_once() {
    // A set line and a member line of the SAME repository are two rows about one subject. A
    // verdict reached for the first must hold the second — otherwise a deleted repo named twice
    // costs two requests and says the same sentence twice in one breath.
    let rig = Rig::new("repo-two-rows-gone");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n\"github.com/o/r/alpha\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::Gone);

    let out = update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        1,
        "one repository, one question"
    );
    let said: Vec<&String> = out
        .warnings
        .iter()
        .filter(|w| w.starts_with("REMOTE_FETCH"))
        .collect();
    assert_eq!(said.len(), 1, "said once, not once per row: {said:?}");
}

#[test]
fn a_pinned_rows_fetch_is_recorded_as_a_check_like_any_other() {
    // A pinned row never probes: it is settled and silent, or it fetches. The fetch is still a
    // check, and "recorded always" has to mean always — otherwise `status` would show nothing
    // about a source topos had just contacted.
    let rig = Rig::new("repo-pinned-record");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"aaaaaaaaaaaa1\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(git.probes(), 0, "a pinned row is never probed");
    assert!(git.fetches() > 0, "it fetched the pinned bytes");

    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    let rec = doc
        .sources
        .get(&crate::forge_check::question(
            "github.com/o/r",
            "aaaaaaaaaaaa1",
        ))
        .unwrap_or_else(|| panic!("the pinned fetch is recorded: {:?}", doc.sources));
    assert_eq!(rec.checked_at_ms, rig_now(&rig));
    assert_eq!(rec.answered_at_ms, Some(rig_now(&rig)));
    assert!(rec.failure.is_none(), "{rec:?}");
}

#[test]
fn a_machine_with_no_external_rows_schedules_nothing() {
    // A clock is about a subject. Nothing was checked — no external rows at all — so there is no
    // turn to have taken and no next turn to schedule; the document is not even born.
    let rig = Rig::new("repo-no-rows");
    rig.write_global("[bundles]\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz("o-r-aaaaaaaaaaaa1", &[]));
    update_now(&ctx, &plane, &dir, &git);
    assert!(
        !rig.layout().forge_check_path().exists(),
        "no source, no clock"
    );
}

#[test]
fn a_failed_check_on_an_uninstalled_member_says_one_thing_not_two() {
    // The fault is the whole story. Adding "this machine has not fetched it yet" beside it would
    // be a second, weaker sentence about the same event — and would read as a separate problem.
    let rig = Rig::new("repo-one-sentence");
    rig.write_global("[bundles]\n\"github.com/o/r/alpha\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::Unreachable);
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(
        out.warnings.iter().any(|w| w.starts_with("REMOTE_FETCH")),
        "the fault is named: {:?}",
        out.warnings
    );
    assert!(
        !out.warnings.iter().any(|w| w.starts_with("NOT_INSTALLED")),
        "and only once: {:?}",
        out.warnings
    );
}

#[test]
fn editing_a_sibling_rows_ref_reopens_it_past_another_rows_settled_verdict() {
    // Editing the ref is the documented way to reopen a settled verdict. A SECOND row naming the
    // same repository at its old ref must not be able to take that away: the one-turn rule is
    // about the QUESTION asked, and two rows at different refs ask different questions.
    let rig = Rig::new("repo-sibling-reopen");
    rig.write_global("[bundles]\n\"github.com/o/r/alpha\" = \"*\"\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    // Both rows settle on a gone verdict at the floating ref.
    git.fail_with(FetchFault::Gone);
    update_now(&ctx, &plane, &dir, &git);
    let settled = git.probes() + git.fetches();
    assert!(settled > 0);

    // Now ONE of them is edited to a pin. The other still spells the old floating ref and is
    // reconciled first — it must not silence the edited one.
    rig.write_global(
        "[bundles]\n\"github.com/o/r/alpha\" = \"*\"\n\"github.com/o/r\" = \"ffffffffffff9\"\n",
    );
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert!(
        git.probes() + git.fetches() > settled,
        "the edited row asks again despite its sibling's standing verdict"
    );
}

#[test]
fn a_round_that_only_carried_verdicts_schedules_nothing() {
    // A carried verdict is not a dial. Writing it back into the round's outcomes would make a run
    // that asked nobody anything look like one that did — and push the clock out for sources that
    // were never contacted.
    let rig = Rig::new("repo-carried-only");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    git.fail_with(FetchFault::Gone);
    update_now(&ctx, &plane, &dir, &git);
    let due_after_the_real_round = forge_next_due(&rig, "github.com/o/r", "", "person");

    // A later round finds only the standing verdict: it dials nothing, and moves nothing.
    forge_interval_elapsed(&rig);
    let parked = forge_next_due(&rig, "github.com/o/r", "", "person");
    let dialed = git.probes() + git.fetches();
    quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(git.probes() + git.fetches(), dialed, "nothing was asked");
    assert_eq!(
        forge_next_due(&rig, "github.com/o/r", "", "person"),
        parked,
        "and nothing was scheduled: {due_after_the_real_round}"
    );
}

#[test]
fn a_reachable_but_failing_forge_is_not_called_unreachable() {
    // "Unreachable" names a network. A forge that answers 403 or 500 was reached and answered
    // badly, which is a different thing to go look at — and the line must not say the wrong one.
    let rig = Rig::new("repo-answered-badly");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    git.fail_with(FetchFault::unavailable());
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(out.stale_forge.len(), 1, "{:?}", out.stale_forge);
    assert!(out.stale_forge[0].reached, "the forge answered");
    let stale_now = rig_now(&rig) + crate::forge_check::STALE_AFTER_MS + DAY_MS;
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        !lines[0].contains("unreachable"),
        "a host that answered is not unreachable: {:?}",
        lines[0]
    );
    assert!(lines[0].contains("github.com"), "{:?}", lines[0]);
    assert!(lines[0].contains("they still work"), "{:?}", lines[0]);
}

#[test]
fn an_unreadable_archive_is_a_failed_check_not_a_passed_one() {
    // The request succeeded; the answer did not. Recording a 200 as a good check would let
    // `status` report success for a round that could not install anything — and would file the
    // PREVIOUS commit as the one just seen.
    let rig = Rig::new("repo-bad-archive");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"aaaaaaaaaaaa1\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(b"this is not a gzip archive".to_vec());
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(!out.warnings.is_empty(), "the round failed");

    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    let rec = doc
        .sources
        .get(&crate::forge_check::question(
            "github.com/o/r",
            "aaaaaaaaaaaa1",
        ))
        .unwrap_or_else(|| panic!("the attempt is recorded: {:?}", doc.sources));
    assert!(
        rec.failure.is_some(),
        "an unreadable archive is a FAILED check: {rec:?}"
    );
    assert_eq!(
        rec.answered_at_ms, None,
        "it never really answered: {rec:?}"
    );
}

#[test]
fn a_member_the_repo_dropped_leaves_the_list_entirely() {
    // The mirror image of the bug the set-member enumeration exists to fix, and just as much a lie.
    // When upstream drops a member, the reconcile retires its placements but KEEPS its origin and
    // lock — custody outlives delivery. The retained record mints NO list row (records may
    // describe rows, never create them): only members that still hold placed bytes are itemized,
    // and the leftover's one-time resolution belongs to a later `update`.
    let rig = Rig::new("repo-dropped-member");
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
    update_now(&ctx, &plane, &dir, &git);
    assert!(rig.home.0.join(".claude/skills/beta/SKILL.md").exists());

    // Upstream drops `beta`. The update retires its placements; its custody record stays.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    forge_interval_elapsed(&rig);
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "beta" && s.action == PullAction::Withdrawn),
        "the member was retired: {:?}",
        out.data.skills
    );
    assert!(
        crate::ops::forge_imports(&ctx)
            .iter()
            .any(|i| i.lock.name == "beta"),
        "and its custody record is deliberately kept"
    );

    // What `list` must say about it: NOTHING — no row demands it, so no line originates from it.
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    let rows: Vec<&topos_types::results::SkillEntry> = listed
        .data
        .scopes
        .iter()
        .flat_map(|sc| sc.rows.iter())
        .collect();
    assert!(
        !rows.iter().any(|r| r.skill == "beta"),
        "a member the repo no longer holds mints no row: {rows:?}"
    );
    // …while the member that IS still delivered reads exactly as it did before.
    let alpha = rows.iter().find(|r| r.skill == "alpha").expect("alpha");
    assert_eq!(
        alpha.status,
        Some(topos_types::results::SkillStatus::Current),
        "{alpha:?}"
    );
}

#[test]
fn a_signed_out_workspaces_leftover_resolves_once_then_retires() {
    // The one-time orphan resolution: a store record no row claims and nothing delivers gets ONE
    // closing statement — workspace-named, files-led — and then leaves every surface. Nothing on
    // disk is deleted: the placed copy stays, and belongs to the person now.
    let rig = Rig::new("orphan-signed-out");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# alpha v1\n");
    let plane = FakePlane::new(log).with_version("s_alpha", &v1);
    plane.serves(vec![delivered("s_alpha", "alpha", &v1)]);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/alpha");
    assert!(placed.join("SKILL.md").exists(), "alpha's bytes are placed");

    // Sign out (the session ends; a deliberate state) and drop the feed line: nothing claims the
    // record and nothing can deliver it. BEFORE the resolution, the record still owns its name.
    sessions::set_session_status(&rig.fs, &rig.layout(), HOST, WS, SESSION_ENDED).unwrap();
    rig.write_global("[bundles]\n");
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: None,
    };
    assert!(
        matches!(
            ops::plan_bare_add(
                &ctx,
                &connect(&plane, &dir),
                &roots,
                "alpha",
                false,
                false,
                None
            ),
            Err(ClientError::AlreadyTrackedName { .. })
        ),
        "before the resolution the record still resolves the bare name"
    );

    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "alpha")
        .unwrap_or_else(|| {
            panic!(
                "the one-time resolution rides the receipt: {:?}",
                out.data.skills
            )
        });
    assert_eq!(row.action, PullAction::Released);
    // Byte-exact: the workspace-named, single-folder path form of the released fact.
    assert_eq!(
        row.note.as_deref(),
        Some(
            format!(
                "{WS_NAME} stopped sharing this — topos will not update it any more\nthe files \
                 stay where they are, and are yours to keep or delete: {}",
                placed.display()
            )
            .as_str()
        )
    );
    assert_eq!(
        row.display.as_deref(),
        Some(format!("{HOST}/{WS_NAME}/alpha").as_str()),
        "no live host, so the full spelling qualifies the name"
    );
    // The marker is durable; the files and the store bytes stay untouched.
    let sid = crate::id::SkillId::parse("s_alpha").unwrap();
    assert!(rig.fs.exists(&rig.layout().published(&sid).retired));
    assert!(
        placed.join("SKILL.md").exists(),
        "the resolution deletes nothing"
    );
    assert!(
        rig.fs.exists(&rig.layout().skill_dir(&sid)),
        "the record's bytes stay on disk forever"
    );

    // RETIREMENT DURABILITY: the next sweep says nothing; `list` shows nothing; the record no
    // longer resolves a bare name (the kept folder is adoptable again instead).
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "alpha"),
        "a retired record is never mentioned again: {:?}",
        out.data.skills
    );
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    assert!(
        !listed
            .data
            .scopes
            .iter()
            .flat_map(|sc| sc.rows.iter())
            .any(|r| r.skill == "alpha"),
        "no row claims it, so no line originates from it"
    );
    // The retired record no longer answers for the name at all (this rig's placement dir sits
    // outside the registry discovery roots, so the honest answer is the ordinary no-such-name —
    // never the record's "already tracked").
    assert!(
        matches!(
            ops::plan_bare_add(
                &ctx,
                &connect(&plane, &dir),
                &roots,
                "alpha",
                false,
                false,
                None
            ),
            Err(ClientError::NoUntrackedSkill { .. })
        ),
        "a retired record must not resolve the bare name"
    );
}

#[test]
fn an_unreached_workspace_freezes_its_records_and_a_withdrawal_resolves_later() {
    // The freeze gates, end to end: a live session with NO fresh snapshot this run freezes its
    // records (an outage never reads as an orphan); the run a withdrawal actually lands in speaks
    // through the withdraw row alone; and only the NEXT full sweep resolves the leftover — once.
    let rig = Rig::new("orphan-freeze");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# alpha v1\n");
    let plane = FakePlane::new(log).with_version("s_alpha", &v1);
    plane.serves(vec![delivered("s_alpha", "alpha", &v1)]);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let sid = crate::id::SkillId::parse("s_alpha").unwrap();

    // The workspace is unreachable: everything freezes — no withdrawal, no resolution.
    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.data.skills.iter().any(|s| {
            s.skill == "alpha" && matches!(s.action, PullAction::Withdrawn | PullAction::Released)
        }),
        "an outage must never read as an orphan: {:?}",
        out.data.skills
    );
    assert!(!rig.fs.exists(&rig.layout().published(&sid).retired));

    // The workspace answers fresh and no longer serves alpha: the withdraw arm speaks this run,
    // and the record is fresh news — not yet a standing orphan.
    plane.serves(Vec::new());
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );
    assert!(
        !rig.fs.exists(&rig.layout().published(&sid).retired),
        "a record that earned a receipt row this run is not retired under it"
    );

    // The NEXT full sweep resolves the leftover once: no copies remain (the withdrawal cleaned
    // them), the record retires, and a further sweep says nothing.
    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "alpha" && s.action == PullAction::Released)
        .unwrap_or_else(|| panic!("the one-time resolution: {:?}", out.data.skills));
    assert_eq!(
        row.note.as_deref(),
        Some("no copies remain — nothing left to manage")
    );
    assert!(rig.fs.exists(&rig.layout().published(&sid).retired));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "alpha"),
        "{:?}",
        out.data.skills
    );
}

#[test]
fn a_targeted_update_never_resolves_orphans() {
    // A targeted update narrows within the driven scope: it cannot know the whole demand set, so
    // the orphan pass never runs under it.
    let rig = Rig::new("orphan-targeted");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# alpha v1\n");
    let v2 = one_file(b"# beta v1\n");
    let plane = FakePlane::new(log)
        .with_version("s_alpha", &v1)
        .with_version("s_beta", &v2);
    plane.serves(vec![
        delivered("s_alpha", "alpha", &v1),
        delivered("s_beta", "beta", &v2),
    ]);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    // Upstream withdraws alpha; a full sweep lands the withdrawal (fresh news, not yet an
    // orphan). From here alpha's record is orphan-eligible.
    plane.serves(vec![delivered("s_beta", "beta", &v2)]);
    sweep(&ctx, &plane, &dir);

    // The targeted form: converge beta alone — the eligible record is left exactly as it is.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["beta".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "alpha"),
        "{:?}",
        out.data.skills
    );
    let sid = crate::id::SkillId::parse("s_alpha").unwrap();
    assert!(
        !rig.fs.exists(&rig.layout().published(&sid).retired),
        "a targeted update never resolves orphans"
    );

    // The full sweep still owns the resolution.
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::Released),
        "{:?}",
        out.data.skills
    );
    assert!(rig.fs.exists(&rig.layout().published(&sid).retired));
}

#[test]
fn the_released_fact_shapes_are_byte_exact() {
    // Every final-copy shape, pinned at the composer (the renderer prints the fact verbatim, one
    // line per line). The two facts a person needs, in order: who stopped sharing, and where that
    // leaves the files — the second line naming the ONE folder, or listing several beneath itself.
    assert_eq!(
        ops::orphan_fact(
            Some("acme"),
            &["~/.claude/skills/legacy-deploy".to_owned()],
            &["~/.claude/skills/legacy-deploy".to_owned()],
        ),
        "acme stopped sharing this — topos will not update it any more\nthe files stay where they \
         are, and are yours to keep or delete: ~/.claude/skills/legacy-deploy"
    );
    let folders = |n: usize| -> Vec<String> { (0..n).map(|i| format!("/tmp/f{i}")).collect() };
    assert_eq!(
        ops::orphan_fact(Some("acme"), &folders(2), &folders(2)),
        "acme stopped sharing this — topos will not update it any more\nthe files stay where they \
         are, and are yours to keep or delete:\n  /tmp/f0\n  /tmp/f1"
    );
    // No workspace name on record: the row states only what it knows — where the files stand —
    // rather than a sentence with a hole where the name belongs.
    assert_eq!(
        ops::orphan_fact(
            None,
            &["~/.claude/skills/runbook".to_owned()],
            &["~/.claude/skills/runbook".to_owned()],
        ),
        "the files stay where they are, and are yours to keep or delete: ~/.claude/skills/runbook"
    );
    // Nothing left on disk: the fact states the DISCOVERY (the copies were already gone), never an
    // act this run performed. One recorded copy names it; several name the last of them.
    assert_eq!(
        ops::orphan_fact(None, &folders(0), &["/private/tmp/deploy".to_owned()]),
        "its only copy, /private/tmp/deploy, no longer exists — nothing left to manage"
    );
    assert_eq!(
        ops::orphan_fact(
            None,
            &folders(0),
            &[
                "~/.claude/skills/deploy".to_owned(),
                "~/.cursor/skills/deploy".to_owned(),
                "/private/tmp/deploy".to_owned(),
            ],
        ),
        "its 3 copies, last at /private/tmp/deploy, no longer exist — nothing left to manage"
    );
    // Nothing on disk AND nothing recorded (the withdrawal already cleaned the map): the same
    // consequence, minimally stated.
    assert_eq!(
        ops::orphan_fact(None, &folders(0), &folders(0)),
        "no copies remain — nothing left to manage"
    );
}

#[test]
fn re_adding_a_repo_that_came_back_re_enables_its_automatic_update() {
    // A verdict is a record of what a forge SAID. A forge that has just handed over the bytes has
    // said something newer — so a repository deleted, written off, and later restored must not stay
    // permanently un-updated because a refusal is still on file.
    let rig = Rig::new("repo-came-back");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // Gone. The verdict settles and the lane stops asking.
    git.fail_with(FetchFault::Gone);
    update_now(&ctx, &plane, &dir, &git);
    forge_interval_elapsed(&rig);
    let settled = git.probes() + git.fetches();
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        settled,
        "a settled verdict stops the dialing"
    );

    // The repository comes back, and the person re-adds it. The add succeeds, which is newer
    // evidence than the refusal.
    *git.fault.lock().unwrap() = None;
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());

    // THE POINT: automatic updates work again. Without the forgetting the lane would go on
    // refusing to dial a source the person just proved is alive.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    forge_interval_elapsed(&rig);
    let before = git.probes() + git.fetches();
    quiet_sweep(&ctx, &plane, &dir, &git);
    assert!(
        git.probes() + git.fetches() > before,
        "the restored source is checked again"
    );
    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "and the silent sweep carries it forward again"
    );
}

#[test]
fn a_deletion_the_silent_sweep_finds_is_actually_said() {
    // "Report once" has to mean once OUT LOUD. The silent sweep discards the warnings channel, so
    // a deletion discovered by the sweep — the ordinary way it is discovered — would be recorded
    // as reported, never shown, and then suppressed forever.
    let rig = Rig::new("repo-quiet-deletion");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    git.fail_with(FetchFault::Gone);
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), rig_now(&rig), &out);
    assert_eq!(lines.len(), 1, "the sweep says it: {lines:?}");
    assert!(lines[0].contains("github.com/o/r"), "{:?}", lines[0]);
    assert!(lines[0].contains("still work"), "{:?}", lines[0]);

    // And exactly once: the next sweep neither dials nor repeats it.
    forge_interval_elapsed(&rig);
    let out = quiet_sweep(&ctx, &plane, &dir, &git);
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), rig_now(&rig), &out);
    assert!(lines.is_empty(), "and only once: {lines:?}");
}

#[test]
fn an_emptied_repository_stays_probe_only() {
    // A repository that drops its last skill leaves retained custody records still naming the OLD
    // commit, so "what is installed" can never again match the head. Without remembering that this
    // scope already finished at that head, the archive would be downloaded every single cadence to
    // rediscover that there is nothing in it.
    let rig = Rig::new("repo-emptied");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    // The repo empties out.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("README.md", b"nothing to share any more\n")],
    ));
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    let fetches = git.fetches();

    // Every later round sees the same head and downloads NOTHING.
    for _ in 0..3 {
        forge_interval_elapsed(&rig);
        let out = quiet_sweep(&ctx, &plane, &dir, &git);
        assert!(
            !out.disclosures.iter().any(|d| d.starts_with("GIT_UPDATED")),
            "and does not re-announce a move that already happened: {:?}",
            out.disclosures
        );
    }
    assert_eq!(
        git.fetches(),
        fetches,
        "an unchanged empty repository is probed, never downloaded"
    );
    assert!(git.probes() > 0, "it is still checked");
}

#[test]
fn scoped_forge_health_answers_about_the_scopes_own_pin() {
    // Two scopes can track one repository at different pins. Those are different questions with
    // different answers, and a scoped read must report the one IT asks. A project-only `status`
    // showing the machine-only pin's failure would be a report about somebody else's row.
    let rig = Rig::new("repo-scoped-health");
    // The MACHINE pins a commit that is gone; the PROJECT pins one that is fine. The machine's
    // question sorts LAST, so a source-level filter would let it win the "newest" fold.
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"ffffffffffff9\"\n");
    let proj = project(
        "proj-scoped-health",
        "[bundles]\n\"github.com/o/r\" = \"aaaaaaaaaaaa1\"\n",
    );
    let pctx = rig.ctx_at(Some(&proj.0));
    let now = rig_now(&rig);
    let fine = crate::forge_check::SourceCheck {
        next_check_at: Default::default(),
        checked_at_ms: now,
        answered_at_ms: Some(now),
        commit: Some("aaaaaaaaaaaa1".to_owned()),
        failure: None,
        settled_at: Default::default(),
    };
    let gone = crate::forge_check::SourceCheck {
        next_check_at: Default::default(),
        checked_at_ms: now,
        answered_at_ms: None,
        commit: None,
        failure: Some(crate::forge_check::CheckFailure {
            reason: "repo not found".to_owned(),
            gone: true,
            reached: true,
            git_ref: "ffffffffffff9".to_owned(),
            reported: true,
        }),
        settled_at: Default::default(),
    };
    crate::forge_check::record_round(
        &rig.fs,
        &rig.layout(),
        None,
        &[
            (
                crate::forge_check::question("github.com/o/r", "aaaaaaaaaaaa1"),
                fine,
            ),
            (
                crate::forge_check::question("github.com/o/r", "ffffffffffff9"),
                gone,
            ),
        ],
    )
    .unwrap();

    // The project's own question answered fine, so its `status` says nothing is wrong.
    let here = ops::status_snapshot(&pctx, ops::ScopeView::Here).unwrap();
    assert_eq!(here.forge.len(), 1, "{:?}", here.forge);
    assert_eq!(here.forge[0].source, "github.com/o/r");
    assert!(
        here.forge[0].error.is_none() && !here.forge[0].gone,
        "a project-only read must not carry the machine-only pin's refusal: {:?}",
        here.forge[0]
    );
    assert_eq!(here.forge[0].commit.as_deref(), Some("aaaaaaaaaaaa1"));

    // The MACHINE scope asks the other question, and gets the other answer.
    let machine = ops::status_snapshot(&pctx, ops::ScopeView::Machine).unwrap();
    assert_eq!(machine.forge.len(), 1, "{:?}", machine.forge);
    assert!(
        machine.forge[0].gone,
        "the machine's own pin is the one that is gone: {:?}",
        machine.forge[0]
    );
}

#[test]
fn the_deep_dive_names_only_its_own_external_source() {
    // The one-skill dive is the only `list` view that PRINTS this block, and it prints one line at
    // most. Every line the dive shows is a fact about the skill on screen, so an unrelated
    // repository's last check would read as one more of them — "what is this machine tracking?"
    // answering a question nobody asked here. It shows the repository that DELIVERS this skill, or
    // nothing. The resolution still computes the field for every view (the wire contract must not
    // depend on which view fell through); the LISTING simply names each row's origin in its `from`
    // column instead of enumerating sources under the list.
    let rig = Rig::new("repo-focused-views");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    let dive = |name: &str| {
        crate::ops::list_with(
            &ctx,
            &ops::ListRequest {
                name: Some(name.to_owned()),
                ..ops::ListRequest::default()
            },
            None,
            None,
            crate::ops::RowPage::unlimited(),
        )
        .unwrap()
        .data
        .forge
    };

    // The repo delivers `alpha`, so `alpha`'s own answer names it.
    let mine = dive("alpha");
    assert_eq!(mine.len(), 1, "{mine:?}");
    assert_eq!(mine[0].source, "github.com/o/r");

    // Nothing external delivers the built-in — and the machine's unrelated GitHub row is not news
    // about it. THE reported bug: this block used to ride every dive on the machine.
    assert!(dive("topos").is_empty(), "{:?}", dive("topos"));

    // The dive PRINTS the one line it kept — the block is not merely computed there.
    let printed = crate::render::list_tty(
        &crate::ops::list_with(
            &ctx,
            &ops::ListRequest {
                name: Some("alpha".to_owned()),
                ..ops::ListRequest::default()
            },
            None,
            None,
            crate::ops::RowPage::unlimited(),
        )
        .unwrap(),
    );
    assert!(printed.contains("external sources:"), "{printed}");
    assert!(printed.contains("github.com/o/r"), "{printed}");

    // The LISTING still carries the field on the wire — a `--json` reader asks about source health
    // from any view — while its TTY says the same repository per row, in the `from` column.
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    assert!(
        listed
            .data
            .forge
            .iter()
            .any(|f| f.source == "github.com/o/r"),
        "{:?}",
        listed.data.forge
    );
    let text = crate::render::list_tty(&listed);
    assert!(!text.contains("external sources:"), "{text}");
    assert!(text.contains("from github.com/o/r"), "{text}");
}

#[test]
fn a_second_checkout_is_never_starved_by_the_first_ones_sweeps() {
    // THE failure this increment exists to remove, reintroduced by a machine-wide clock. A sweep
    // only ever visits the checkout it starts in plus the machine manifest. If one global due time
    // governed the lane, the first checkout active after each due moment would spend the turn, and
    // a second project — never visited at a due moment — would never be checked at all.
    let rig = Rig::new("repo-two-checkouts");
    let a = project("proj-a", "[bundles]\n\"github.com/o/a\" = \"*\"\n");
    let b = project("proj-b", "[bundles]\n\"github.com/o/b\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let actx = rig.ctx_at(Some(&a.0));
    let bctx = rig.ctx_at(Some(&b.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // Both check once, so both have a clock.
    quiet_sweep(&actx, &plane, &dir, &git);
    quiet_sweep(&bctx, &plane, &dir, &git);
    assert!(b.0.join(".claude/skills/alpha/SKILL.md").exists());

    // Now A is the only checkout anyone works in. Its sweeps run at every due moment.
    for _ in 0..3 {
        forge_interval_elapsed(&rig);
        quiet_sweep(&actx, &plane, &dir, &git);
    }
    // B has not been visited since, so its clock must still be due — A cannot have spent B's turn.
    assert!(
        crate::forge_check::due(
            &crate::forge_check::read(&rig.fs, &rig.layout()),
            &crate::forge_check::question("github.com/o/b", ""),
            &b.0.display().to_string(),
            rig_now(&rig)
        ),
        "the second checkout's row is still owed a check"
    );

    // And the moment somebody opens B, upstream really does reach it.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    quiet_sweep(&bctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(b.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "the second checkout is not starved"
    );
}

#[test]
fn a_fresh_checkout_installs_now_rather_than_waiting_out_another_scopes_interval() {
    // A turn belongs to a SCOPE. Two checkouts share a question but not a state, so a clone must
    // fetch on its first sweep instead of sitting empty until an interval another checkout started
    // runs out — while a scope that HAS taken its turn still waits, whatever the answer was.
    let rig = Rig::new("repo-fresh-clone");
    let a = project("clone-a", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let b = project("clone-b", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    quiet_sweep(&rig.ctx_at(Some(&a.0)), &plane, &dir, &git);
    assert!(a.0.join(".claude/skills/alpha/SKILL.md").exists());

    // B has never asked. Its sweep — well inside the interval A started — must still install.
    quiet_sweep(&rig.ctx_at(Some(&b.0)), &plane, &dir, &git);
    assert!(
        b.0.join(".claude/skills/alpha/SKILL.md").exists(),
        "a fresh checkout is waiting on bytes, not on somebody else's cadence"
    );

    // Both have now had their turn: neither dials again inside the interval.
    let dialed = git.probes() + git.fetches();
    quiet_sweep(&rig.ctx_at(Some(&a.0)), &plane, &dir, &git);
    quiet_sweep(&rig.ctx_at(Some(&b.0)), &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        dialed,
        "a scope that has taken its turn waits"
    );
}

/// A repo whose members refuse differently: `alpha` refreshes cleanly, `beta` carries local edits
/// and its refresh is refused. Two members are the point — with one, the sole tracked import IS the
/// refused one, and a "first recorded commit matches" test can never wrongly pass.
fn two_member_repo(top: &str, alpha: &[u8], beta: &[u8]) -> Vec<u8> {
    build_repo_targz(
        top,
        &[
            ("skills/alpha/SKILL.md", alpha),
            ("skills/beta/SKILL.md", beta),
        ],
    )
}

#[test]
fn a_sibling_moving_on_never_settles_a_member_whose_refresh_was_refused() {
    // `forge_imports` sorts by name, so a row's "recorded commit" read from the FIRST tracked
    // import is alphabetically first — `alpha`. Once alpha moves to the new head, a row-level
    // comparison says the whole row is current while `beta`, whose refresh was refused, sits a
    // commit behind reporting up to date. On a quiet repository that lasts until the next push.
    let rig = Rig::new("repo-sibling-settles");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(two_member_repo(
        "o-r-aaaaaaaaaaaa1",
        b"# alpha v1\n",
        b"# beta v1\n",
    ));
    update_now(&ctx, &plane, &dir, &git);
    let beta = rig.home.0.join(".claude/skills/beta/SKILL.md");
    assert!(beta.exists());

    // beta gets local edits; upstream moves. alpha refreshes, beta's refresh is refused.
    std::fs::write(&beta, "# beta v1 MY EDITS\n").unwrap();
    git.serve(two_member_repo(
        "o-r-bbbbbbbbbbbb2",
        b"# alpha v2\n",
        b"# beta v2\n",
    ));
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "the sibling moved on"
    );

    // The person resolves the edits. The very next update must carry beta forward — nothing
    // upstream has changed, so a row that called itself settled would never revisit it.
    std::fs::write(&beta, "# beta v1\n").unwrap();
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(&beta).unwrap(),
        "# beta v2\n",
        "a member left behind by a refusal is still owed the update"
    );
}

#[test]
fn a_pinned_row_keeps_trying_a_member_whose_refresh_was_refused() {
    // Strictly worse than the floating case: a pin never moves, so there is no later push to heal
    // it. A presence-only settled test here means the member never reaches the pin — ever — while
    // the archive is re-downloaded every interval to skip it again.
    let rig = Rig::new("repo-pinned-refusal");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"aaaaaaaaaaaa1\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(two_member_repo(
        "o-r-aaaaaaaaaaaa1",
        b"# alpha v1\n",
        b"# beta v1\n",
    ));
    update_now(&ctx, &plane, &dir, &git);
    let beta = rig.home.0.join(".claude/skills/beta/SKILL.md");
    assert!(beta.exists());

    // Repin to a new commit, with beta carrying edits so its refresh is refused.
    std::fs::write(&beta, "# beta v1 MY EDITS\n").unwrap();
    git.serve(two_member_repo(
        "o-r-bbbbbbbbbbbb2",
        b"# alpha v2\n",
        b"# beta v2\n",
    ));
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"bbbbbbbbbbbb2\"\n");
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(rig.home.0.join(".claude/skills/alpha/SKILL.md")).unwrap(),
        "# alpha v2\n",
        "the sibling reached the pin"
    );

    // Edits resolved. The pin has not moved and never will, so this update is the only chance
    // beta will ever get.
    std::fs::write(&beta, "# beta v1\n").unwrap();
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    assert_eq!(
        std::fs::read_to_string(&beta).unwrap(),
        "# beta v2\n",
        "a pin with no healing event must keep trying the member it could not place"
    );
}

#[test]
fn a_wiped_agent_directory_is_re_placed_not_declared_current() {
    // A record is not bytes. The sidecar survives a wiped agent directory — an ordinary harness
    // reinstall, or somebody clearing `~/.claude` — and every import goes on claiming its commit
    // for a copy that is no longer anywhere. A settled test resting on records alone would call
    // the row current forever while the skills are simply gone.
    let rig = Rig::new("repo-wiped-agent-dir");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    let placed = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    assert!(placed.exists());

    // The agent directory goes; the sidecar stays exactly as it was.
    std::fs::remove_dir_all(rig.home.0.join(".claude")).unwrap();
    assert!(
        crate::ops::forge_imports(&ctx)
            .iter()
            .any(|i| i.lock.name == "alpha"),
        "the record still claims the member"
    );

    forge_interval_elapsed(&rig);
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(
        placed.exists(),
        "bytes that are gone are not current: {:?}",
        out.warnings
    );
}

#[test]
fn a_refused_refresh_is_not_a_convergence() {
    // A settlement mark says "there is nothing to do here at this head", and the probe trusts it.
    // So it must only ever be written when the work really happened: a refresh the engine REFUSED
    // — a member carrying local edits — leaves that member at the OLD commit, and recording
    // settlement over it would suppress the retry even after the edits are resolved.
    let rig = Rig::new("repo-refused-refresh");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);
    let placed = rig.home.0.join(".claude/skills/alpha/SKILL.md");
    assert!(placed.exists());

    // Local edits, then upstream moves. The refresh keeps the edits rather than clobbering them,
    // so the member stays at the old commit — this round converged nothing.
    std::fs::write(&placed, "# alpha v1 WITH MY EDITS\n").unwrap();
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);

    // Whatever the engine chose to do with the edits, it must not have recorded the new head as
    // settled unless the member really sits on it.
    let doc = crate::forge_check::read(&rig.fs, &rig.layout());
    let rec = doc
        .sources
        .get(&crate::forge_check::question("github.com/o/r", ""))
        .expect("recorded");
    let landed_on_new_head = crate::ops::forge_imports(&ctx)
        .iter()
        .any(|i| i.lock.name == "alpha" && i.origin.commit.as_deref() == Some("bbbbbbbbbbbb2"));
    if !landed_on_new_head {
        // The EARLIER round really did converge at the old head, and that mark rightly stands.
        // What must not exist is a mark claiming the NEW head, which nothing reached.
        assert!(
            rec.settled_at.values().all(|m| m.head != "bbbbbbbbbbbb2"),
            "a refusal is not a convergence: {:?}",
            rec.settled_at
        );
    }

    // And the row keeps being fetched until it really is converged.
    let fetches = git.fetches();
    forge_interval_elapsed(&rig);
    update_now(&ctx, &plane, &dir, &git);
    if !landed_on_new_head {
        assert!(
            git.fetches() > fetches,
            "unfinished work is retried, not written off"
        );
    }
}

#[test]
fn a_settlement_mark_is_never_trusted_over_a_deleted_store() {
    // The mark lives in machine state, which outlives any one checkout. Delete the project store
    // (or reclone at the same path) and the mark is still there while the bytes it vouches for are
    // not — so honoring it unread would leave the fresh checkout with the row installed nowhere and
    // the lane convinced there was nothing to do.
    let rig = Rig::new("repo-store-deleted");
    let proj = project(
        "proj-store-deleted",
        "[bundles]\n\"github.com/o/r\" = \"*\"\n",
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let pctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&pctx, &plane, &dir, &git);
    assert!(proj.0.join(".claude/skills/alpha/SKILL.md").exists());
    let mark = crate::forge_check::read(&rig.fs, &rig.layout());
    assert!(
        mark.sources.values().any(|c| !c.settled_at.is_empty()),
        "the pass recorded a settlement: {:?}",
        mark.sources
    );

    // The checkout is wiped and recloned at the same path — the machine's mark survives it.
    std::fs::remove_dir_all(proj.0.join(".topos")).unwrap();
    std::fs::remove_dir_all(proj.0.join(".claude")).unwrap();
    assert!(
        !crate::forge_check::read(&rig.fs, &rig.layout())
            .sources
            .values()
            .all(|c| c.settled_at.is_empty()),
        "the mark is still on file"
    );

    // The fresh checkout must be installed, not written off.
    forge_interval_elapsed(&rig);
    let out = update_now(&pctx, &plane, &dir, &git);
    assert!(
        proj.0.join(".claude/skills/alpha/SKILL.md").exists(),
        "a mark whose bytes are gone proves nothing: {:?}",
        out.warnings
    );
}

#[test]
fn every_check_is_recorded_and_visible_to_status_and_list() {
    // ACCEPTANCE 4's first two tiers: recorded ALWAYS, and shown on demand — the answer to "is
    // this even working?", from local state alone.
    let rig = Rig::new("repo-recorded");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    let status = ops::status_snapshot(&ctx, ops::ScopeView::All).unwrap();
    let row = status
        .forge
        .iter()
        .find(|f| f.source == "github.com/o/r")
        .unwrap_or_else(|| panic!("status shows the source: {:?}", status.forge));
    assert_eq!(row.checked_at, rig_now(&rig));
    assert_eq!(row.answered_at, Some(rig_now(&rig)));
    assert_eq!(row.commit.as_deref(), Some("aaaaaaaaaaaa1"));
    assert!(row.error.is_none(), "{row:?}");

    // A failure is visible in exactly the same place, rather than only in a receipt nobody kept.
    git.fail_with(FetchFault::Unreachable);
    forge_interval_elapsed(&rig);
    quiet_sweep(&ctx, &plane, &dir, &git);
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    let row = listed
        .data
        .forge
        .iter()
        .find(|f| f.source == "github.com/o/r")
        .unwrap_or_else(|| panic!("list shows the source: {:?}", listed.data.forge));
    assert!(row.error.is_some(), "{row:?}");
    assert_eq!(
        row.answered_at,
        Some(rig_now(&rig)),
        "the last ANSWER survives a later failure"
    );
    // And the LISTING says it ON THE ROWS THE SOURCE FROZE, never as a block under the list: the
    // reader is already looking at the row, and the row is what stopped moving. `[not responding]`
    // REPLACES `[current]` — current would be measured against an answer nobody got — and the last
    // ANSWER is what says how stale the copy is (the sweep's clock advances on failure, so a
    // "last checked" here would always be recent and read like a blip to ignore).
    let text = crate::render::list_tty(&listed);
    assert!(text.contains("[not responding]"), "{text}");
    assert!(text.contains("from github.com/o/r"), "{text}");
    assert!(text.contains("last answered:"), "{text}");
    assert!(!text.contains("[current]"), "{text}");
    assert!(!text.contains("external sources:"), "{text}");
}

#[test]
fn a_healthy_external_source_is_named_on_its_rows_and_nowhere_else() {
    // The other half of the rule above: nothing is wrong, so the listing is ONE list of bundles.
    // The repository appears in the `from` column of the rows it delivers and gets no block of its
    // own — the block used to restate, under the list, an identity every row already carried.
    let rig = Rig::new("repo-healthy-quiet");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    update_now(&ctx, &plane, &dir, &git);

    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    assert!(
        listed.data.forge.iter().all(|f| f.error.is_none()),
        "{:?}",
        listed.data.forge
    );
    let text = crate::render::list_tty(&listed);
    assert!(text.contains("from github.com/o/r"), "{text}");
    assert!(!text.contains("external sources:"), "{text}");
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

    // A fresh checkout spells the same repo row: the update installs into the PROJECT's own store.
    // Scopes are unblended, so the machine's landing neither helps nor hinders the checkout's.
    let proj = project("proj-granted", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let pctx = rig.ctx_at(Some(&proj.0));
    let out = update_now(&pctx, &plane, &dir, &git);
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

// =================================================================================================
// Cleaning.
// =================================================================================================

#[test]
fn a_dropped_feed_row_uninstalls_clean_copies_and_keeps_edited_ones() {
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

    // A CLEAN copy: dropping the feed row uninstalls it now, and the receipt row speaks in
    // destinations — the removed dir, the qualified name — never an agent.
    rig.write_global("[bundles]\n");
    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert_eq!(
        row.display.as_deref(),
        Some(&format!("@{WS_NAME}/deploy")[..])
    );
    assert_eq!(row.destinations, vec![placed.display().to_string()]);
    assert!(row.kept.is_empty(), "{:?}", row.kept);
    assert!(!placed.exists(), "the clean person-scope copy is retired");
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "every sidecar byte stays"
    );

    // An EDITED copy: re-adopt, edit, drop again — the edit is the person's own work, so the
    // copy STAYS IN PLACE (disclosed on the row), with a snapshot in the store behind it.
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    assert!(placed.join("SKILL.md").exists());
    let before = store_versions(&rig.layout(), "s_deploy");
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    rig.write_global("[bundles]\n");
    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert!(row.destinations.is_empty(), "{:?}", row.destinations);
    assert_eq!(row.kept, vec![placed.display().to_string()]);
    assert!(placed.join("SKILL.md").exists(), "the edited copy stays");
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# my edit\n",
        "byte-for-byte the person's edit"
    );
    assert!(
        store_versions(&rig.layout(), "s_deploy") > before,
        "the kept edit is snapshotted into the store too"
    );

    // Idempotent: the sweep after has nothing left to retire — the kept copy is the person's own
    // file now, not something to re-announce every session start.
    let out2 = sweep(&ctx, &plane, &dir);
    assert!(
        !out2
            .data
            .skills
            .iter()
            .any(|s| matches!(s.action, PullAction::Removed | PullAction::Withdrawn)),
        "{:?}",
        out2.data.skills
    );
    assert!(placed.join("SKILL.md").exists());
}

#[test]
fn a_new_off_row_cleans_its_bundles_placements_and_keeps_the_bytes() {
    let rig = Rig::new("offclean");
    rig.seed_session();
    rig.seed_feed();
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
// `--force` (the rebuild repair).
// =================================================================================================

#[test]
fn rebuild_absorbs_the_edit_before_it_re_projects() {
    let rig = Rig::new("rebuild");
    rig.seed_session();
    rig.seed_feed();
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

/// The BUILT-IN survives a `--force`, in the same invocation.
///
/// Its force-sync from the binary IS its rebuild, and it has already run by the time the repair
/// starts. Re-projecting it the ordinary way — drop the dirs, let the sweep write them back —
/// therefore deletes a folder nothing later in the run puts back: the person asks for a repair and
/// their agents lose the one skill that teaches them what topos is, silently, until the next bare
/// sweep happens to heal it.
#[test]
fn force_leaves_the_built_in_placed() {
    let rig = Rig::new("rebuild-builtin");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    // The same force-sync every real invocation runs before the reconcile.
    ops::ensure_builtin(&ctx).unwrap();
    let builtin: Vec<std::path::PathBuf> = ops::builtin_placement_dirs(&ctx)
        .unwrap()
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    assert!(!builtin.is_empty(), "the built-in is placed to begin with");
    for dir in &builtin {
        assert!(dir.join("SKILL.md").exists(), "{}", dir.display());
    }

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
    for dir in &builtin {
        assert!(
            dir.join("SKILL.md").exists(),
            "the built-in is still placed after --force: {}",
            dir.display()
        );
    }
}

/// A rebuild of a bundle whose merge is still UNDECIDED leaves its folders exactly as they are.
///
/// The ordinary repair works by dropping every placement dir and letting the sweep re-project the
/// bundle pristine — but a blocked bundle has no pristine version to project: the sweep stops at
/// the block and writes no placement, so dropping the dirs would empty every agent folder and
/// leave it empty until the merge is settled. Nothing is lost either way (the bytes are
/// snapshotted), but "your agents have this skill" would stop being true because of a repair. So
/// the rebuild stands aside and names the two exits — both of which rewrite every managed
/// placement on their way out, which is the repair the person came for.
#[test]
fn rebuild_leaves_a_blocked_bundle_alone_and_names_both_exits() {
    let rig = Rig::new("rebuild-blocked");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# deploy\n");
    let v2 = one_file(b"# deploy, theirs\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.join("SKILL.md").exists());

    // My own edit to the same file the team is about to change → a conflict on the next sweep.
    std::fs::write(placed.join("SKILL.md"), b"# deploy, mine\n").unwrap();
    plane.serves(vec![DeliverySkill {
        version_id: v2.id,
        generation: 2,
        bundle_digest: v2.digest,
        ..delivered("s_deploy", "deploy", &v2)
    }]);
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        out.data.skills.iter().map(|s| s.action).collect::<Vec<_>>(),
        vec![PullAction::Conflicted]
    );
    let sp = rig
        .layout()
        .published(&crate::id::SkillId::parse("s_deploy").unwrap());
    assert!(sp.conflict.exists(), "the block is recorded");
    let mine = snapshot_dir(&placed);
    assert_eq!(
        mine,
        vec![("SKILL.md".to_owned(), b"# deploy, mine\n".to_vec())]
    );

    // THE POINT: `--force` touches nothing here, and says why in the scope's own spelling.
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
    assert_eq!(
        snapshot_dir(&placed),
        mine,
        "a rebuild must not empty the folders of a bundle it cannot re-project"
    );
    assert!(sp.conflict.exists(), "and the block still stands");
    // A bundle waiting on a person is NOT a failure: nothing broke, no retry helps, and the run's
    // exit status must not say otherwise.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // No internal code, and each way out on its own line, runnable exactly as printed.
    assert_eq!(out.decisions.len(), 1, "{:?}", out.decisions);
    assert_eq!(
        out.decisions[0].name, "deploy",
        "the bundle leads its own row"
    );
    assert_eq!(
        out.decisions[0].line,
        "waiting on a merge decision, so its folders were left as they are"
    );
    assert_eq!(
        out.decisions[0].detail,
        vec![
            "settle it first, then rebuild:".to_owned(),
            "  topos update -g deploy --keep-mine".to_owned(),
            "  topos update -g deploy --reset".to_owned(),
        ]
    );
    // The receipt renders it as a row of this run, padded with the rest, and counts it as an
    // answer owed — never as something that failed.
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
    );
    assert!(
        tty.contains(
            "deploy   waiting on a merge decision, so its folders were left as they are\n    \
             settle it first, then rebuild:\n      topos update -g deploy --keep-mine\n      topos \
             update -g deploy --reset"
        ),
        "{tty}"
    );
    assert!(tty.contains("waiting on you"), "{tty}");
    assert!(!tty.contains("failed"), "{tty}");
    // And the HEADER says what the run did: it checked. Nothing moved here — the whole point of
    // the assertions above is that the folders were left as they are — so `updated machine-wide`
    // was a claim the rows under it contradicted.
    assert!(tty.starts_with("checked machine-wide\n"), "{tty}");
    // The row in the SAME receipt still names the untouched folder — the rebuild changed nothing
    // about what the conflict row can promise.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.action == PullAction::Conflicted)
        .expect("the block is re-disclosed");
    assert_eq!(
        row.merge.as_ref().map(|m| m.placements.len()),
        Some(1),
        "{:?}",
        row.merge
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

/// The fault the freshness cache recorded for a workspace's LAST exchange — by ID, the way the
/// cache is keyed.
fn recorded_fault(rig: &Rig, workspace_id: &str) -> Option<ExchangeFault> {
    sync_status::read(&rig.fs, &rig.layout())
        .unwrap()
        .workspaces
        .get(workspace_id)
        .and_then(|e| e.last_exchange_fault)
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

    assert_eq!(
        recorded_fault(&rig, WS),
        None,
        "a landed exchange records no fault"
    );

    // Now the server is gone.
    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert_eq!(out.unreachable[0].workspace_id, WS);
    assert_eq!(out.unreachable[0].workspace_name, WS_NAME);
    // The transient warning below is only half of it: the fault OUTLIVES this run, under the same
    // id the freshness facts sit under, so a later read can still say the exchange did not land.
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Unreachable));
    assert_eq!(
        sync_status::read(&rig.fs, &rig.layout())
            .unwrap()
            .workspaces[WS]
            .last_delivery_at,
        Some(rig_now(&rig)),
        "the fault lands BESIDE the freshness facts — it never overwrites the entry"
    );

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
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Unavailable));

    // The OTHER half of the same variant: the answer got cut off part-way.
    plane.serve_truncated();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out),
        vec![unavailable_line],
        "a truncated body is the same variant and reads the same"
    );
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Unavailable));

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
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Malformed));

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
    // And no row is BORN to carry the fault either — the same philosophy: there is nothing here a
    // later read could be answering staler than.
    assert!(
        !sync_status::read(&rig.fs, &rig.layout())
            .unwrap()
            .workspaces
            .contains_key(WS),
        "a never-delivered workspace gains no freshness row from a failure"
    );
}

/// A landed exchange CLEARS a recorded fault: the successful write replaces a workspace's whole
/// entry, so the fault cannot outlive the run that fixed it (a stale one would have `log` warning
/// forever about a server that came back).
#[test]
fn a_landed_exchange_clears_the_recorded_fault() {
    let rig = Rig::new("faultclear");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    sweep(&ctx, &plane, &dir);
    plane.serve_malformed();
    sweep(&ctx, &plane, &dir);
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Malformed));

    // The server comes back.
    plane.serve(empty_snapshot());
    sweep(&ctx, &plane, &dir);
    assert_eq!(recorded_fault(&rig, WS), None);
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
    rig.seed_feed();
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
    // Standing outside any project, the searched scope was the machine — and the line says so.
    assert!(
        err.to_string()
            .contains("'nonesuch' is not in your machine-wide set"),
        "{err}"
    );
}

// =================================================================================================
// Per-scope forge stores, the absent-member clean, and the dropped-row clean.
// =================================================================================================

#[test]
fn a_repo_row_converges_in_place_inside_the_forge_interval() {
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
    // An `add` is not a lane round — it schedules nothing. The first sweep after it therefore
    // still checks; this one is what starts the clock.
    quiet_sweep(&ctx, &plane, &dir, &git);

    // Tracked now: inside its interval the silent sweep converges in place without dialing, and
    // the hand-run update answers the same way because nothing upstream moved.
    let dialed = git.probes() + git.fetches();
    let quiet = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        dialed,
        "inside the interval, nothing is asked"
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
    let out = update_now(&ctx, &plane, &dir, &git);
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
        &Default::default(),
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
        &Default::default(),
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
    // The person's OWN choice ended it, so the row reads `removed` (with the destination that
    // left, `~`-abbreviated), never "withdrawn upstream".
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "alpha" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert_eq!(row.destinations, vec!["~/.claude/skills/alpha".to_owned()]);
    // Idempotent: nothing left to retire on the sweep after.
    let out2 = sweep(&ctx, &plane, &dir);
    assert!(
        !out2
            .data
            .skills
            .iter()
            .any(|s| matches!(s.action, PullAction::Removed | PullAction::Withdrawn)),
        "{:?}",
        out2.data.skills
    );
}

// =================================================================================================
// Per-scope mentions (the person clean is never shielded by a project mention).
// =================================================================================================

/// Driven through the BACKGROUND sweep: only a both-scope run converges the person scope from
/// inside a project, and the cross-scope shielding question only arises when it does.
#[test]
fn a_project_mention_never_shields_a_person_scope_clean() {
    let rig = Rig::new("scope-mention");
    rig.seed_session();
    rig.seed_feed();
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
    sweep_both(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.exists(), "the person feed installed its copy");

    // The feed withdraws the bundle. The person-scope copy must retire — the PROJECT's mention
    // of the same name is a different scope's business and shields nothing.
    plane.serves(Vec::new());
    let out = sweep_both(&ctx, &plane, &dir);
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
        out.advisories
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
        &Default::default(),
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
    match ops::add_reference(
        ctx,
        &connect(plane, dir),
        None,
        raw,
        true,
        false,
        &Default::default(),
    )
    .unwrap()
    {
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
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.codex/skills\"] }}\n"
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
fn an_off_switch_over_a_draft_applies_with_the_kept_disclosure() {
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
        &Default::default(),
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

    // DRAFTED: the edited copy is KEPT IN PLACE by the eager uninstall, so nothing is
    // loss-shaped — the switch applies immediately, the note disclosing the kept copy.
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Applied(data) => {
            assert!(data.applied);
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("local edits"), "{note}");
            assert!(note.contains("stays in place"), "{note}");
            // The edited copy really is still on disk.
            assert_eq!(
                std::fs::read(placed.join("SKILL.md")).unwrap(),
                b"# my edit\n"
            );
        }
        other => panic!("a drafted off switch applies with the kept disclosure: {other:?}"),
    }
}

#[test]
fn a_feed_drop_applies_immediately_and_uninstalls_in_the_same_invocation() {
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

    // DRAFTED: the edited copy is KEPT IN PLACE, so nothing is loss-shaped — a bare drop applies
    // immediately, uninstalls in the SAME invocation, and the receipt carries the kept copy and
    // the sugared undo.
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    let token = format!("@{WS_NAME}");
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        std::slice::from_ref(&token),
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    let data = match out {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a bare feed drop applies immediately: {other:?}"),
    };
    assert_eq!(
        data.undo,
        vec!["topos", "add", "-g", &format!("@{WS_NAME}")],
        "the undo line spells the sugar at the machine's one host"
    );
    let u = data
        .uninstalled
        .iter()
        .find(|u| u.name == format!("@{WS_NAME}/deploy"))
        .unwrap_or_else(|| panic!("{:?}", data.uninstalled));
    assert!(u.destinations.is_empty(), "{:?}", u.destinations);
    assert_eq!(u.kept, vec![placed.display().to_string()]);
    assert!(
        placed.join("SKILL.md").exists(),
        "the edited copy stays in place"
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains(&format!("{HOST}/{WS_NAME}")), "{text}");

    // CLEAN: a fresh unedited install leaves in the SAME invocation — the receipt names the
    // destination that went, and the dir is gone before the command returns. (The kept copy is
    // the person's own file now; they delete it themselves before re-adopting the feed.)
    std::fs::remove_dir_all(&placed).unwrap();
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    assert!(placed.join("SKILL.md").exists());
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[token],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    let data = match out {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean feed drop applies immediately: {other:?}"),
    };
    let u = data
        .uninstalled
        .iter()
        .find(|u| u.name == format!("@{WS_NAME}/deploy"))
        .unwrap_or_else(|| panic!("{:?}", data.uninstalled));
    assert_eq!(u.destinations, vec![placed.display().to_string()]);
    assert!(u.kept.is_empty(), "{:?}", u.kept);
    assert!(!placed.exists(), "the copies left in this invocation");
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
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = {{ dest = [\"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The describe discloses BOTH the split and the field carriage.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { data, .. } => {
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("dest settings carry"), "{note}");
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
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
                f.dest.as_deref(),
                Some(&["~/.codex/skills".to_owned()][..]),
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
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = {{ dest = [\"~/.codex/skills\"] }}\n\
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
        &Default::default(),
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
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
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
            if f.dest.as_deref() == Some(&["~/.codex/skills".to_owned()][..])),
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
        vec![catalog_entry(
            added.skill_id.as_deref().unwrap_or_default(),
            "deploy",
            &v,
        )],
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
        &ops::Selection::default(),
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
        .advisories
        .iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.advisories));
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
            .advisories
            .iter()
            .any(|w| w.starts_with("GOVERNANCE_CONVERGED")),
        "{:?}",
        out2.warnings
    );
}

/// A landed publish IS the new current, and the row naming it has to say so with no sweep in
/// between. `behind` is decided OFFLINE against the delivery cache — what the plane last SERVED —
/// which only the sweep used to write, so a publish that advanced its local documents alone left
/// `list` tagging the version it had just published `behind`, contradicting the receipt printed a
/// line earlier. A landed push updates the remote-tracking ref; so does this.
#[test]
fn a_landed_publish_leaves_its_own_version_current_before_any_sweep() {
    let rig = Rig::new("publish-served");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());

    // A bundle adopted here and already governed by the workspace: the delivery cache carries the
    // sweep's row for it, naming exactly the version this copy applies (so the row reads current).
    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();
    let id = added.skill_id.clone().expect("the adopt minted a record");
    let base = added.version_id.clone().expect("and a base version");
    // The machine recipe demands it by path — the line the landed publish transfers to the
    // workspace reference.
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = \"*\"\n",
        src.canonicalize().unwrap().display()
    ));
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                last_delivery_at: Some(1),
                delivered: [(
                    id.clone(),
                    sync_status::DeliveredSkill {
                        name: "deploy".to_owned(),
                        served_version: base.clone(),
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

    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(vec![catalog_entry(&id, "deploy", &v)], Vec::new());
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let cc = |_base: &str, _tok: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        Box::new(NoContribute)
    };

    // Edit the copy and publish it — a clean landed publish (state ①).
    std::fs::write(src.join("SKILL.md"), b"# deploy, sharper\n").unwrap();
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
        &ops::Selection::default(),
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    assert_ne!(data.version_id, base, "the publish minted a new version");

    // The remote-tracking half: the cache names what was just published, not what it replaced.
    let cache = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    assert_eq!(
        cache.workspaces[WS].delivered[&id].served_version, data.version_id,
        "the delivery cache holds the version this publish landed"
    );

    // THE REPORTED BUG: the very next `list`, with no sweep in between, must not tag the published
    // version `behind` its own publish.
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    let row = listed
        .data
        .scopes
        .iter()
        .flat_map(|s| &s.rows)
        .find(|r| r.skill == "deploy")
        .unwrap_or_else(|| panic!("the published row is listed: {:?}", listed.data.scopes));
    assert_eq!(row.version_id, data.version_id, "{row:?}");
    assert_eq!(
        row.status,
        Some(topos_types::results::SkillStatus::Current),
        "the row agrees with the receipt that just printed: {row:?}"
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
        vec![catalog_entry(
            added.skill_id.as_deref().unwrap_or_default(),
            "deploy",
            &v,
        )],
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
        &ops::Selection::default(),
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
        .advisories
        .iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.advisories));
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
        vec![catalog_entry(
            added.skill_id.as_deref().unwrap_or_default(),
            "deploy",
            &v,
        )],
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
        &ops::Selection::default(),
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
        vec![catalog_entry(
            added.skill_id.as_deref().unwrap_or_default(),
            "deploy",
            &v,
        )],
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
        &ops::Selection::default(),
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
        .advisories
        .iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.advisories));
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
            dest_root: None,
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
            dest_root: None,
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
        .advisories
        .iter()
        .find(|w| w.starts_with("GIT_VISIBLE"))
        .unwrap_or_else(|| panic!("the visibility disclosure: {:?}", out.advisories));
    assert!(w.contains("deploy"), "{w}");
}

#[test]
fn an_interactive_add_of_a_git_source_always_describes_first() {
    // The describe is a property of the VERB, not of the origin. `add` is where a person is
    // present to read what a repo holds and where it would land, so every interactive add says it
    // — including a re-add of a source already tracked here, and including one whose row is
    // already standing in the file.
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
        &Default::default(),
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
            panic!("an interactive add of a git source always describes first")
        }
    }
    assert!(
        !proj.0.join(".claude/skills/alpha").exists(),
        "the describe installs nothing"
    );

    // `--yes` applies.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        true,
        &Default::default(),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(proj.0.join(".claude/skills/alpha/SKILL.md").exists());

    // Tracked now — and a further BARE add of the SAME origin still describes. That is the
    // deliberate cost of making the shape a property of the verb: an answer a person asked for is
    // never skipped because the machine happens to have seen the source before.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r/beta",
        false,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Described { .. } => {}
        ops::AddRefOutcome::Applied(_) => {
            panic!("a repeat add describes too — the shape belongs to the verb")
        }
    }
}

// =================================================================================================
// The SELECTOR import (`-s`/`-a`) — the same describe-first shape, the same per-scope store.
// =================================================================================================

#[test]
fn a_selector_import_describes_first_then_installs() {
    // A selector narrows WHICH members land and WHERE; it is not a way around reading what a
    // source holds. So `add owner/repo -s alpha` DESCRIBES, puts nothing in place, and applies
    // only under `--yes` — exactly like the bare reference arm.
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
    // A later bare reference add of the same origin describes too — the two-phase shape belongs
    // to the verb, so it never depends on what the machine happens to have seen before.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r/beta",
        true,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Described { .. } => {}
        ops::AddRefOutcome::Applied(_) => panic!("an interactive add describes first"),
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
    let probes_before = git.probes();

    // The update therefore CONVERGES it — and the comparison is the cheap probe, so no archive
    // moves at all.
    let out = update_now(&ctx, &plane, &dir, &git);
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
        fetches_before,
        "nothing re-installed, and nothing downloaded to find that out"
    );
    assert_eq!(git.probes(), probes_before + 1, "one probe to compare");
}

/// The store-routing helper the reconcile uses, reachable from the suite.
fn ops_ctx_with_layout<'a>(ctx: &'a Ctx<'a>, layout: &Layout) -> Ctx<'a> {
    Ctx {
        progress: crate::progress::silent(),
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
    match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &targets,
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Described { data, .. } => {
            assert_eq!(data.items.len(), 1, "one line split once: {:?}", data.items);
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("alpha") && note.contains("beta"), "{note}");
            assert!(note.contains("new rows are written for gamma"), "{note}");
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    assert!(matches!(
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &targets,
            None,
            true,
            &Default::default()
        )
        .unwrap(),
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

/// The ways out an ambiguity refusal offers, as the runnable `topos remove …` lines both surfaces
/// print. The candidates ride the ACTIONS (one per candidate, the failing verb re-spelled), not the
/// sentence — so this, not `err.to_string()`, is where "every way out is named, paste-ready" is
/// proven.
fn ways_out(err: &ClientError) -> String {
    // The invocation these suites make: `topos remove <token>` — one verb, one target, so the
    // reconstruction is faithful and the ways out are offered.
    let argv = vec!["remove".to_owned(), "deploy".to_owned()];
    crate::render::err_hint_tty("remove", &argv, err).unwrap_or_default()
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
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = ways_out(&err);
    assert!(msg.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{msg}");
    assert!(msg.contains("github.com/o/deploy"), "{msg}");
    // Nothing moved.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("github.com/o/deploy"), "{text}");

    // Spelled in full, exactly one row answers — and it applies.
    let one = format!("{HOST}/{WS_NAME}/deploy");
    assert!(matches!(
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &[one],
            None,
            true,
            &Default::default()
        )
        .unwrap(),
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
    );
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    assert!(
        plan.targets.iter().all(|t| t.dir.starts_with(&ok.0)),
        "{:?}",
        plan.targets
    );
}

// =================================================================================================
// The applied report's cross-store pick is deterministic; cross-scope STALENESS is disclosed and
// a deliberate pin is not.
// =================================================================================================

#[test]
fn a_bundle_held_at_two_versions_reports_the_person_copy_and_says_nothing_about_a_pin() {
    // The wire carries ONE row per (session, bundle). Which store answers must not depend on which
    // checkout the update happened to run from — the PERSON store answers whenever it holds the
    // bundle. The OTHER store's version is a DIFFERENCE, and here a deliberate one: the project
    // row is pinned, so no line is earned. Difference is the design working; only staleness is
    // news, and only when a command from here would fix it.
    let rig = Rig::new("split-report");
    rig.seed_session();
    rig.seed_feed();
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
    let out = sweep_both(&ctx, &plane, &dir);

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
        .find(|(id, ..)| id == "s_deploy")
        .unwrap_or_else(|| panic!("the bundle is reported: {reported:?}"));
    assert_eq!(row.1, topos_core::digest::to_hex(&v2.id), "{reported:?}");
    // The pin is a CHOICE, so the receipt says nothing about it: no staleness entry, no line, and
    // above all no internal warning code on a run where everything went right.
    assert!(
        out.data.behind_elsewhere.is_empty(),
        "a pinned row is deliberate, never behind: {:?}",
        out.data.behind_elsewhere
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
    );
    assert!(!tty.contains("behind"), "{tty}");
    assert!(!tty.contains("VERSION_SPLIT"), "{tty}");
    assert!(!tty.contains(&v1_hex[..12]), "{tty}");
}

#[test]
fn the_machine_copy_left_behind_by_a_project_update_earns_the_counted_trailer() {
    // The other half of the rule: a copy that differs because it is simply OLD — no pin, no `off`,
    // nothing deliberate — IS news, and the receipt closes with the count and the one command
    // that fixes it. This run drives the PROJECT (the scope rule), so the machine-wide copy is
    // exactly what nothing here touched.
    let rig = Rig::new("behind-machine");
    rig.seed_session();
    rig.seed_feed();
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let proj = project(
        "proj-behind",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());

    // Both scopes land v1 (the background sweep drives both).
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep_both(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(rig.work.0.join("skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n"
    );

    // Current moves; a HAND-RUN update from inside the checkout drives the project alone.
    let mut served = delivered("s_deploy", "deploy", &v2);
    served.generation = 2;
    plane.serves(vec![served]);
    let mut listed = catalog_entry("s_deploy", "deploy", &v2);
    listed.generation = 2;
    let dir = FakeDirectory::new(vec![listed], Vec::new());
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the scope you stand in is the one that moves"
    );
    assert_eq!(
        std::fs::read(rig.work.0.join("skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the machine-wide copy was never driven"
    );
    assert_eq!(
        out.data.behind_elsewhere,
        vec![topos_types::results::BehindElsewhere {
            bundle: "deploy".to_owned(),
            project_dir: None,
        }],
        "the machine's own copy is the one behind"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
    );
    assert!(
        tty.ends_with("1 bundle behind machine-wide — `topos update -g` updates it."),
        "{tty}"
    );

    // Run THAT command and the line is gone — a trailer nobody can clear is a nag, not a receipt.
    let after = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        after.data.behind_elsewhere.is_empty(),
        "{:?}",
        after.data.behind_elsewhere
    );
}

/// The PINNED half of the staleness rule, reached for real. The two tests around it never touch
/// it: one pins the scope the run DRIVES (so the driven-scope filter answers first and the pin is
/// never consulted), the other uses `"off"`. Here the pin sits on the scope this run leaves alone,
/// which is the only arrangement in which the pin itself decides — and the control run, the same
/// fixture with `"*"` in place of the pin, proves the silence is the PIN's doing and not the
/// fixture's.
#[test]
fn a_pin_on_the_scope_this_run_left_alone_is_never_reported_behind() {
    let rig = Rig::new("behind-pinned");
    rig.seed_session();
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let v1_hex = topos_core::digest::to_hex(&v1.id);
    let proj = project(
        "proj-pinned",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());

    // The MACHINE recipe pins the bundle to v1; the project takes whatever is current. Both land
    // v1 on the first sweep, which drives both scopes.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"{v1_hex}\"\n"
    ));
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep_both(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(rig.work.0.join("skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n"
    );

    // Current moves; a hand-run update from inside the checkout drives the PROJECT alone, so the
    // machine scope is exactly the one nothing here touched — the arrangement that makes the pin
    // the deciding fact.
    let mut served = delivered("s_deploy", "deploy", &v2);
    served.generation = 2;
    plane.serves(vec![served.clone()]);
    let mut listed = catalog_entry("s_deploy", "deploy", &v2);
    listed.generation = 2;
    let dir2 = FakeDirectory::new(vec![listed.clone()], Vec::new());
    let out = sweep(&ctx, &plane, &dir2);
    assert_eq!(
        std::fs::read(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the scope you stand in is the one that moves"
    );
    assert_eq!(
        std::fs::read(rig.work.0.join("skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the pinned machine copy is materially behind v2 — deliberately"
    );
    assert!(
        out.data.behind_elsewhere.is_empty(),
        "a pinned row is a choice no `topos update` would undo: {:?}",
        out.data.behind_elsewhere
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
    );
    assert!(!tty.contains("behind"), "{tty}");

    // THE CONTROL. Same fixture, same undriven scope, same version gap — only the pin removed.
    // The line appears, so the silence above is the pin's and nothing else's.
    let rig = Rig::new("behind-unpinned");
    rig.seed_session();
    let proj = project(
        "proj-unpinned",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"
    ));
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    sweep_both(&ctx, &plane, &dir);
    plane.serves(vec![served]);
    let dir2 = FakeDirectory::new(vec![listed], Vec::new());
    let out = sweep(&ctx, &plane, &dir2);
    assert_eq!(
        out.data.behind_elsewhere,
        vec![topos_types::results::BehindElsewhere {
            bundle: "deploy".to_owned(),
            project_dir: None,
        }],
        "with no pin the same gap IS news"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
    );
    assert!(
        tty.ends_with("1 bundle behind machine-wide — `topos update -g` updates it."),
        "{tty}"
    );
}

/// The reason there is no SET-level branch in the staleness rule: a channel — the only set a
/// workspace bundle arrives through — cannot carry a pin at all. The grammar refuses it in both
/// spellings, so "a pinned set delivers this bundle" is not a state a manifest can express, and a
/// rule about it would be one no fixture could ever reach.
#[test]
fn a_channel_cannot_be_pinned_so_a_set_is_never_the_deliberate_fact() {
    let rig = Rig::new("behind-pinned-set");
    let v1 = one_file(b"# deploy v1\n");
    let v1_hex = topos_core::digest::to_hex(&v1.id);
    for body in [
        format!("[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"{v1_hex}\"\n"),
        format!(
            "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = {{ version = \"{v1_hex}\" }}\n"
        ),
    ] {
        rig.write_global(&body);
        let err = crate::manifest::scopes::person_plan(&rig.fs, &rig.layout())
            .expect_err("a pinned channel is refused at the parser");
        let message = crate::render::safe_message(&err);
        assert!(message.contains("channel takes no pin"), "{message}");
    }
}

#[test]
fn an_off_row_is_never_reported_behind() {
    // The `"off"` switch is the other deliberate spelling: the machine keeps its copy but stops
    // taking the workspace's version of it. Nothing would ever bring it to current, so a line
    // saying it is behind could never be cleared.
    let rig = Rig::new("behind-off");
    rig.seed_session();
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let proj = project(
        "proj-off",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    rig.seed_feed();
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep_both(&ctx, &plane, &dir);

    // The machine switches the bundle off AFTER its copy landed, then current moves on. The `off`
    // row is global-file-only, so the copy STAYS on disk at v1 — materially behind v2, and
    // deliberately so.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"off\"\n"
    ));
    let mut served = delivered("s_deploy", "deploy", &v2);
    served.generation = 2;
    plane.serves(vec![served]);
    let mut listed = catalog_entry("s_deploy", "deploy", &v2);
    listed.generation = 2;
    let dir = FakeDirectory::new(vec![listed], Vec::new());
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(rig.work.0.join("skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the switched-off copy is left exactly where it was"
    );
    assert!(
        out.data.behind_elsewhere.is_empty(),
        "an `off` row is a choice: {:?}",
        out.data.behind_elsewhere
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
            &Default::default(),
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

    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = ways_out(&err);
    assert!(msg.contains("github.com/o/r/deploy"), "{msg}");
    assert!(msg.contains("channels/backend"), "{msg}");

    // The EXACT spelling is an answer, never an ambiguity — otherwise the refusal would be a dead
    // end (a set member has no spelling of its own).
    let one = "github.com/o/r/deploy".to_owned();
    assert!(matches!(
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &[one],
            None,
            true,
            &Default::default()
        )
        .unwrap(),
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
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = ways_out(&err);
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
    // Each is a whole COMMAND — `topos remove <reference> --via <line>`, the verb that refused.
    assert!(
        msg.lines()
            .filter(|l| l.trim_start().starts_with("topos remove "))
            .count()
            == 2,
        "one runnable line per candidate: {msg}"
    );
    // And the refusal SENTENCE no longer inlines them: a paste-ready invocation buried in prose
    // read as prose, and the `--via` form's token boundary vanished into the comma list.
    let sentence = err.to_string();
    assert!(!sentence.contains("--via"), "{sentence}");
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
        &Default::default(),
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
        &Default::default(),
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
        &Default::default(),
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
        &Default::default(),
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

    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = ways_out(&err);
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
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["alpha".into()],
        None,
        true,
        &Default::default(),
    )
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
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &["alpha".into()],
            None,
            true,
            &Default::default()
        )
        .unwrap(),
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
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["alpha".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
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
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["alpha".into()],
        None,
        true,
        &Default::default(),
    )
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
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &["alpha".into()],
            None,
            true,
            &Default::default()
        )
        .unwrap(),
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
        &Default::default(),
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
        &Default::default(),
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
fn a_subtree_url_naming_several_skills_writes_nothing() {
    // The row is PROVEN before anything lands: a subtree that names no single skill refuses with
    // the names, and no manifest file is born for a row that cannot legally exist.
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
        &Default::default(),
    ) {
        Err(e) => e,
        Ok(_) => panic!("a subtree naming several skills names none of them"),
    };
    assert_eq!(err.code(), "AMBIGUOUS_SKILL", "{err:?}");
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
            adopted_source: false,
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
    // The refresh stood down, and said so as a DECISION — not a failure. Nothing broke, nothing
    // was lost, and no retry answers it: the person picks, and until they do the run is still a
    // clean one.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(out.decisions.len(), 1, "{:?}", out.decisions);
    assert_eq!(out.decisions[0].name, "deploy");
    assert_eq!(
        out.decisions[0].line,
        "github.com/o/r has a newer version, but your edits would be overwritten"
    );
    // ONE way out, and it is the only one that runs: a skill imported from a repository reaches
    // people who have no workspace at all, and `topos publish` refuses them at the login step.
    // Keeping the edits needs no command — nothing was overwritten.
    assert_eq!(
        out.decisions[0].detail,
        vec!["to discard them:   topos update -g deploy --reset".to_owned()]
    );
    assert!(
        !out.decisions[0]
            .ways_out
            .iter()
            .any(|w| w.iter().any(|t| t == "publish")),
        "{:?}",
        out.decisions[0].ways_out
    );
    // The whole receipt block, as a person reads it: one row, its way out, and a summary that
    // counts the row under the answer it is waiting for.
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
    );
    let expected_block = concat!(
        "deploy   github.com/o/r has a newer version, but your edits would be overwritten\n",
        "    to discard them:   topos update -g deploy --reset\n",
    );
    assert!(tty.contains(expected_block), "{tty}");
    assert!(!tty.contains("topos publish"), "{tty}");
    assert!(tty.contains("waiting on you"), "{tty}");
    // Neither internal code survives: not the refusal's, and not the source-moved disclosure's —
    // the row already says the source has a newer version, in words a person can act on.
    assert!(!tty.contains("INVALID_ARGUMENT"), "{tty}");
    assert!(!tty.contains("GIT_UPDATED"), "{tty}");
    assert!(!tty.contains('~'), "{tty}");
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
        &[],
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
        // The `-a` selection is recorded as the agent's DEST DIR (default spelling) — the row
        // says where the copy lives, not which slug once resolved it.
        crate::manifest::document::EntryValue::Fields(f) => assert_eq!(
            f.dest.as_deref(),
            Some(&["~/.codestudio/skills".to_owned()][..]),
            "{text}"
        ),
        other => panic!("the dest selection rides the row, got {other:?}: {text}"),
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
    // The one shape that cannot carry both: a whole-repo row takes `dest` and nothing else, so a
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
        &[],
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
                adopted_source: false,
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

    let mut advisories = Vec::new();
    let mut warnings = Vec::new();
    ops::handover_legacy_project_rows(
        &ctx,
        std::slice::from_ref(&proj.0),
        &mut advisories,
        &mut warnings,
    );

    assert!(advisories.is_empty(), "nothing to disclose: {advisories:?}");
    assert!(warnings.is_empty(), "and nothing failed: {warnings:?}");
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

    let mut advisories = Vec::new();
    let mut warnings = Vec::new();
    ops::handover_legacy_project_rows(
        &ctx,
        std::slice::from_ref(&proj.0),
        &mut advisories,
        &mut warnings,
    );

    // The park LANDED — the line rides the advisory channel, never the failure one: a handover
    // that worked must not make the run count a failure or exit non-zero.
    assert!(warnings.is_empty(), "{warnings:?}");
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
    let line = advisories
        .iter()
        .find(|w| w.starts_with("STATE_HANDOVER"))
        .unwrap_or_else(|| panic!("the disclosure line: {advisories:?}"));
    assert!(line.contains(&parked.display().to_string()), "{line}");
    // The bytes in the checkout were never the subject — they stay exactly where they are.
    assert!(placed.join("SKILL.md").exists());
}

// =================================================================================================
// The BARE-NAME ladder: one namespace, made of the local inventory AND the connected catalogs.
// =================================================================================================

/// The probe name the bare-name ladder resolves — deliberately IMPROBABLE. Discovery also walks
/// agent roots ambient env configures (a developer's real machine), and a genuine skill named
/// like `deploy` there would flip a Subscribe into an Adopt under `cargo test`.
const BARE: &str = "zx-bare-probe";

/// [`add_rig`]'s twin for the bare-name ladder: the one catalog skill is named [`BARE`].
fn bare_rig(tag: &str) -> (Rig, FakePlane, FakeDirectory, Version) {
    let rig = Rig::new(tag);
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_bare", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_bare", BARE, &v)], Vec::new());
    (rig, plane, dir, v)
}

/// An untracked skill a discovery walk will find: `<root>/.aider-desk/skills/<name>/SKILL.md`.
/// The harness is chosen for DETERMINISM, not realism: its user dir hangs off the passed home with
/// no env override, where a `.claude/skills` fixture vanishes the moment a developer's real
/// `CLAUDE_CONFIG_DIR` redirects claude-code's root away from the rig's home.
fn untracked_skill(root: &std::path::Path, name: &str, body: &[u8]) -> PathBuf {
    let dir = root.join(".aider-desk/skills").join(name);
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
    plan_bare_add_in(rig, plane, dir, target, subscribe, None)
}

/// [`bare_plan`] with the global `--workspace` selector (a canonicalized workspace id).
fn plan_bare_add_in(
    rig: &Rig,
    plane: &FakePlane,
    dir: &FakeDirectory,
    target: &str,
    subscribe: bool,
    workspace: Option<&str>,
) -> Result<ops::BareAddPlan, ClientError> {
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(rig.work.0.clone()),
    };
    ops::plan_bare_add(
        &ctx,
        &connect(plane, dir),
        &roots,
        target,
        subscribe,
        false,
        workspace,
    )
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
    let (rig, plane, dir, _v) = bare_rig("bare-ws-only");
    match bare_plan(&rig, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Subscribe {
            reference,
            workspace,
        } => {
            // The CANONICAL host-qualified spelling — unambiguous however many servers this
            // machine is logged into.
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/{BARE}"));
            assert_eq!(workspace, WS_NAME);
        }
        other => panic!("the workspace's copy is the only thing that name can mean: {other:?}"),
    }
    // A name NOBODY has stays the plain not-found — the workspace half never invents a match.
    let err = bare_plan(&rig, &plane, &dir, "zx-bare-ghost", true).unwrap_err();
    assert_eq!(err.code(), "NO_UNTRACKED_SKILL");
}

/// Record [`BARE`] as delivered by the rig's workspace — the machine's own delivery history, which
/// is the ONLY thing that nominates a workspace for a clean local resolve's receipt disclosure (a
/// local adopt never fans out across every session's catalog just for a courtesy line).
fn seed_delivered_bare(rig: &Rig) {
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
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
}

#[test]
fn a_local_directory_wins_and_the_receipt_names_the_workspace_spelling() {
    let (rig, plane, dir, _v) = bare_rig("bare-both");
    // The SAME bytes the workspace serves, sitting untracked in the home agent dir — and the
    // delivery history that nominates the workspace for the receipt's disclosure.
    seed_delivered_bare(&rig);
    let path = untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let published = match bare_plan(&rig, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Adopt {
            path: p,
            name,
            published,
        } => {
            assert_eq!(p, path, "the bytes in front of you are what you asked for");
            assert_eq!(name, BARE);
            published.expect("the one workspace publishing the name is disclosed")
        }
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    // The receipt judges the workspace's current version against the bytes that just landed —
    // the ONE confirming catalog read carried the digest.
    let data = ops::add_with_name(&ctx, &path, Some(BARE), true).unwrap();
    let same = published.suggestion(data.bundle_digest.as_deref().unwrap_or_default());
    assert_eq!(same.reference, format!("{HOST}/{WS_NAME}/{BARE}"));
    assert_eq!(same.workspace, WS_NAME);
    assert!(same.identical, "byte-identical to what the catalog serves");

    // A local copy that has DRIFTED from the team's version is disclosed just the same — only the
    // identical claim goes away.
    let rig2 = Rig::new("bare-both-drift");
    rig2.seed_session();
    seed_delivered_bare(&rig2);
    let drifted = untracked_skill(&rig2.home.0, BARE, b"# deploy (mine)\n");
    let ctx2 = rig2.ctx_at(Some(&rig2.work.0));
    let published2 = match bare_plan(&rig2, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => published.expect("still disclosed"),
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    let data2 = ops::add_with_name(&ctx2, &drifted, Some(BARE), true).unwrap();
    assert!(
        !published2
            .suggestion(data2.bundle_digest.as_deref().unwrap_or_default())
            .identical
    );

    // A name the workspace publishes but never delivered HERE adopts with no disclosure at all —
    // the local act does not go asking every catalog for one.
    let rig3 = Rig::new("bare-both-undelivered");
    rig3.seed_session();
    untracked_skill(&rig3.home.0, BARE, b"# deploy\n");
    match bare_plan(&rig3, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => assert!(published.is_none()),
        other => panic!("a local copy adopts in place: {other:?}"),
    }
}

#[test]
fn a_name_several_workspaces_publish_refuses_naming_every_spelling() {
    let (rig, plane, dir, _v) = bare_rig("bare-two-ws");
    seed_second_session(&rig);

    let err = bare_plan(&rig, &plane, &dir, BARE, true).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_WORKSPACE");
    let message = err.to_string();
    for reference in [
        format!("{HOST}/{WS_NAME}/{BARE}"),
        format!("beta.test/ops/{BARE}"),
    ] {
        assert!(message.contains(&reference), "{message}");
    }
    // The machine-readable half: one runnable subscribe per spelling, and the references in `data`.
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    assert_eq!(
        envelope.next_actions.len(),
        2,
        "{:?}",
        envelope.next_actions
    );
    assert_eq!(
        envelope.data["references"],
        serde_json::json!([
            format!("{HOST}/{WS_NAME}/{BARE}"),
            format!("beta.test/ops/{BARE}")
        ]),
        "sorted by the spelling the message prints"
    );

    // The global `--workspace` selector settles it: only the named workspace is probed, so the
    // same machine subscribes deterministically to the one that was ASKED — never the other.
    match plan_bare_add_in(&rig, &plane, &dir, BARE, true, Some("w_ops")).unwrap() {
        ops::BareAddPlan::Subscribe {
            reference,
            workspace,
        } => {
            assert_eq!(reference, format!("beta.test/ops/{BARE}"));
            assert_eq!(workspace, "ops");
        }
        other => panic!("the selector names the workspace: {other:?}"),
    }

    // With a LOCAL copy in hand the adopt still wins — and carries no suggestion, because naming
    // one of two teams' copies beside an adopt that already landed would be a guess.
    untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    match bare_plan(&rig, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => assert!(published.is_none()),
        other => panic!("a local copy adopts in place: {other:?}"),
    }

    // A LOCAL ambiguity on top of the two workspaces: the hint's runnable subscribes cover EVERY
    // spelling — advertising only one would settle an ambiguity nobody resolved.
    untracked_skill(&rig.work.0, BARE, b"# deploy\n");
    let err = bare_plan(&rig, &plane, &dir, BARE, true).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_SCOPE");
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    let subscribes: Vec<_> = envelope
        .next_actions
        .iter()
        .filter(|a| a.argv.len() == 4 && a.argv[1] == "add")
        .map(|a| a.argv[2].clone())
        .collect();
    assert_eq!(
        subscribes,
        vec![
            format!("{HOST}/{WS_NAME}/{BARE}"),
            format!("beta.test/ops/{BARE}")
        ],
        "{:?}",
        envelope.next_actions
    );
}

#[test]
fn a_local_ambiguity_discloses_the_team_copy_and_judges_the_bytes() {
    let (rig, plane, dir, _v) = bare_rig("bare-scope");
    // The same name in the home AND project dirs of ONE harness — `@harness` cannot split them.
    untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    untracked_skill(&rig.work.0, BARE, b"# deploy\n");

    let err = bare_plan(&rig, &plane, &dir, BARE, true).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_SCOPE");
    let message = err.to_string();
    assert!(
        message.contains("byte-identical") && message.contains(&format!("{HOST}/{WS_NAME}/{BARE}")),
        "every copy matches the served version, so the message collapses toward the subscribe: \
         {message}"
    );
    // The subscribe rides beside the inventory read as a runnable action.
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    assert_eq!(
        envelope.next_actions.last().map(|a| a.argv.clone()),
        Some(vec![
            "topos".to_owned(),
            "add".to_owned(),
            format!("{HOST}/{WS_NAME}/{BARE}"),
            "--json".to_owned(),
        ]),
        "{:?}",
        envelope.next_actions
    );

    // One copy DRIFTS: the disclosure stands, the identical claim does not.
    untracked_skill(&rig.work.0, BARE, b"# deploy (mine)\n");
    let message = bare_plan(&rig, &plane, &dir, BARE, true)
        .unwrap_err()
        .to_string();
    assert!(!message.contains("byte-identical"), "{message}");
    assert!(message.contains("is also published in"), "{message}");
}

#[test]
fn a_selector_or_harness_form_never_subscribes() {
    let (rig, plane, dir, _v) = bare_rig("bare-gated");
    // `-s`/`-a` narrow a LOCAL adopt, so the fully-bare gate is closed: today's answer, exactly.
    let err = bare_plan(&rig, &plane, &dir, BARE, false).unwrap_err();
    assert_eq!(err.code(), "NO_UNTRACKED_SKILL");
    // Same for a second workspace publishing it — the gate is about the FORM, not the count.
    seed_second_session(&rig);
    assert_eq!(
        bare_plan(&rig, &plane, &dir, BARE, false)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );
    // A `@harness` suffix names a local harness's dir — it cannot reach the subscribe arm at all.
    let err = bare_plan(&rig, &plane, &dir, &format!("{BARE}@aider-desk"), true).unwrap_err();
    assert_eq!(err.code(), "HARNESS_NOT_FOUND");
}

#[test]
fn a_cache_only_match_subscribes_and_an_unanswering_session_is_skipped() {
    let (rig, plane, dir, _v) = bare_rig("bare-offline");
    // The catalog cannot be read at all — a transport fault, never an existence claim.
    dir.set_unavailable(true);
    assert_eq!(
        bare_plan(&rig, &plane, &dir, BARE, true)
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
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
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
    match bare_plan(&rig, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Subscribe { reference, .. } => {
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/{BARE}"));
        }
        other => panic!("the cache alone is enough to spell the reference: {other:?}"),
    }
    // And with a local copy beside it, the disclosure carries no identical claim: a cached row
    // holds a served VERSION id, which is a different hash from a bundle digest.
    let path = untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let published = match bare_plan(&rig, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => published.expect("disclosed"),
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    let data = ops::add_with_name(&ctx, &path, Some(BARE), true).unwrap();
    assert!(
        !published
            .suggestion(data.bundle_digest.as_deref().unwrap_or_default())
            .identical
    );
}

#[test]
fn a_bare_name_subscribe_records_the_canonical_row_and_its_inverse() {
    let (rig, plane, dir, _v) = bare_rig("bare-e2e");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let ops::BareAddPlan::Subscribe {
        reference,
        workspace,
    } = bare_plan(&rig, &plane, &dir, BARE, true).unwrap()
    else {
        panic!("nothing local carries the name");
    };

    // With NO manifest covering this folder the subscribe refuses toward the two scopes — it
    // never invents a file. `topos init` is what creates one.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &reference,
        false,
        false,
        &Default::default(),
    ) {
        Err(e) => assert_eq!(e.code(), "NO_MANIFEST"),
        Ok(_) => panic!("no topos.toml covers this folder — the subscribe must refuse"),
    }
    ops::init(&ctx, false).expect("the folder's manifest");

    // The composition root's own hand-off: the resolved reference goes through the ORDINARY
    // reference arm, so the row, the delivery, and the receipt shape are the spelled-out ones.
    let mut data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &reference,
        false,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => {
            panic!("a workspace reference applies immediately")
        }
    };
    ops::push_note(
        &mut data,
        format!("resolved '{BARE}' to {reference} — {workspace} publishes it"),
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
    assert!(note.contains(BARE) && note.contains(&reference), "{note}");
}

#[test]
fn a_cache_row_of_an_ended_session_or_a_withdrawal_resolves_nothing() {
    let (rig, plane, dir, _v) = bare_rig("bare-stale-cache");
    // The catalog is unreachable, so ONLY the cache could answer — exactly the offline posture.
    dir.set_unavailable(true);

    // A leftover row from a workspace whose session has ENDED (`logout` removes the session but
    // leaves `sync_status.json` behind): resolving it would aim the subscribe at a workspace this
    // machine can no longer reach — an honest not-found is the only right answer.
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            "w_gone".to_owned(),
            sync_status::WorkspaceSync {
                host: Some("gone.test".to_owned()),
                workspace_name: Some("gone".to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
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
    assert_eq!(
        bare_plan(&rig, &plane, &dir, BARE, true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL",
        "an ended session's cache row keeps no say in the namespace"
    );

    // A row the workspace since WITHDREW — the live session stands, but withdrawn is not
    // published, whatever the cache still holds.
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
                        withdrawn: true,
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
    assert_eq!(
        bare_plan(&rig, &plane, &dir, BARE, true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );
}

#[test]
fn an_unspellable_catalog_name_stays_the_plain_not_found() {
    // A workspace CAN publish a bundle named `channels` (nothing upstream reserves it yet), but
    // the manifest grammar reserves that spelling as an incomplete channel reference — so the
    // ladder must answer exactly as before the workspace half existed, never hand the reference
    // arm a key it will refuse.
    let rig = Rig::new("bare-unspellable");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# channels\n");
    let plane = FakePlane::new(log).with_version("s_ch", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_ch", "channels", &v)], Vec::new());
    assert_eq!(
        bare_plan(&rig, &plane, &dir, "channels", true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );
}

#[test]
fn a_read_catalog_clears_the_cache_row_it_no_longer_carries() {
    let (rig, plane, _dir, _v) = bare_rig("bare-cache-invalidate");
    // The last delivery still remembers the name…
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
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
    // …but the workspace's catalog ANSWERS and no longer carries it (deleted or archived since).
    // The answered read is authoritative: the stale row must not fabricate a subscribe that
    // `add_reference` would then refuse as not-available.
    let empty = FakeDirectory::new(Vec::new(), Vec::new());
    assert_eq!(
        bare_plan(&rig, &plane, &empty, BARE, true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );

    // The clean-resolve receipt obeys the same authority: its ONE confirming read finds the name
    // gone, so the stale row's disclosure is withdrawn — never printed beside an adopt.
    untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    match bare_plan(&rig, &plane, &empty, BARE, true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => assert!(published.is_none()),
        other => panic!("a local copy adopts in place: {other:?}"),
    }
}

// =================================================================================================
// The SCOPE line: every verb acts on WHERE YOU STAND, `-g` means the machine, and no verb crosses
// that line on its own.
// =================================================================================================

/// A one-file skill source at `dir` — the adopt-in-place fixture.
fn skill_source(dir: &std::path::Path, body: &[u8]) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
}

/// The composition root's OWN path-adopt sequence, in one call (mirrors `app.rs`): the scope is
/// resolved BEFORE any byte moves, the adopt runs against that scope's store, and the row is
/// written into the SAME resolved target — never re-resolved in between.
fn scoped_path_add(
    ctx: &Ctx<'_>,
    source: &std::path::Path,
    global: bool,
) -> Result<topos_types::results::AddData, ClientError> {
    let scope = ops::add_scope(ctx, global)?;
    let sctx = ops::ctx_with_layout(ctx, &scope.layout);
    let mut data = ops::adopt_path(&sctx, &scope.target, source)?;
    ops::note_added_path_in(ctx, &mut data, &scope.target, source)?;
    Ok(data)
}

/// [`FakeDirectory`] that also ANSWERS `me` — the one read the resolver universe needs before a
/// workspace's names exist at all. (The bare fake leaves `me` absent on purpose: a publish's
/// post-landing read is best-effort there.)
#[derive(Clone)]
struct NamedDirectory(FakeDirectory);
impl DirectorySource for NamedDirectory {
    fn me(&self, ws: &str) -> Result<WireMe, ClientError> {
        Ok(WireMe {
            workspace_id: ws.to_owned(),
            name: WS_NAME.to_owned(),
            display_name: "Engineering".to_owned(),
            address: format!("{HOST}/{WS_NAME}"),
            principal: "u_test".to_owned(),
            role: "member".to_owned(),
            invited_by: None,
            session_status: Some(SESSION_ACTIVE.to_owned()),
            link_status: SESSION_ACTIVE.to_owned(),
        })
    }
    fn channels_index(&self, ws: &str) -> Result<WireChannelIndex, ClientError> {
        self.0.channels_index(ws)
    }
    fn skills_index(&self, ws: &str) -> Result<WireSkillIndex, ClientError> {
        self.0.skills_index(ws)
    }
    fn proposals_index(&self, ws: &str) -> Result<WireProposalIndex, ClientError> {
        self.0.proposals_index(ws)
    }
    fn skill_log(&self, ws: &str, s: &str) -> Result<WireSkillLog, ClientError> {
        self.0.skill_log(ws, s)
    }
    fn channel_place(&self, ws: &str, c: &str, s: &str) -> Result<(), ClientError> {
        self.0.channel_place(ws, c, s)
    }
    fn channel_unplace(&self, ws: &str, c: &str, s: &str) -> Result<(), ClientError> {
        self.0.channel_unplace(ws, c, s)
    }
    fn protect_skill(&self, ws: &str, s: &str, l: &str) -> Result<(), ClientError> {
        self.0.protect_skill(ws, s, l)
    }
    fn protect_channel(&self, ws: &str, c: &str, l: &str) -> Result<(), ClientError> {
        self.0.protect_channel(ws, c, l)
    }
    fn ack_notices(&self, ws: &str, ids: &[String]) -> Result<(), ClientError> {
        self.0.ack_notices(ws, ids)
    }
}

/// One error's next actions as `(code, argv)` pairs — the machine surface of a refusal.
fn scope_ways_out(command: &str, argv: &[&str], err: &ClientError) -> Vec<(String, Vec<String>)> {
    let argv: Vec<String> = argv.iter().map(|t| (*t).to_owned()).collect();
    crate::render::err_envelope(command, &argv, err)
        .next_actions
        .into_iter()
        .map(|a| (a.code.as_str().to_owned(), a.argv))
        .collect()
}

#[test]
fn a_bare_edit_never_writes_the_machine_file_and_a_g_edit_never_a_project_one() {
    let rig = Rig::new("zq-scope-property");
    let proj = project("zq-scope-property-proj", "[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    let global_path = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let project_path = proj.0.join(crate::manifest::MANIFEST_FILE);

    // BARE: the row lands in THIS folder's file, and the machine-wide file is never even born.
    let inside = proj.0.join("zq-inside-src");
    skill_source(&inside, b"# zq-inside\n");
    let added = scoped_path_add(&ctx, &inside, false).unwrap();
    assert_eq!(
        added.manifest.as_deref(),
        Some(project_path.to_str().unwrap())
    );
    assert_eq!(added.reference.as_deref(), Some("./zq-inside-src"));
    assert!(
        !global_path.exists(),
        "a bare add never touches the machine-wide file"
    );

    // `-g`: the row lands in the machine-wide file, and THIS folder's is byte-identical after.
    let outside = rig.work.0.join("zq-outside-src");
    skill_source(&outside, b"# zq-outside\n");
    let before = std::fs::read_to_string(&project_path).unwrap();
    let g_added = scoped_path_add(&ctx, &outside, true).unwrap();
    assert_eq!(
        g_added.manifest.as_deref(),
        Some(global_path.to_str().unwrap())
    );
    assert_eq!(
        std::fs::read_to_string(&project_path).unwrap(),
        before,
        "a `-g` add never touches a project file"
    );
    let global_text = std::fs::read_to_string(&global_path).unwrap();
    assert!(
        global_text.contains(outside.to_str().unwrap()),
        "{global_text}"
    );

    // The INVERSES obey the same line: each removal edits only the file its scope names.
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let session_connect = connect(&plane, &dir);
    let global_before = std::fs::read_to_string(&global_path).unwrap();
    ops::remove_project(
        &ctx,
        &session_connect,
        &["./zq-inside-src".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap()
    .expect("the project row is a manifest arm");
    assert!(
        !std::fs::read_to_string(&project_path)
            .unwrap()
            .contains("zq-inside-src"),
        "the project row is gone"
    );
    assert_eq!(
        std::fs::read_to_string(&global_path).unwrap(),
        global_before,
        "the project remove never touched the machine-wide file"
    );

    let project_before = std::fs::read_to_string(&project_path).unwrap();
    ops::remove_global(
        &ctx,
        &session_connect,
        &[outside.to_str().unwrap().to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    assert!(
        !std::fs::read_to_string(&global_path)
            .unwrap()
            .contains("zq-outside-src"),
        "the machine-wide row is gone"
    );
    assert_eq!(
        std::fs::read_to_string(&project_path).unwrap(),
        project_before,
        "the `-g` remove never touched a project file"
    );
}

#[test]
fn a_project_add_with_no_manifest_refuses_and_lands_nothing() {
    let rig = Rig::new("zq-nomanifest-add");
    // A folder no `topos.toml` covers — and no `.git` either, so nothing invents a repo root.
    let bare = Scratch::new("zq-nomanifest-cwd");
    let ctx = rig.ctx_at(Some(&bare.0));
    let src = bare.0.join("zq-nomanifest-src");
    skill_source(&src, b"# zq-nomanifest\n");

    let err = scoped_path_add(&ctx, &src, false).unwrap_err();
    assert_eq!(err.code(), "NO_MANIFEST");
    // Nothing landed: no manifest at either scope, no project store, no store entry.
    assert!(!bare.0.join(crate::manifest::MANIFEST_FILE).exists());
    assert!(
        !rig.layout()
            .home()
            .join(crate::manifest::MANIFEST_FILE)
            .exists()
    );
    assert!(
        !bare.0.join(".topos").exists(),
        "the project store is never minted by a refused add"
    );
    assert!(
        std::fs::read_dir(rig.layout().skills_dir())
            .map(|d| d.count())
            .unwrap_or(0)
            == 0,
        "the home store holds nothing"
    );

    // The two ways out ride as executable actions, and the retry is the caller's OWN invocation
    // with `-g` inserted after the verb (`--json` filtered out — the surfaces append their own).
    assert_eq!(
        scope_ways_out("add", &["add", "./zq-nomanifest-src", "--json"], &err),
        vec![
            (
                "INIT_PROJECT_MANIFEST".to_owned(),
                vec!["topos".to_owned(), "init".to_owned()]
            ),
            (
                "RETRY_MACHINE_WIDE".to_owned(),
                vec![
                    "topos".to_owned(),
                    "add".to_owned(),
                    "-g".to_owned(),
                    "./zq-nomanifest-src".to_owned()
                ]
            ),
        ]
    );

    // `-g` is not refused — the machine-wide file is always resolvable, and is BORN by the add.
    let mut added = scoped_path_add(&ctx, &src, true).unwrap();
    assert_eq!(
        added.manifest.as_deref(),
        Some(
            rig.layout()
                .home()
                .join(crate::manifest::MANIFEST_FILE)
                .to_str()
                .unwrap()
        )
    );

    // The row-write BACKSTOP refuses identically for a caller that skipped the scope resolve —
    // the rule lives in one place, not only at the composition root.
    match ops::note_added_path(&ctx, &mut added, &src, false) {
        Err(e) => assert_eq!(e.code(), "NO_MANIFEST"),
        Ok(()) => panic!("the project scope has no file to record into"),
    }
}

#[test]
fn a_reference_verb_with_no_manifest_refuses_while_a_bare_name_falls_through() {
    let (rig, plane, dir, _v) = bare_rig("zq-nomanifest-ref");
    let bare = Scratch::new("zq-nomanifest-ref-cwd");
    let ctx = rig.ctx_at(Some(&bare.0));
    let session_connect = connect(&plane, &dir);

    // A workspace reference is a manifest ROW — with no file to hold it, the add refuses.
    match ops::add_reference(
        &ctx,
        &session_connect,
        None,
        &format!("@{WS_NAME}/{BARE}"),
        false,
        false,
        &Default::default(),
    ) {
        Err(e) => assert_eq!(e.code(), "NO_MANIFEST"),
        Ok(_) => panic!("no topos.toml covers this folder"),
    }
    // …and with `-g` the same reference applies against the machine-wide file.
    match ops::add_reference(
        &ctx,
        &session_connect,
        None,
        &format!("@{WS_NAME}/{BARE}"),
        true,
        false,
        &Default::default(),
    ) {
        Ok(ops::AddRefOutcome::Applied(d)) => {
            assert_eq!(
                d.reference.as_deref(),
                Some(&format!("{HOST}/{WS_NAME}/{BARE}")[..])
            );
        }
        Ok(ops::AddRefOutcome::Described { .. }) => {
            panic!("a workspace reference never gates")
        }
        Err(e) => panic!("the machine-wide add applies: {}", e.code()),
    }

    // `remove` is the same line from the other side: a ROW-SPELLED token (a reference, or a path)
    // refuses toward the two scopes rather than falling through to a "no such skill".
    for token in [
        format!("@{WS_NAME}/{BARE}"),
        "./zq-nomanifest-ref".to_owned(),
    ] {
        let err = ops::remove_project(
            &ctx,
            &session_connect,
            std::slice::from_ref(&token),
            None,
            true,
            &Default::default(),
        )
        .expect_err("a row spelling with no file to hold it");
        assert_eq!(err.code(), "NO_MANIFEST", "token={token}");
        assert_eq!(
            scope_ways_out("remove", &["remove", &token], &err)[1].1,
            vec![
                "topos".to_owned(),
                "remove".to_owned(),
                "-g".to_owned(),
                token.clone()
            ]
        );
    }
    // A BARE NAME still falls through — the classic ladder owns untracked copies and the built-in.
    assert!(
        ops::remove_project(
            &ctx,
            &session_connect,
            &["zq-plain-name".to_owned()],
            None,
            true,
            &Default::default()
        )
        .unwrap()
        .is_none(),
        "a bare name is not a manifest row spelling"
    );
}

#[test]
fn a_project_add_keeps_its_history_in_the_checkouts_own_store() {
    let rig = Rig::new("zq-custody");
    let proj = project("zq-custody-proj", "[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));

    let src = proj.0.join("zq-custody-src");
    skill_source(&src, b"# zq-custody\n");
    let added = scoped_path_add(&ctx, &src, false).unwrap();
    let sid = crate::id::SkillId::parse(added.skill_id.as_deref().unwrap_or_default()).unwrap();

    // Custody follows the SCOPE: the checkout's own store holds the history, the home store none.
    let project_store = crate::sidecar::project_store_layout(&proj.0);
    assert!(
        project_store.skill_dir(&sid).join("lock.json").exists(),
        "the checkout's own store holds the version history"
    );
    assert!(
        !rig.layout().skill_dir(&sid).exists(),
        "the home store holds no entry for a project-scoped adopt"
    );
    assert_eq!(
        std::fs::read_dir(rig.layout().skills_dir())
            .map(|d| d.count())
            .unwrap_or(0),
        0,
        "the home store's skills/ is empty"
    );
    // The row is the committed, travels-with-the-repo spelling, in the folder's own file.
    assert_eq!(added.reference.as_deref(), Some("./zq-custody-src"));
    let text = std::fs::read_to_string(proj.0.join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("\"./zq-custody-src\""), "{text}");
}

#[test]
fn an_out_of_tree_source_records_absolutely_in_the_folders_own_file() {
    let rig = Rig::new("zq-outoftree");
    let proj = project("zq-outoftree-proj", "[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));

    // The source sits OUTSIDE the checkout: the reference cannot travel with the repo, so it is
    // spelled absolutely — and still recorded in THIS folder's file. The scope was the person's
    // (they omitted `-g`); it is never rerouted to the machine-wide file.
    let src = rig.work.0.join("zq-outoftree-src");
    skill_source(&src, b"# zq-outoftree\n");
    let added = scoped_path_add(&ctx, &src, false).unwrap();
    assert_eq!(added.reference.as_deref(), src.to_str());
    assert_eq!(
        added.manifest.as_deref(),
        proj.0.join(crate::manifest::MANIFEST_FILE).to_str()
    );
    assert!(
        !rig.layout()
            .home()
            .join(crate::manifest::MANIFEST_FILE)
            .exists(),
        "no machine-wide file was written"
    );
}

#[test]
fn a_dropped_rows_record_is_re_linked_while_a_standing_row_still_refuses() {
    // `remove <path>` edits the file and KEEPS the bytes, so the record outlives the row that
    // asked for it. Re-adding that folder has nothing to refuse — it re-links to the record:
    // the same id, the same lock, no second store dir, nothing minted.
    let rig = Rig::new("zq-relink");
    let proj = project("zq-relink-proj", "[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let session_connect = connect(&plane, &dir);

    let src = proj.0.join("zq-relink-src");
    skill_source(&src, b"# zq-relink\n");
    let added = scoped_path_add(&ctx, &src, false).unwrap();
    let sid = crate::id::SkillId::parse(added.skill_id.as_deref().unwrap_or_default()).unwrap();
    let store = crate::sidecar::project_store_layout(&proj.0);
    let lock_before = std::fs::read(store.skill_dir(&sid).join("lock.json")).unwrap();

    ops::remove_project(
        &ctx,
        &session_connect,
        &["./zq-relink-src".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap()
    .expect("the path row is a manifest arm");
    assert!(
        store.skill_dir(&sid).join("lock.json").exists(),
        "the record outlives the row"
    );

    let back = scoped_path_add(&ctx, &src, false).unwrap();
    assert_eq!(back.skill_id, added.skill_id, "the same record answers");
    assert_eq!(back.version_id, added.version_id, "at the version it left");
    assert_eq!(back.name, added.name);
    assert_eq!(back.reference.as_deref(), Some("./zq-relink-src"));
    assert!(
        back.note.is_some_and(|n| n.contains("earlier add")),
        "the receipt says the record was re-linked, not freshly adopted"
    );
    assert_eq!(
        std::fs::read(store.skill_dir(&sid).join("lock.json")).unwrap(),
        lock_before,
        "the lock is untouched — no version, no history, no draft snapshot moved"
    );
    assert_eq!(
        std::fs::read_dir(store.skills_dir())
            .map(|d| d.count())
            .unwrap_or(0),
        1,
        "no second record was minted"
    );

    // A folder the file STILL spells is a second adoption of one mutable dir — refused as before.
    assert_eq!(
        scoped_path_add(&ctx, &src, false).unwrap_err().code(),
        "ALREADY_TRACKED"
    );
}

#[test]
fn a_project_remove_of_a_machine_delivered_skill_refuses_toward_g() {
    // (a) The machine-wide FILE spells the row: the near-miss is named, with the `-g` spelling.
    let rig = Rig::new("zq-crossscope");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let proj = project("zq-crossscope-proj", "[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    let session_connect = connect(&plane, &dir);

    let err = ops::remove_project(
        &ctx,
        &session_connect,
        &["deploy".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .expect_err("this folder's file does not carry it");
    let message = crate::render::safe_message(&err);
    assert!(
        message.contains("topos remove -g deploy"),
        "the other scope's row is named as the fix: {message}"
    );
    assert!(
        std::fs::read_to_string(proj.0.join(crate::manifest::MANIFEST_FILE))
            .unwrap()
            .trim()
            .ends_with("[bundles]"),
        "the refusal wrote nothing"
    );

    // (b) The machine-wide file holds only the FEED row (no explicit line spells the bundle):
    // the manifest arm claims nothing, and the CLASSIC removal refuses toward the same `-g`
    // spelling.
    let rig = Rig::new("zq-crossscope-feed");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project("zq-crossscope-feed-proj", "[bundles]\n");
    // The feed has actually DELIVERED here (the machine sweep records it) — the delivered set is
    // what makes the name the feed row's claim; a workspace merely publishing a name is not a
    // demand, and would fall through to the classic not-found instead.
    sweep(&rig.ctx_at(Some(&rig.work.0)), &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    let session_connect = connect(&plane, &dir);
    assert!(
        ops::remove_project(
            &ctx,
            &session_connect,
            &["deploy".to_owned()],
            None,
            true,
            &Default::default()
        )
        .unwrap()
        .is_none(),
        "no manifest arm claims a feed-delivered name"
    );
    // The classic path resolves against the workspace's own names, so this fake answers `me`.
    let named = NamedDirectory(dir.clone());
    let named_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(named.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let dir_connect = |_: &str| -> Box<dyn DirectorySource> { Box::new(dir.clone()) };
    let connectors = ops::RemoveConnectors {
        session: &named_connect,
        directory: &dir_connect,
    };
    let err = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, true)
        .expect_err("what a workspace gives you is not this folder's to delete");
    let message = crate::render::safe_message(&err);
    assert!(
        message.contains("topos remove -g deploy"),
        "the classic path names the machine-wide switch: {message}"
    );
}

// =================================================================================================
// The TARGETED verbs over PROJECT custody: a bundle a project file delivers keeps its engine state
// in the checkout's own store, so every verb that names it by name has to look there — a home-store
// -only resolution answers NO_SUCH_SKILL, or (worse) with a same-named machine copy.
// =================================================================================================

/// A checkout whose `topos.toml` delivers `deploy`, swept to `v` — custody in the PROJECT's store,
/// nothing in the machine's. Returns the checkout and the placed dir.
fn project_custody(tag: &str, rig: &Rig, plane: &FakePlane, dir: &FakeDirectory) -> Scratch {
    let proj = project(
        tag,
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, plane, dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        !rig.layout().skill_dir(&sid("s_deploy")).exists(),
        "the machine store must hold nothing — the point of the fixture"
    );
    proj
}

fn sid(id: &str) -> crate::id::SkillId {
    crate::id::SkillId::parse(id).unwrap()
}

/// `diff` and `log`, run from inside the checkout, read the PROJECT store's copy: the diff shows
/// the edit made to the in-repo placement, and the log walks that store's version history. Both
/// used to resolve the home store alone — a project-delivered bundle was simply not found.
#[test]
fn diff_and_log_resolve_a_project_stores_copy() {
    let rig = Rig::new("proj-targeted");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project_custody("proj-targeted-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    // An edit in the IN-REPO placement — the draft `diff` must show.
    let placed = proj.0.join(".claude/skills/deploy");
    std::fs::write(placed.join("SKILL.md"), b"# deploy\nrun the canary first\n").unwrap();

    let d = ops::diff(
        &ctx,
        "deploy",
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
    )
    .unwrap();
    assert!(
        d.diff.contains("run the canary first"),
        "the project copy's draft is the diff: {}",
        d.diff
    );

    // `log` walks the PROJECT store's git history (the version the sweep applied there).
    let dirs = |_: &str| -> Box<dyn DirectorySource> { Box::new(dir.clone()) };
    let sessions = connect(&plane, &dir);
    let connectors = ops::LogConnectors {
        directory: &dirs,
        session: &sessions,
    };
    let out = ops::log(&ctx, &connectors, "deploy", ops::RowPage::unlimited()).unwrap();
    let versions: Vec<&str> = out
        .events
        .iter()
        .filter(|e| e.get("action").and_then(|x| x.as_str()) == Some("version"))
        .filter_map(|e| e.get("version_id").and_then(|x| x.as_str()))
        .collect();
    assert!(
        versions.contains(&&*topos_core::digest::to_hex(&v.id)),
        "the applied version is in the project store's log: {:?}",
        out.events
    );
}

/// A targeted `update <name>@<version>` goes back inside the PROJECT store: the checkout's copy
/// returns to the older bytes, that store's lock records it, and the machine store — which never
/// held this bundle — stays empty.
#[test]
fn a_targeted_go_back_runs_against_the_project_store() {
    let rig = Rig::new("proj-goback");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let proj = project_custody("proj-goback-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    let placed = proj.0.join(".claude/skills/deploy");
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# deploy v1\n"
    );

    // The team moves to v2 (a real pointer move — the next generation); the project store then
    // holds both versions, which is what a go-back needs.
    let mut moved = catalog_entry("s_deploy", "deploy", &v2);
    moved.generation = 2;
    let dir2 = FakeDirectory::new(vec![moved], Vec::new());
    let out = sweep(&ctx, &plane, &dir2);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# deploy v2\n"
    );

    // Back to v1, by name — resolved in the project store, applied there. The follow seam is the
    // PRODUCTION one (built from the delivery cache the sweep just wrote): it is what makes the
    // re-plan reach for a workspace-scoped placement, so an inert seam would not exercise the
    // path at all.
    let cache_follow = ops::CacheFollow::load(&rig.fs, &rig.layout());
    let ctx = Ctx {
        follow: &cache_follow,
        ..rig.ctx_at(Some(&proj.0))
    };
    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "deploy".to_owned(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v1.id)),
        },
    )
    .unwrap();
    assert_eq!(out.data.skills.len(), 1);
    assert_eq!(out.data.skills[0].action, PullAction::Held);
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the IN-REPO copy went back"
    );
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    let sp = playout.published(&sid("s_deploy"));
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    assert_eq!(lock.base_commit, topos_core::digest::to_hex(&v1.id));
    // The bytes stayed IN the checkout: the re-plan is the project's, so nothing was aimed at the
    // machine's harness dirs (which the project store's containment rail would refuse outright).
    let map = crate::doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    assert!(
        map.placements
            .iter()
            .all(|p| std::path::Path::new(p).starts_with(&proj.0)),
        "{:?}",
        map.placements
    );
    assert!(
        !rig.layout().skill_dir(&sid("s_deploy")).exists(),
        "the machine store never gained a copy"
    );
}

// =================================================================================================
// The SCOPE FLAG on the targeted verbs: a bare run reads and acts WHERE YOU STAND, `-g` on the
// machine — including for the modes that never touch the reconcile (a go-back, `--reset`).
// =================================================================================================

/// Both scopes holding the SAME bundle: the machine (through the feed) and the checkout (through
/// its own file). Sweeps both, returns the checkout — the two-copy fixture the scope flag is about.
fn both_scopes_hold_deploy(
    tag: &str,
    rig: &Rig,
    plane: &FakePlane,
    dir: &FakeDirectory,
) -> Scratch {
    let proj = project(
        tag,
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let out = sweep_both(&rig.ctx_at(Some(&proj.0)), plane, dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        rig.layout().skill_dir(&sid("s_deploy")).exists()
            && crate::sidecar::existing_project_store(&rig.fs, &proj.0)
                .is_some_and(|l| l.skill_dir(&sid("s_deploy")).exists()),
        "the fixture needs BOTH stores holding the name"
    );
    proj
}

/// With the name in BOTH stores, the reads answer with the copy you are STANDING IN. The machine
/// copy's own draft does not pull them back to the machine: the draft preference exists for
/// publish (ship the edited copy), and letting it steer a read would make `diff` from inside a
/// checkout describe another scope's edit.
#[test]
fn reads_from_inside_a_project_answer_the_project_copy() {
    let rig = Rig::new("scope-reads");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let proj = both_scopes_hold_deploy("scope-reads-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    let machine_copy = rig.work.0.join("skills/deploy");
    let project_copy = proj.0.join(".claude/skills/deploy");

    // The team moves to v2 and only the MACHINE converges (`-g`), so the two stores' histories
    // genuinely differ — a log that answered from the wrong store would show a version this
    // checkout has never held.
    let mut moved = delivered("s_deploy", "deploy", &v2);
    moved.generation = 2;
    plane.serves(vec![moved]);
    let mut moved_cat = catalog_entry("s_deploy", "deploy", &v2);
    moved_cat.generation = 2;
    let dir2 = FakeDirectory::new(vec![moved_cat], Vec::new());
    let out = sweep_scoped(&ctx, &plane, &dir2, ops::UpdateScope::Machine);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read(machine_copy.join("SKILL.md")).unwrap(),
        b"# deploy v2\n"
    );
    assert_eq!(
        std::fs::read(project_copy.join("SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the checkout stayed where its own file put it"
    );

    // Each copy carries a DIFFERENT edit; the machine's is the one the old home-first rule would
    // have shown (a drafted home copy outranked everything).
    std::fs::write(machine_copy.join("SKILL.md"), b"# machine edit\n").unwrap();
    std::fs::write(project_copy.join("SKILL.md"), b"# project edit\n").unwrap();

    let d = ops::diff(
        &ctx,
        "deploy",
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
    )
    .unwrap();
    assert!(
        d.diff.contains("project edit") && !d.diff.contains("machine edit"),
        "the diff is the copy you stand in: {}",
        d.diff
    );

    let dirs = |_: &str| -> Box<dyn DirectorySource> { Box::new(dir2.clone()) };
    let sessions = connect(&plane, &dir2);
    let connectors = ops::LogConnectors {
        directory: &dirs,
        session: &sessions,
    };
    let out = ops::log(&ctx, &connectors, "deploy", ops::RowPage::unlimited()).unwrap();
    let versions: Vec<String> = out
        .events
        .iter()
        .filter(|e| e.get("action").and_then(|x| x.as_str()) == Some("version"))
        .filter_map(|e| e.get("version_id").and_then(|x| x.as_str()))
        .map(str::to_owned)
        .collect();
    assert!(
        versions.contains(&topos_core::digest::to_hex(&v1.id)),
        "the checkout's own version is in its log: {versions:?}"
    );
    assert!(
        !versions.contains(&topos_core::digest::to_hex(&v2.id)),
        "the machine's newer version is NOT this copy's history: {versions:?}"
    );
}

/// A history read must not present itself as fresh when the workspace behind it did not answer. The
/// sweep's own warning is transient (and silent inside the staleness window), so `log` reads the
/// RECORDED fault — machine-scoped, like the device id, even for a copy the checkout's own file
/// delivers — and says it once, naming the workspace the way a person addresses it and ending in
/// the same clause the sweep would have used.
#[test]
fn a_project_copys_log_names_the_workspace_whose_last_exchange_failed() {
    let rig = Rig::new("logfault");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    // Delivered by the CHECKOUT's own file: the custody (and the action log) live in the project
    // store, while the freshness cache the fault rides is the machine's.
    let proj = project(
        "logfault-repo",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let out = sweep(&rig.ctx_at(Some(&proj.0)), &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj.0)
            .is_some_and(|l| l.skill_dir(&sid("s_deploy")).exists()),
        "the fixture needs the CHECKOUT holding the copy"
    );

    // The server answers with a failure. Nothing about the copy changes — only the record of the
    // exchange does.
    plane.serve_unavailable();
    sweep(&rig.ctx_at(Some(&proj.0)), &plane, &dir);
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Unavailable));

    // The production follow seam (built from the delivery cache the sweep wrote) is what maps this
    // copy back to its workspace.
    let cache_follow = ops::CacheFollow::load(&rig.fs, &rig.layout());
    let ctx = Ctx {
        follow: &cache_follow,
        ..rig.ctx_at(Some(&proj.0))
    };
    let dirs = |_: &str| -> Box<dyn DirectorySource> { Box::new(dir.clone()) };
    let sessions = connect(&plane, &dir);
    let connectors = ops::LogConnectors {
        directory: &dirs,
        session: &sessions,
    };
    let data = ops::log(&ctx, &connectors, "deploy", ops::RowPage::unlimited()).unwrap();
    let fault = data.sync_fault.clone().expect("the fault reaches `log`");
    assert_eq!(
        fault.workspace, WS_NAME,
        "named the way a person addresses it"
    );
    assert_eq!(fault.kind, ExchangeFault::Unavailable);

    // ONE line, the cause named exactly as the sweep would have named it — and never the id.
    let rendered = crate::render::log_tty(&data);
    assert!(
        rendered.contains(&format!(
            "note: {WS_NAME}'s last exchange with this machine did not succeed — the server did \
             not answer successfully; retry with `topos update`"
        )),
        "{rendered}"
    );
    assert!(
        !rendered.contains(WS),
        "the cache's key is never what a person is shown: {rendered}"
    );

    // The server comes back: the note goes with the fault.
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    sweep(&rig.ctx_at(Some(&proj.0)), &plane, &dir);
    let data = ops::log(&ctx, &connectors, "deploy", ops::RowPage::unlimited()).unwrap();
    assert!(data.sync_fault.is_none());
    assert!(
        !crate::render::log_tty(&data).contains("did not succeed"),
        "a landed exchange prints no such line"
    );
}

/// `-g` pins the MACHINE store for the targeted modes too, INCLUDING when only the checkout's copy
/// carries a draft — the case the home-first-unless-drafted resolution got backwards, so a `-g
/// --reset` described and then discarded the project copy's edit. Its describe also re-spells the
/// flag in the apply command: a `--yes` without `-g` would act on the other copy.
#[test]
fn a_g_reset_inside_a_project_never_reaches_the_checkouts_copy() {
    let rig = Rig::new("scope-greset");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = both_scopes_hold_deploy("scope-greset-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    let machine_copy = rig.work.0.join("skills/deploy");
    let project_copy = proj.0.join(".claude/skills/deploy");
    // ONLY the checkout's copy is edited. The machine's is clean — so `-g` has nothing to discard,
    // and anything it DOES discard came from the scope the flag excluded.
    std::fs::write(project_copy.join("SKILL.md"), b"# project edit\n").unwrap();

    let described = ops::reset(
        &ctx,
        &["deploy".to_owned()],
        false,
        ops::StoreScope::Machine,
        &ops::Selection::default(),
    )
    .expect("the machine store holds it");
    let (items, yes_argv) = match described {
        ops::ResetOutcome::Described { items, yes_argv } => (items, yes_argv),
        other => panic!("a bare reset describes: {other:?}"),
    };
    assert!(
        !items[0].drop_diff.contains("project edit"),
        "the machine copy is clean — nothing of the checkout's is disclosed as lost: {}",
        items[0].drop_diff
    );
    assert_eq!(
        yes_argv,
        vec!["topos", "update", "-g", "deploy", "--reset", "--yes"],
        "the apply command re-spells the scope flag"
    );

    ops::reset(
        &ctx,
        &["deploy".to_owned()],
        true,
        ops::StoreScope::Machine,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(project_copy.join("SKILL.md")).unwrap(),
        b"# project edit\n",
        "`-g` never reaches into the checkout"
    );
    assert_eq!(
        std::fs::read(machine_copy.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the machine copy is where the reset ran — at the team's bytes, as it already was"
    );

    // The BARE run is the other half of the line: it acts where you stand, so it is the
    // checkout's edit that is disclosed and discarded.
    let bare = ops::reset(
        &ctx,
        &["deploy".to_owned()],
        false,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    match &bare {
        ops::ResetOutcome::Described { items, yes_argv } => {
            assert!(
                items[0].drop_diff.contains("project edit"),
                "{}",
                items[0].drop_diff
            );
            assert!(!yes_argv.contains(&"-g".to_owned()), "{yes_argv:?}");
        }
        other => panic!("a bare reset describes: {other:?}"),
    }
    ops::reset(
        &ctx,
        &["deploy".to_owned()],
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(project_copy.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the bare run discarded the copy you stand in"
    );
}

/// A name only a PROJECT store holds is an honest miss under `-g` — never quietly answered by (or
/// applied to) the copy the flag excluded. The same name resolves fine without the flag.
#[test]
fn a_g_targeted_run_misses_a_project_only_name() {
    let rig = Rig::new("scope-gmiss");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project_custody("scope-gmiss-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    for (scope, want) in [
        (ops::StoreScope::Machine, false),
        (ops::StoreScope::Here, true),
    ] {
        let r = ops::reset(
            &ctx,
            &["deploy".to_owned()],
            false,
            scope,
            &ops::Selection::default(),
        );
        assert_eq!(
            r.is_ok(),
            want,
            "scope {scope:?} resolved {:?}",
            r.as_ref().err()
        );
        if !want {
            let err = r.unwrap_err();
            assert!(
                matches!(&err, ClientError::NoSuchSkill { name } if name == "deploy"),
                "got {err:?}"
            );
        }
    }
    // The go-back obeys the same line — `-g` looks only where the flag says.
    let err = ops::pull(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Machine,
            name: "deploy".to_owned(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
        },
    );
    let err = match err {
        Ok(_) => panic!("`-g` must not reach the checkout's copy"),
        Err(e) => e,
    };
    assert!(
        matches!(&err, ClientError::NoSuchSkill { name } if name == "deploy"),
        "got {err:?}"
    );
}

/// A path adopted IN PLACE under a project file keeps its recorded placement through a reset: the
/// planner's project dispatch is for bundles whose dirs the ENGINE owns, and this dir is the
/// person's own. Re-planning it into `.claude/skills/<name>` would materialize a second copy and
/// leave the edited source exactly as it was — while the receipt claimed the edits were discarded.
#[test]
fn a_reset_of_an_adopted_path_restores_the_source_dir() {
    let rig = Rig::new("zq-adopt-reset");
    let proj = project("zq-adopt-reset-proj", "[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));

    let src = proj.0.join("tools/zq-adopted");
    skill_source(&src, b"# adopted\n");
    let added = scoped_path_add(&ctx, &src, false).unwrap();
    assert_eq!(added.name, "zq-adopted");

    // The edit lands where the person works — the adopted source itself.
    std::fs::write(src.join("SKILL.md"), b"# adopted\nlocal edit\n").unwrap();
    let described = ops::reset(
        &ctx,
        &["zq-adopted".to_owned()],
        false,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    match &described {
        ops::ResetOutcome::Described { items, .. } => assert!(
            items[0].drop_diff.contains("local edit"),
            "the loss is the source dir's edit: {}",
            items[0].drop_diff
        ),
        other => panic!("a bare reset describes: {other:?}"),
    }

    ops::reset(
        &ctx,
        &["zq-adopted".to_owned()],
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(src.join("SKILL.md")).unwrap(),
        b"# adopted\n",
        "the SOURCE dir is what the reset restored"
    );
    assert!(
        !proj.0.join(".claude/skills/zq-adopted").exists(),
        "no second copy was planted in the checkout's harness dirs"
    );
}

// =================================================================================================
// Adopted-in-place custody is NEVER the sweep's to destroy: the user's source dir survives every
// clean path byte-identical, in both scopes — and a retained ghost's `remove` speaks honestly.
// =================================================================================================

/// Every file under `dir` with its exact bytes — the byte-identical assertion's witness.
fn dir_bytes(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((
                    p.strip_prefix(dir).unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}

/// The PROJECT-scope clean paths: a plain update leaves the adopted source alone; a hand-edited
/// manifest that orphans the record (the exact file state a `remove` row-drop leaves too) retires
/// NOTHING of the source dir — it survives byte-identical, record retained, idempotent across
/// sweeps. This was live data loss: the pre-fix cleaner marked every under-project placement of an
/// undemanded record stale, the adopted source included.
#[test]
fn an_adopted_in_place_source_dir_survives_the_project_retire() {
    let rig = Rig::new("zq-adoptkeep");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let proj = project("zq-adoptkeep-proj", "[bundles]\n");
    let src = proj.0.join("skills/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    std::fs::write(src.join("notes.txt"), b"the user's own extra file\n").unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    scoped_path_add(&ctx, &src, false).unwrap();
    let baseline = dir_bytes(&src);

    // A plain update (the row present) writes nothing into the source dir.
    sweep(&ctx, &plane, &dir);
    assert_eq!(
        dir_bytes(&src),
        baseline,
        "a plain update left the source alone"
    );

    // The row is orphaned by hand (the same file state a row-drop `remove` leaves): the sweep
    // retires nothing of the adopted source.
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    sweep(&ctx, &plane, &dir);
    assert!(src.is_dir(), "the adopted source dir survives the retire");
    assert_eq!(dir_bytes(&src), baseline, "…byte-identical");
    // Idempotent: the sweep after changes nothing either.
    sweep(&ctx, &plane, &dir);
    assert_eq!(dir_bytes(&src), baseline);
    // The record is retained (bytes-stay honesty; the ghost row explains itself elsewhere).
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(
        std::fs::read_dir(playout.skills_dir()).unwrap().count() > 0,
        "the record is retained"
    );
}

/// The same retire driven through the REAL row-drop (`remove` → the manifest arm), project scope:
/// the row leaves, the sweep retires nothing of the source dir.
#[test]
fn remove_then_update_never_touches_an_adopted_source_dir() {
    let rig = Rig::new("zq-adoptrm");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let proj = project("zq-adoptrm-proj", "[bundles]\n");
    let src = proj.0.join("skills/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    scoped_path_add(&ctx, &src, false).unwrap();
    let baseline = dir_bytes(&src);

    let session_connect = connect(&plane, &dir);
    let outcome = ops::remove_project(
        &ctx,
        &session_connect,
        &["quaggamap".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap()
    .expect("the row-drop arm claims the adopted name");
    match outcome {
        ops::RemoveOutcome::Applied(_) => {}
        other => panic!("a row drop applies immediately: {other:?}"),
    }
    assert_eq!(dir_bytes(&src), baseline, "the remove itself moved no byte");

    sweep(&ctx, &plane, &dir);
    assert!(src.is_dir(), "the retire sweep spared the adopted source");
    assert_eq!(dir_bytes(&src), baseline, "…byte-identical");
}

/// The MACHINE scope holds the same promise: a `-g` adopted source survives the row-drop sweep AND
/// the `--force` repair (which parks + re-projects every placement topos wrote — the user's own
/// dir is not one of them).
#[test]
fn an_adopted_source_dir_survives_the_machine_sweeps_and_rebuild() {
    let rig = Rig::new("zq-adoptg");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global("[bundles]\n");
    let src = rig.home.0.join("tools/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    scoped_path_add(&ctx, &src, true).unwrap();
    let baseline = dir_bytes(&src);

    // `update --force` with the row present: every topos-written placement re-projects; the
    // adopted source is left exactly as it stands.
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            rebuild: true,
            scope: ops::UpdateScope::Machine,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        dir_bytes(&src),
        baseline,
        "a rebuild never touches the source"
    );

    // The row dropped, then the machine sweep: the source survives byte-identical.
    rig.write_global("[bundles]\n");
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(src.is_dir(), "the machine retire spared the adopted source");
    assert_eq!(dir_bytes(&src), baseline, "…byte-identical");
}

/// Item: the demand-guard keys on a ROW claiming the name, not on store/cache provenance. With the
/// row present the refusal stands VERBATIM; with the row gone the same token is a GHOST and falls
/// through to the describe-first permanent delete, whose describe says the sweep would retire it
/// anyway — and whose receipt speaks in the applied tense.
#[test]
fn a_ghost_remove_falls_through_and_a_still_claimed_name_keeps_the_refusal() {
    let rig = Rig::new("zq-ghost");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "delivered + recorded"
    );
    // The app wires the CACHE-BACKED follow seam (the workspace provenance the guard reads);
    // the rig's default is inert, which would hide the ghost's provenance entirely.
    let cf = ops::CacheFollow::load(&rig.fs, &rig.layout());
    let mut ctx = rig.ctx_at(Some(&rig.work.0));
    ctx.follow = &cf;

    let named = NamedDirectory(dir.clone());
    let named_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(named.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let dir_connect = |_: &str| -> Box<dyn DirectorySource> { Box::new(dir.clone()) };
    let connectors = ops::RemoveConnectors {
        session: &named_connect,
        directory: &dir_connect,
    };

    // (a) STILL CLAIMED (the row is in the machine file): today's refusal, verbatim.
    let err = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, false)
        .expect_err("a claimed name refuses toward the demand");
    assert_eq!(
        crate::render::safe_message(&err),
        "'deploy' is delivered from a workspace — remove the DEMAND, not the copy: `topos \
         remove deploy` drops this folder's line for it; `topos remove -g deploy` edits your \
         machine-wide file (switching it off here). What the workspace assigns you is managed \
         on the web."
    );

    // (b) The row leaves (the ghost window: record + cache provenance remain, demand gone): the
    // false refusal is GONE — the bare run DESCRIBES the permanent delete, note honest.
    rig.write_global("[bundles]\n");
    let outcome = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, false)
        .expect("the ghost falls through to the classic ladder");
    let items = match outcome {
        ops::RemoveOutcome::Described { data, yes_argv } => {
            assert!(yes_argv.contains(&"--yes".to_owned()));
            assert!(!data.applied);
            data.items
        }
        other => panic!("a permanent delete describes first: {other:?}"),
    };
    assert_eq!(items.len(), 1);
    assert!(matches!(
        items[0].kind,
        topos_types::results::RemoveKind::TrackedLocalPermanent
    ));
    let note = items[0].note.as_deref().expect("the ghost explains itself");
    assert!(
        note.contains("`topos update` retires it anyway"),
        "the describe says doing nothing also resolves it: {note}"
    );

    // (c) `--yes` applies: dirs + record go, and the receipt's note speaks in the applied tense.
    let outcome = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, true)
        .expect("the consented apply");
    let data = match outcome {
        ops::RemoveOutcome::Applied(d) => d,
        other => panic!("--yes applies: {other:?}"),
    };
    assert!(data.applied);
    let note = data.items[0]
        .note
        .as_deref()
        .expect("the receipt discloses");
    assert!(
        !note.contains("doing nothing"),
        "an applied receipt never claims a choice that is already spent: {note}"
    );
    assert!(
        !rig.layout().skill_dir(&sid).exists(),
        "the record is gone with the copy"
    );
}

/// A gate-valid `server.json` for the MCP rows below (the converge re-runs the whole gate).
fn mcp_server_json(url: &str) -> String {
    format!(
        "{{\"name\":\"io.test/x\",\"description\":\"A test server.\",\"version\":\"1.0.0\",\
         \"remotes\":[{{\"type\":\"streamable-http\",\"url\":\"{url}\"}}]}}"
    )
}

/// TWO bundles of the SAME NAME in one scope — the workspace `linear` a feed delivers and a local
/// `linear` folder a row adopts — each keep their OWN per-agent config states on the receipt. The
/// converge's outcomes join to a row by the bundle's identity: matching on the display name handed
/// whichever row came first both bundles' outcomes and left the other row silent about entries
/// that really landed.
#[test]
fn two_same_named_mcp_bundles_in_one_scope_each_keep_their_own_states() {
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let rig = Rig::new("mcp-same-name");
    rig.seed_session();
    // cursor + openclaw both detect off the fake home and hang their surfaces there, so this stays
    // hermetic whatever the dev machine has installed.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".openclaw")).unwrap();

    let v = mk_version(&[(
        "server.json",
        FileMode::Regular,
        mcp_server_json("https://mcp.example/workspace").as_bytes(),
    )]);
    let plane = FakePlane::new(log).with_version("s_linear", &v);
    let mut ds = delivered("s_linear", "linear", &v);
    ds.kind = "mcp".into();
    plane.serves(vec![ds]);
    let mut entry = catalog_entry("s_linear", "linear", &v);
    entry.kind = "mcp".into();
    let dir = FakeDirectory::new(vec![entry], Vec::new());

    // The LOCAL bundle of the same name: its own folder, its own address.
    let local = rig.work.0.join("linear");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("server.json"),
        mcp_server_json("https://mcp.example/local"),
    )
    .unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"~/.cursor/mcp.json\", \"~/.openclaw/openclaw.json\"] }}\n\
         \"{}\" = {{ kind = \"mcp\", dest = [\"~/.cursor/mcp.json\", \"~/.openclaw/openclaw.json\"] }}\n",
        local.display()
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    let rows: Vec<&topos_types::results::PullSkill> = out
        .data
        .skills
        .iter()
        .filter(|s| s.skill == "linear")
        .collect();
    assert_eq!(rows.len(), 2, "one row each: {:?}", out.data.skills);
    for row in rows {
        let mut agents: Vec<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
        agents.sort_unstable();
        assert_eq!(agents, ["cursor", "openclaw"], "{row:?}");
    }
    // Both entries really landed, each under its own minted key.
    let text = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap();
    assert!(
        text.contains("topos-eng-linear")
            && text.contains("topos-local-linear")
            && text.contains("https://mcp.example/workspace")
            && text.contains("https://mcp.example/local"),
        "{text}"
    );
}

// =================================================================================================
// The `dest` field: a row's frozen destinations — placement, grow, shrink, both scopes.
// =================================================================================================

/// A PERSON-scope workspace row with `dest` places EXACTLY the named folders — the active
/// adapter's default dir gets nothing (detection is ignored for a dest row) — and a hand-edited
/// dest change converges on the next update: a NEW entry installs there (disclosed as the
/// install it is), a REMOVED entry's copy retires through the park-then-verify rail.
#[test]
fn a_dest_row_freezes_placement_and_grows_and_shrinks_on_the_next_update() {
    let rig = Rig::new("dest-freeze");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-a\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    let a = rig.home.0.join("dest-a/deploy");
    assert!(a.join("SKILL.md").exists(), "{:?}", out.data.skills);
    assert!(
        !rig.work.0.join("skills/deploy").exists(),
        "the default adapter dir gets nothing — the row froze its destinations"
    );
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::Installed, "{row:?}");
    assert!(
        row.destinations
            .iter()
            .any(|d| d.ends_with("dest-a/deploy")),
        "{row:?}"
    );

    // GROW: a hand-added entry installs there on the next update, said as an install.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-a\", \"~/dest-b\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    let b = rig.home.0.join("dest-b/deploy");
    assert!(b.join("SKILL.md").exists(), "{:?}", out.data.skills);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::Installed, "{row:?}");
    assert!(
        row.destinations
            .iter()
            .any(|d| d.ends_with("dest-b/deploy")),
        "the grown destination is the one named: {row:?}"
    );

    // SHRINK: the dropped entry's copy retires (park-then-verify); the kept entry stands.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-b\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    assert!(!a.exists(), "the un-named copy left: {:?}", out.data.skills);
    assert!(b.join("SKILL.md").exists(), "the named copy stands");
    let removed = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert!(
        removed
            .destinations
            .iter()
            .any(|d| d.ends_with("dest-a/deploy")),
        "{removed:?}"
    );
}

/// A dest SHRINK meets the keep-edited-in-place discipline: an EDITED copy at the dropped
/// destination is snapshotted and KEPT on disk (its record released), never swept up — exactly
/// like the feed-drop receipts.
#[test]
fn a_dest_shrink_keeps_an_edited_copy_in_place() {
    let rig = Rig::new("dest-keep");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-a\", \"~/dest-b\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let a = rig.home.0.join("dest-a/deploy");
    let b = rig.home.0.join("dest-b/deploy");
    assert!(a.join("SKILL.md").exists() && b.join("SKILL.md").exists());

    // The person edits the copy the shrink is about to drop.
    std::fs::write(a.join("SKILL.md"), b"# my edit\n").unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-b\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(a.join("SKILL.md")).unwrap(),
        b"# my edit\n",
        "the edited copy is the person's own — kept in place"
    );
    let removed = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert!(
        removed.kept.iter().any(|d| d.ends_with("dest-a/deploy")),
        "the kept copy is named: {removed:?}"
    );
}

/// A PROJECT-scope dest row places inside the checkout at the named relative folder — the same
/// dest mechanism that replaced the old path-override seam.
#[test]
fn a_project_dest_row_places_inside_the_checkout_at_the_named_folder() {
    let rig = Rig::new("dest-proj");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project(
        "dest-proj-co",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"tools/ai\"] }}\n"),
    );
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        proj.0.join("tools/ai/deploy/SKILL.md").exists(),
        "{:?} / {:?}",
        out.data.skills,
        out.warnings
    );
    assert!(
        !proj.0.join(".claude/skills/deploy").exists(),
        "the default project dirs get nothing — the row froze its destination"
    );
}

/// A REPO-SET row's dest aims every member's converge slot at the named folder — the dest-keyed
/// generalization of the old per-slug slots.
#[test]
fn a_repo_set_rows_dest_aims_its_members_at_the_named_folder() {
    let rig = Rig::new("dest-set");
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
    // The row is written by hand (demand is the row); the sweep converges it.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "o/r",
        true,
        true,
        &Default::default(),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    // Re-aim the whole set: the row's dest replaces the default dir for every member.
    rig.write_global("[bundles]\n\"github.com/o/r\" = { dest = [\"~/team-skills\"] }\n");
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v2\n"),
            ("skills/beta/SKILL.md", b"# beta v2\n"),
        ],
    ));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    for name in ["alpha", "beta"] {
        assert_eq!(
            std::fs::read(rig.home.0.join("team-skills").join(name).join("SKILL.md")).unwrap(),
            format!("# {name} v2\n").into_bytes(),
            "{name}: {:?} / {:?}",
            out.data.skills,
            out.warnings
        );
    }
}

// =================================================================================================
// `-a <agent>` / `--dest <folder>` — destinations on add and remove (the dest selectors).
// =================================================================================================

fn sel(agents: &[&str], dests: &[&str]) -> ops::Selection {
    ops::Selection {
        agents: agents.iter().map(|s| (*s).to_owned()).collect(),
        dests: dests.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// `topos add -g @ws/skill -a codex`: the row freezes to codex's folder, the copy lands there in
/// the same invocation, and the receipt is the FINAL destination shape, byte for byte.
#[test]
fn an_agent_selected_add_freezes_the_row_and_prints_the_destination_receipt() {
    let (rig, plane, dir, v) = add_rig("dest-add");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The row is frozen to exactly the selected destination.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#""acme.test/eng/deploy" = { dest = ["~/.codex/skills"] }"#),
        "{text}"
    );
    // The copy landed there in this invocation.
    assert!(
        rig.home.0.join(".codex/skills/deploy/SKILL.md").exists(),
        "the narrowed update lands the copy at the selected folder"
    );
    assert_eq!(data.dest, vec!["~/.codex/skills".to_owned()]);
    // The FINAL receipt copy, byte for byte.
    assert_eq!(
        crate::render::add_tty(&data),
        "+ @eng/deploy   installed (~/.codex/skills)\n(undo: topos remove -g deploy)"
    );
}

/// The same `-a` add when the plane cannot DELIVER (the delivery lane down, no bytes servable):
/// the row lands (the durable demand), but no copy does — so the receipt must NOT claim
/// `installed (…)`; the row-recorded receipt prints instead, with the next-sweep path disclosed.
#[test]
fn an_agent_selected_add_whose_delivery_fails_does_not_claim_installed() {
    let rig = Rig::new("dest-add-offline");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    // The catalog answers (the add resolves the name), but the plane serves NOTHING — no
    // delivery, no version bytes.
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    plane.serve_unreachable();
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The row froze to the destination regardless — the demand is durably recorded.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#""acme.test/eng/deploy" = { dest = ["~/.codex/skills"] }"#),
        "{text}"
    );
    // No copy landed — and the receipt does not claim one did.
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert!(data.dest.is_empty(), "{:?}", data.dest);
    assert_eq!(data.display, None);
    let tty = crate::render::add_tty(&data);
    assert!(!tty.contains("installed"), "{tty}");
    assert!(
        tty.contains("land on the next `topos update`"),
        "the next-sweep path is stated: {tty}"
    );
}

/// A same-named LOCAL row's clean report must not prove a workspace bundle's bytes: the adopted
/// `deploy` reconciles up-to-date in the same scope, but the workspace delivery failed — so the
/// receipt must NOT claim `installed (…)` on the strength of the other row.
#[test]
fn a_same_named_local_rows_report_does_not_prove_the_workspace_add() {
    let rig = Rig::new("dest-add-foreign-proof");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    // The catalog answers (the add resolves the name), but the plane serves NOTHING — no
    // delivery, no version bytes.
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    plane.serve_unreachable();
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    // A same-named ADOPTED folder already stands in the machine recipe.
    let local = rig.home.0.join("src/deploy");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("SKILL.md"), b"# mine\n").unwrap();
    rig.write_global(&format!("[bundles]\n\"{}\" = \"*\"\n", local.display()));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // No workspace copy landed — the local row's up-to-date report is not this bundle's proof.
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert!(data.dest.is_empty(), "{:?}", data.dest);
    assert_eq!(data.display, None);
    let tty = crate::render::add_tty(&data);
    assert!(!tty.contains("installed"), "{tty}");
    assert!(
        tty.contains("land on the next `topos update`"),
        "the next-sweep path is stated: {tty}"
    );
}

/// A directory whose channel index answers ONCE (the add's resolve) and then fails — the
/// transport dropping between the resolve and the SAME invocation's delivering reconcile.
#[derive(Clone)]
struct ChannelsDropAfterFirst {
    inner: FakeDirectory,
    answered: Arc<Mutex<u32>>,
}
impl DirectorySource for ChannelsDropAfterFirst {
    fn me(&self, ws: &str) -> Result<WireMe, ClientError> {
        self.inner.me(ws)
    }
    fn channels_index(&self, ws: &str) -> Result<WireChannelIndex, ClientError> {
        let mut n = self.answered.lock().unwrap();
        *n += 1;
        if *n > 1 {
            return Err(ClientError::Plane("directory unreachable".into()));
        }
        self.inner.channels_index(ws)
    }
    fn skills_index(&self, ws: &str) -> Result<WireSkillIndex, ClientError> {
        self.inner.skills_index(ws)
    }
    fn proposals_index(&self, ws: &str) -> Result<WireProposalIndex, ClientError> {
        self.inner.proposals_index(ws)
    }
    fn skill_log(&self, ws: &str, s: &str) -> Result<WireSkillLog, ClientError> {
        self.inner.skill_log(ws, s)
    }
    fn channel_place(&self, ws: &str, c: &str, s: &str) -> Result<(), ClientError> {
        self.inner.channel_place(ws, c, s)
    }
    fn channel_unplace(&self, ws: &str, c: &str, s: &str) -> Result<(), ClientError> {
        self.inner.channel_unplace(ws, c, s)
    }
    fn protect_skill(&self, ws: &str, s: &str, l: &str) -> Result<(), ClientError> {
        self.inner.protect_skill(ws, s, l)
    }
    fn protect_channel(&self, ws: &str, c: &str, l: &str) -> Result<(), ClientError> {
        self.inner.protect_channel(ws, c, l)
    }
    fn ack_notices(&self, ws: &str, ids: &[String]) -> Result<(), ClientError> {
        self.inner.ack_notices(ws, ids)
    }
}

/// A channel add with `-a` whose expansion FAILS in the delivering reconcile must not borrow an
/// unrelated feed bundle's clean row as its proof: the same untargeted sweep reconciles the
/// feed's `other` up-to-date, but THIS channel proved nothing — the row lands (the durable
/// demand), the receipt does not claim `installed (…)`, and the next-sweep path prints.
#[test]
fn a_channel_add_whose_expansion_fails_does_not_borrow_the_feeds_proof() {
    let rig = Rig::new("dest-add-channel-fail");
    rig.seed_session();
    let v = one_file(b"# other\n");
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new()))).with_version("s_other", &v);
    plane.serves(vec![delivered("s_other", "other", &v)]);
    let inner = FakeDirectory::new(
        vec![catalog_entry("s_other", "other", &v)],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_other".into(),
                name: "other".into(),
            }],
        }],
    );
    rig.seed_feed();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // The feed bundle stands CURRENT before the add — the unrelated up-to-date row the gate
    // must not borrow.
    let primed = sweep_scoped(&ctx, &plane, &inner, ops::UpdateScope::Machine);
    assert!(
        primed
            .data
            .skills
            .iter()
            .any(|s| s.skill == "other" && s.action == PullAction::Installed),
        "the fixture needs the feed delivering: {:?}",
        primed.data.skills
    );
    let wrapper = ChannelsDropAfterFirst {
        inner: inner.clone(),
        answered: Arc::new(Mutex::new(0)),
    };
    let session_connect = move |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(wrapper.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let data = match ops::add_reference(
        &ctx,
        &session_connect,
        None,
        &format!("@{WS_NAME}/channels/backend"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The row froze to the destination regardless — the demand is durably recorded.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#""acme.test/eng/channels/backend" = { dest = ["~/.codex/skills"] }"#),
        "{text}"
    );
    // The channel expansion failed — nothing of ITS proved, whatever the feed reconciled.
    assert!(data.dest.is_empty(), "{:?}", data.dest);
    assert_eq!(data.display, None);
    let tty = crate::render::add_tty(&data);
    assert!(!tty.contains("installed"), "{tty}");
    assert!(
        tty.contains("land on the next `topos update`"),
        "the next-sweep path is stated: {tty}"
    );
}

/// A re-add whose own reconcile MERGES a local draft onto the new current landed bytes — the
/// merged draft stands placed at the destination, so the receipt's `installed (…)` claim stands
/// (a merged row is not a failed placement).
#[test]
fn an_agent_selected_add_whose_reconcile_merges_keeps_the_destination_receipt() {
    let rig = Rig::new("dest-add-merged");
    rig.seed_session();
    let v1 = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n\nsteps\n")]);
    let v2 = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy v2\n\nsteps\n")]);
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())))
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir1 = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir1),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    assert_eq!(data.dest, vec!["~/.codex/skills".to_owned()]);
    let placed = rig.home.0.join(".codex/skills/deploy/SKILL.md");
    // The person edits their copy (a draft), and the team moves current — a real pointer move.
    std::fs::write(&placed, b"# deploy\n\nsteps\nmine\n").unwrap();
    let mut moved = delivered("s_deploy", "deploy", &v2);
    moved.generation = 2;
    plane.serves(vec![moved]);
    let mut moved_cat = catalog_entry("s_deploy", "deploy", &v2);
    moved_cat.generation = 2;
    let dir2 = FakeDirectory::new(vec![moved_cat], Vec::new());
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir2),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The merge landed BOTH edits at the destination — the receipt may say so.
    let text = std::fs::read_to_string(&placed).unwrap();
    assert!(
        text.contains("# deploy v2") && text.contains("mine"),
        "the merged draft stands placed: {text}"
    );
    assert_eq!(data.dest, vec!["~/.codex/skills".to_owned()]);
    let tty = crate::render::add_tty(&data);
    assert!(tty.contains("installed (~/.codex/skills)"), "{tty}");
    assert!(!tty.contains("could not be placed"), "{tty}");
}

/// A re-add whose own reconcile ends in a merge CONFLICT clears the destination claim — but the
/// note must not promise the next update (a standing conflict is not a transient miss): it says
/// what the row reports and where the detail lives.
#[test]
fn an_agent_selected_add_whose_reconcile_conflicts_names_the_standing_state() {
    let rig = Rig::new("dest-add-conflicted");
    rig.seed_session();
    let v1 = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n\nsteps\n")]);
    let v2 = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy THEIRS\n\nsteps\n")]);
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())))
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir1 = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir1),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    }
    let placed = rig.home.0.join(".codex/skills/deploy/SKILL.md");
    // BOTH sides edit the same line — the three-way merge cannot resolve it.
    std::fs::write(&placed, b"# deploy MINE\n\nsteps\n").unwrap();
    let mut moved = delivered("s_deploy", "deploy", &v2);
    moved.generation = 2;
    plane.serves(vec![moved]);
    let mut moved_cat = catalog_entry("s_deploy", "deploy", &v2);
    moved_cat.generation = 2;
    let dir2 = FakeDirectory::new(vec![moved_cat], Vec::new());
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir2),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The conflicted state is not a placed install — the destination claim clears …
    assert!(data.dest.is_empty(), "{:?}", data.dest);
    assert_eq!(data.display, None);
    // … and the note names the STANDING state instead of promising the next update.
    let note = data.note.clone().unwrap_or_default();
    assert!(note.contains("'deploy' reports conflicted here"), "{note}");
    assert!(
        note.contains("`topos list deploy` has the detail"),
        "{note}"
    );
    assert!(!note.contains("land on the next"), "{note}");
}

/// `topos remove -g <skill> -a codex` over a three-destination row: the codex folder is
/// subtracted and its copy leaves; the row keeps the rest; the receipt is the FINAL narrow
/// shape, byte for byte, and its undo re-adds exactly what left.
#[test]
fn a_narrowed_remove_subtracts_one_destination_and_prints_the_final_receipt() {
    let (rig, plane, dir, v) = add_rig("dest-narrow");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\", \"~/.cursor/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    for agent in [".claude", ".codex", ".cursor"] {
        assert!(
            rig.home
                .0
                .join(agent)
                .join("skills/deploy/SKILL.md")
                .exists(),
            "{agent}: the dest row installed everywhere it names"
        );
    }
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a narrow applies immediately: {other:?}"),
    };
    // The row keeps the remaining two destinations; the codex copy is gone, the others stay.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#"dest = ["~/.claude/skills", "~/.cursor/skills"]"#),
        "{text}"
    );
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert!(rig.home.0.join(".claude/skills/deploy/SKILL.md").exists());
    // The receipt facts, typed.
    let u = &data.uninstalled[0];
    assert_eq!(u.name, format!("@{WS_NAME}/deploy"));
    assert_eq!(u.destinations, vec!["~/.codex/skills".to_owned()]);
    assert_eq!(u.remaining, Some(2));
    // The FINAL receipt copy, byte for byte.
    assert_eq!(
        crate::render::remove_applied_tty(&data),
        "- @eng/deploy   removed (~/.codex/skills) — 2 folders remain\n(undo: topos add -g \
         @eng/deploy -a codex)"
    );
}

/// The narrow when the reconcile cannot move the copy (a hand-written dest row on a machine the
/// plane never delivered to, the plane unreachable): the row's subtraction lands (the durable
/// edit), but NOTHING was uninstalled — so the receipt must NOT claim `removed (…)`; the row-edit
/// line prints with the next-sweep path instead.
#[test]
fn an_offline_narrow_does_not_claim_the_copy_left() {
    let rig = Rig::new("dest-narrow-offline");
    rig.seed_session();
    // The plane serves nothing at all — no delivery snapshot, no version bytes, empty catalog.
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    plane.serve_unreachable();
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\", \"~/.cursor/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a narrow applies immediately: {other:?}"),
    };
    // The row edit landed: the manifest keeps the remaining two destinations.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#"dest = ["~/.claude/skills", "~/.cursor/skills"]"#),
        "{text}"
    );
    // Nothing was uninstalled — and the receipt does not claim anything was.
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
    let tty = crate::render::remove_applied_tty(&data);
    assert!(!tty.contains("removed ("), "{tty}");
    assert!(
        tty.contains("leaves on the next `topos update`"),
        "the next-sweep path is stated: {tty}"
    );
    assert!(
        tty.contains("the row keeps 2 folders"),
        "the remaining count survives the fallback: {tty}"
    );
}

/// Narrowing a dest-less row with NO recorded copies refuses with the honest zero-state — never
/// the old trailing-off "its destinations here are " with nothing after it.
#[test]
fn narrowing_a_row_with_no_recorded_copies_refuses_with_the_zero_state() {
    let (rig, plane, dir, _v) = add_rig("dest-narrow-zero");
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("'deploy' has no recorded copies in this scope yet"),
        "{msg}"
    );
    assert!(msg.contains("run `topos update` first"), "{msg}");
    assert!(msg.contains("topos remove -g deploy"), "{msg}");
    // The manifest row is untouched — the refusal changed nothing.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("deploy"), "{text}");
}

/// The same zero-state over a FEED-delivered skill (no row anywhere): "its row was added" would
/// be a fabrication — there is no row to remove. The honest variant names what stands (the feed
/// delivers it) and the whole-machine end that actually exists (the `"off"` switch).
#[test]
fn narrowing_a_feed_delivered_skill_with_no_copies_names_the_off_switch() {
    let (rig, plane, dir, v) = add_rig("dest-narrow-feed-zero");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.seed_feed();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("'deploy' is feed-delivered and has no recorded copies in this scope yet"),
        "{msg}"
    );
    assert!(msg.contains("run `topos update` first"), "{msg}");
    assert!(
        msg.contains("`topos remove -g deploy` to switch it off entirely"),
        "{msg}"
    );
    assert!(!msg.contains("its row was added"), "{msg}");
    // The manifest is untouched — no `"off"` switch was written by the refusal.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("off"), "{text}");
}

/// Removing the LAST destination removes the whole row — a row is never left bare by
/// subtraction (bare means default reach and would resurrect copies).
#[test]
fn removing_the_last_destination_removes_the_row_entirely() {
    let (rig, plane, dir, v) = add_rig("dest-last");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join(".codex/skills/deploy/SKILL.md").exists());
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("deploy"), "the whole row is gone: {text}");
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    // The whole-row undo reconstructs the destination that left.
    assert_eq!(
        data.undo,
        vec!["topos", "add", "-g", "@eng/deploy", "-a", "codex"]
    );
}

/// A bare `remove -g <skill>` of a dest row removes the row AND every copy it placed, in the
/// same invocation — the FINAL whole-row receipt, byte for byte, with the `-a` reconstruction.
#[test]
fn a_whole_row_remove_uninstalls_eagerly_and_reconstructs_the_undo() {
    let (rig, plane, dir, v) = add_rig("dest-whole");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(!rig.home.0.join(".claude/skills/deploy").exists());
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert_eq!(
        data.undo,
        vec![
            "topos",
            "add",
            "-g",
            "@eng/deploy",
            "-a",
            "claude-code",
            "-a",
            "codex"
        ]
    );
    // The FINAL receipt copy, byte for byte.
    assert_eq!(
        crate::render::remove_applied_tty(&data),
        "- @eng/deploy   removed (2 folders)\n(undo: topos add -g @eng/deploy -a claude-code -a \
         codex)"
    );
}

/// Narrowing a row that names NO destinations first materializes the CURRENT resolved set —
/// the result is a frozen row of the remaining destinations.
#[test]
fn narrowing_a_no_dest_row_freezes_the_remainder() {
    let (rig, plane, dir, v) = add_rig("dest-materialize");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    // codex DETECTED (its home dir exists) so the default plan includes its native folder.
    std::fs::create_dir_all(rig.home.0.join(".codex")).unwrap();
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join(".codex/skills/deploy/SKILL.md").exists());
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    // The codex copy left; the row is now FROZEN to the remaining destination(s).
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("dest = ["), "the remainder is frozen: {text}");
    assert!(!text.contains("~/.codex/skills"), "{text}");
    // The materialize disclosure rides the receipt.
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(note.contains("named no destinations"), "{note}");
    let u = &data.uninstalled[0];
    assert_eq!(u.destinations, vec!["~/.codex/skills".to_owned()]);
}

/// Dropping an explicit row whose bundle the FEED still delivers: the row edit lands, the copies
/// CORRECTLY stay (the feed still demands them) — and the receipt says exactly that, with the off
/// switch, instead of the stock "the copies it placed leave this machine now" lie.
#[test]
fn removing_a_row_the_feed_still_delivers_says_the_copies_stay() {
    let rig = Rig::new("row-feed-stays");
    rig.seed_session();
    // BOTH the feed line and an explicit row for the same bundle.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"
    ));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let placed = rig.work.0.join("skills/deploy/SKILL.md");
    assert!(placed.exists());

    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean row drop applies immediately: {other:?}"),
    };
    // The row is gone; the feed line survives; the copy stays because the feed still demands it.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("/deploy\""), "the row left: {text}");
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}\"")),
        "the feed line stands: {text}"
    );
    assert!(placed.exists(), "the feed-demanded copy stays in place");
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
    // The receipt: the honest copies-stay line with the off switch — never the copies-leave claim.
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains(&format!("{WS_NAME}'s feed still delivers it here")),
        "{note}"
    );
    assert!(
        note.contains("`topos remove -g deploy` switches it off"),
        "{note}"
    );
    let tty = crate::render::remove_applied_tty(&data);
    assert!(tty.contains("the copies stay in place"), "{tty}");
    assert!(!tty.contains("leave this machine now"), "{tty}");
}

/// The unknown-agent refusal: the FINAL copy shape — real registry slugs, alphabetical, ellipsis
/// past the handful — and the TTY closes with `nothing changed`.
#[test]
fn an_unknown_agent_refuses_with_the_registry_list_and_nothing_changed() {
    let (rig, plane, dir, _v) = add_rig("dest-unknown");
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codx"], &[]),
    )
    .unwrap_err();
    assert_eq!(err.code(), "UNKNOWN_AGENT");
    let mut slugs: Vec<&str> = topos_harness::registry::known_harnesses()
        .iter()
        .map(|h| h.slug)
        .collect();
    slugs.sort_unstable();
    assert_eq!(
        crate::render::err_tty(&err),
        format!(
            "error: unknown agent: codx — known: {}, …\nnothing changed",
            slugs[..4].join(", ")
        )
    );
    // Nothing was written.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert_eq!(text, "[bundles]\n");
}

/// A selection over a whole FEED refuses (a feed reaches every agent by nature), teaching the
/// narrower row — and the TTY closes with `nothing changed`.
#[test]
fn a_selection_over_a_feed_refuses_whole() {
    let (rig, plane, dir, _v) = add_rig("dest-feed");
    rig.seed_feed();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}"),
        true,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    let tty = crate::render::err_tty(&err);
    assert!(tty.contains("whole feed"), "{tty}");
    assert!(tty.ends_with("nothing changed"), "{tty}");
}

/// A feed add states its one fact ONCE. The receipt's closing sentence is where "this machine now
/// takes whatever <ws> gives you" belongs, so the row write adds no `note:` line saying the same
/// thing one line above it. The IDEMPOTENT re-add keeps its own note — "already adopting …" is a
/// fact that sentence does not carry.
#[test]
fn a_feed_add_states_what_this_machine_now_takes_exactly_once() {
    let (rig, plane, dir, _v) = add_rig("feed-once");
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let add = || match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}"),
        true,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        other => panic!("a feed row applies immediately: {other:?}"),
    };

    let data = add();
    assert!(
        data.note.is_none(),
        "the closing sentence is the ONE place this fact is stated: {:?}",
        data.note
    );
    let tty = crate::render::add_tty(&data);
    assert_eq!(
        tty.matches("takes whatever").count(),
        1,
        "the same fact, twice: {tty}"
    );
    assert!(
        tty.contains(&format!(
            "This machine now takes whatever {WS_NAME} gives you"
        )),
        "{tty}"
    );

    // The re-add rewrites nothing — and says so; the closing sentence still prints once.
    let data = add();
    let note = data.note.as_deref().expect("the no-op discloses itself");
    assert!(note.contains("already adopting"), "{note}");
    assert!(note.contains("nothing changed"), "{note}");
    let tty = crate::render::add_tty(&data);
    assert_eq!(tty.matches("takes whatever").count(), 1, "{tty}");
}

/// The shared-copy refusal, byte for byte: subtraction cannot narrow one shared folder, and the
/// two ways out print as aligned command lines (the `-a` list = the covered agents that read the
/// shared copy, minus the one being removed).
#[test]
fn a_shared_only_copy_refuses_per_agent_removal_with_both_ways_out() {
    let (rig, plane, _dir, v) = add_rig("dest-shared");
    let v2 = one_file(b"# coolify\n");
    let plane = plane.with_version("s_cool", &v2);
    plane.serves(vec![
        delivered("s_deploy", "deploy", &v),
        delivered("s_cool", "coolify-deploy", &v2),
    ]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &v),
            catalog_entry("s_cool", "coolify-deploy", &v2),
        ],
        Vec::new(),
    );
    // amp + cline DETECTED — both read the shared `~/.agents/skills` dir per the coverage table,
    // so the machine plan places ONE shared copy.
    std::fs::create_dir_all(rig.home.0.join(".config/amp")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".cline")).unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/coolify-deploy\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        rig.home
            .0
            .join(".agents/skills/coolify-deploy/SKILL.md")
            .exists(),
        "the covered agents share one copy"
    );
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["coolify-deploy".into()],
        None,
        false,
        &sel(&["amp"], &[]),
    )
    .unwrap_err();
    assert_eq!(err.code(), "SHARED_COPY_ONLY");
    // The FINAL copy shape, byte for byte: the statement, then the two aligned ways out.
    assert_eq!(
        crate::render::err_tty(&err),
        "coolify-deploy has no amp-only copy — its one copy is \
         ~/.agents/skills/coolify-deploy, which several agents read\n  topos remove -g \
         coolify-deploy              remove it for every agent\n  topos add -g \
         @eng/coolify-deploy -a cline   keep it per-agent instead, then re-run"
    );
    // No hint block repeats the two commands (they are the copy) — but the agent surface gets
    // BOTH ways out as structured next actions, byte-identical argvs.
    assert!(crate::render::err_hint_tty("remove", &[], &err).is_none());
    let actions = crate::render::next_actions("remove", &[], &err);
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert_eq!(
        actions[0].argv,
        vec!["topos", "remove", "-g", "coolify-deploy"]
    );
    assert_eq!(
        actions[1].argv,
        vec!["topos", "add", "-g", "@eng/coolify-deploy", "-a", "cline"]
    );
}

/// A forge import unions `-a` and `--dest`: the row records BOTH destinations, and the member
/// lands at each in the same apply.
#[test]
fn a_forge_import_unions_agent_and_dest_selectors() {
    let (rig, plane, dir, _v) = add_rig("dest-forge-union");
    rig.write_global("[bundles]\n");
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
        &["codex".to_owned()],
        &["~/team-skills".to_owned()],
        true,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(items) => {
            assert_eq!(items.len(), 2, "one landing per destination slot");
            assert_eq!(
                items[0].dest,
                vec!["~/.codex/skills".to_owned(), "~/team-skills".to_owned()]
            );
        }
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(
        rig.home.0.join(".codex/skills/deploy/SKILL.md").exists(),
        "the agent slot landed"
    );
    assert!(
        rig.home.0.join("team-skills/deploy/SKILL.md").exists(),
        "the literal folder landed"
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#"dest = ["~/.codex/skills", "~/team-skills"]"#),
        "the union rides the row: {text}"
    );
}

/// A LOCAL adopt with a selection: the adopted folder stays the working copy, and the row's
/// `dest` places a managed COPY at the selected folder through the ordinary sweep.
#[test]
fn a_local_adopt_with_dest_places_a_copy_at_the_selected_folder() {
    let (rig, plane, dir, _v) = add_rig("dest-local-adopt");
    rig.write_global("[bundles]\n");
    let src = rig.work.0.join("my-skill");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# mine\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();
    let sctx = ops::ctx_with_layout(&ctx, &scope.layout);
    let mut d = ops::adopt_path(&sctx, &scope.target, &src).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut d,
        &scope.target,
        &src,
        &["~/.codex/skills".to_owned()],
    )
    .unwrap();
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    // The managed copy landed at the selected folder; the source is untouched.
    assert_eq!(
        std::fs::read(rig.home.0.join(".codex/skills/my-skill/SKILL.md")).unwrap(),
        b"# mine\n"
    );
    assert_eq!(std::fs::read(src.join("SKILL.md")).unwrap(), b"# mine\n");
    // The sweep is idempotent — a second run moves nothing new.
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        out.data
            .skills
            .iter()
            .all(|r| r.action != PullAction::Installed),
        "{:?}",
        out.data.skills
    );
}

// =================================================================================================
// Whole-row `remove -g`: the row drop takes every copy the row placed out in the SAME invocation,
// whatever enumerated the record — the delivery cache, a live session, or nothing at all. The
// machine-scope cleaner walks the delivery cache and the forge imports; a record neither names (a
// local-path row, a frozen sweep while the plane is dark) must still leave through the verb's own
// retire rail, or the receipt's "the copies it placed leave this machine now" is a lie.
// =================================================================================================

/// The bare whole-row remove with the plane UNREACHABLE at remove time: the eager reconcile's
/// cache walk freezes (no fresh snapshot), so the verb's own rail must retire the copies — both
/// folders leave NOW, the receipt lists them, and the item's `agent_dirs` says what moved.
#[test]
fn an_offline_whole_row_remove_still_deletes_every_copy() {
    let (rig, plane, dir, _v) = add_rig("whole-offline");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join(".claude/skills/deploy/SKILL.md").exists());
    assert!(rig.home.0.join(".codex/skills/deploy/SKILL.md").exists());
    // The plane goes dark BEFORE the remove: every cache-walked clean is frozen this run.
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !rig.home.0.join(".claude/skills/deploy").exists(),
        "the claude copy leaves in-invocation; uninstalled={:?}",
        data.uninstalled
    );
    assert!(
        !rig.home.0.join(".codex/skills/deploy").exists(),
        "the codex copy leaves in-invocation; uninstalled={:?}",
        data.uninstalled
    );
    // The receipt speaks in what actually moved, and the item's agent_dirs carries it.
    let u = &data.uninstalled[0];
    assert_eq!(u.name, format!("@{WS_NAME}/deploy"));
    let mut dests = u.destinations.clone();
    dests.sort();
    assert_eq!(
        dests,
        vec![
            "~/.claude/skills/deploy".to_owned(),
            "~/.codex/skills/deploy".to_owned()
        ]
    );
    assert!(u.kept.is_empty(), "{:?}", u.kept);
    let mut dirs = data.items[0].agent_dirs.clone();
    dirs.sort();
    assert_eq!(dirs, dests);
}

/// The same offline whole-row remove with ONE EDITED copy: the clean copy leaves, the edited one
/// is KEPT IN PLACE (snapshotted into the store first) and the receipt's kept facts say so.
#[test]
fn an_offline_whole_row_remove_keeps_the_edited_copy_in_place() {
    let (rig, plane, dir, _v) = add_rig("whole-offline-edit");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let edited = rig.home.0.join(".codex/skills/deploy/SKILL.md");
    std::fs::write(&edited, b"# deploy\nlocal notes\n").unwrap();
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !rig.home.0.join(".claude/skills/deploy").exists(),
        "the clean copy leaves; uninstalled={:?}",
        data.uninstalled
    );
    assert_eq!(
        std::fs::read(&edited).unwrap(),
        b"# deploy\nlocal notes\n",
        "the edited copy stays in place, byte-identical"
    );
    let u = &data.uninstalled[0];
    assert_eq!(u.destinations, vec!["~/.claude/skills/deploy".to_owned()]);
    assert_eq!(u.kept, vec!["~/.codex/skills/deploy".to_owned()]);
    // Snapshot-first: the store holds the base version AND the kept edit.
    assert_eq!(store_versions(&rig.layout(), "s_deploy"), 2);
}

/// A `--dest` FREE-FORM folder row rides the same rail: the folder's copy leaves with the row.
#[test]
fn an_offline_whole_row_remove_cleans_a_free_form_dest_folder() {
    let (rig, plane, dir, _v) = add_rig("whole-offline-folder");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/team-skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join("team-skills/deploy/SKILL.md").exists());
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !rig.home.0.join("team-skills/deploy").exists(),
        "the free-form folder's copy leaves; uninstalled={:?}",
        data.uninstalled
    );
    let u = &data.uninstalled[0];
    assert_eq!(u.destinations, vec!["~/team-skills/deploy".to_owned()]);
}

/// A MACHINE-scope LOCAL-PATH row: no delivery cache entry, no forge origin, no workspace names
/// this record — only the verb's own rail can take its managed copies out. The adopted-in-place
/// source dir is the person's own and is NEVER deleted.
#[test]
fn a_local_path_whole_row_remove_deletes_managed_copies_and_spares_the_source() {
    let (rig, plane, dir, _v) = add_rig("whole-local");
    rig.write_global("[bundles]\n");
    let src = rig.work.0.join("my-skill");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# mine\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();
    let sctx = ops::ctx_with_layout(&ctx, &scope.layout);
    let mut d = ops::adopt_path(&sctx, &scope.target, &src).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut d,
        &scope.target,
        &src,
        &["~/.codex/skills".to_owned()],
    )
    .unwrap();
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join(".codex/skills/my-skill/SKILL.md").exists());
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["my-skill".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !rig.home.0.join(".codex/skills/my-skill").exists(),
        "the managed copy leaves in-invocation; uninstalled={:?}",
        data.uninstalled
    );
    // The adopted-in-place source is the person's own — never deleted.
    assert_eq!(std::fs::read(src.join("SKILL.md")).unwrap(), b"# mine\n");
    let u = &data.uninstalled[0];
    assert_eq!(u.name, "my-skill");
    assert_eq!(u.destinations, vec!["~/.codex/skills/my-skill".to_owned()]);
    assert_eq!(data.items[0].agent_dirs, u.destinations);
}

/// A whole-row `remove -g` of an MCP row takes its server entries OUT of the config files in the
/// same invocation (the inline converge) — no placement dirs are involved, and the item's note
/// names the removal per config file.
#[test]
fn a_whole_row_remove_of_an_mcp_row_takes_its_config_entries_out() {
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let rig = Rig::new("whole-mcp");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let v = mk_version(&[(
        "server.json",
        FileMode::Regular,
        mcp_server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new(log).with_version("s_linear", &v);
    plane.serves(Vec::new());
    let mut entry = catalog_entry("s_linear", "linear", &v);
    entry.kind = "mcp".into();
    let dir = FakeDirectory::new(vec![entry], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("topos-eng-linear"),
        "the sweep placed the server entry"
    );
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["linear".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !cursor.exists()
            || !std::fs::read_to_string(&cursor)
                .unwrap()
                .contains("topos-eng-linear"),
        "the config entry leaves in-invocation"
    );
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(note.contains("server entry removed"), "{note}");
}

/// Two teams publish a `deploy` and only the OTHER team's copy is on this machine. Dropping the
/// row of the workspace that has NO record here must move nothing: the scope store answers BARE
/// names, so the fallback's clean would otherwise retire a stranger's record — deleting its files
/// and putting them on this row's receipt.
#[test]
fn a_whole_row_remove_spares_another_workspaces_same_named_record() {
    let rig = Rig::new("whole-cross-ws");
    // The OTHER workspace's copy lands first — it is the only connection while it does.
    seed_second_session(&rig);
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_ops_deploy", &v);
    plane.serves(vec![delivered("s_ops_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_ops_deploy", "deploy", &v)],
        Vec::new(),
    );
    rig.write_global("[bundles]\n\"beta.test/ops/deploy\" = { dest = [\"~/.codex/skills\"] }\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let placed = rig.home.0.join(".codex/skills/deploy/SKILL.md");
    assert!(placed.exists());

    // This machine's recipe now carries the FIRST team's same-named bundle instead — a row with
    // no record of its own here. The plane is dark, so only the verb's own rail could move
    // anything at all.
    rig.seed_session();
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/deploy")],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean row drop applies immediately: {other:?}"),
    };
    // The other workspace's bytes AND its record are untouched.
    assert_eq!(
        std::fs::read(&placed).unwrap(),
        b"# deploy\n",
        "another workspace's copy is not this row's to delete"
    );
    let sid = crate::id::SkillId::parse("s_ops_deploy").unwrap();
    assert!(
        !rig.layout().published(&sid).retired.exists(),
        "the other workspace's record is not retired"
    );
    // And the receipt claims nothing: no uninstall block, no cleaned dirs, no copies-leave line.
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
    assert!(
        data.items[0].agent_dirs.is_empty(),
        "{:?}",
        data.items[0].agent_dirs
    );
    let tty = crate::render::remove_applied_tty(&data);
    assert!(!tty.contains("leave this machine now"), "{tty}");
}

/// The copies-stay disclosure when a CHANNEL line is what still delivers the bundle: there is no
/// feed carrying it, so the note may not name one — and `topos remove -g <name>`, which is the off
/// write for a FEED-delivered bundle, gives way to the deep read that names the real line.
#[test]
fn removing_a_row_a_channel_line_still_delivers_names_no_feed() {
    let rig = Rig::new("row-channel-stays");
    rig.seed_session();
    // The login's feed line (which assigns this person nothing), a channel line, and an explicit
    // row for the same bundle — explicit beats set while the row stands.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n\
         \"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"
    ));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    // The workspace's own feed assigns nothing — the channel line is the only other demand.
    plane.serves(Vec::new());
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
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let placed = rig.work.0.join("skills/deploy/SKILL.md");
    assert!(placed.exists());

    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/deploy")],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean row drop applies immediately: {other:?}"),
    };
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("/deploy\""), "the row left: {text}");
    assert!(
        text.contains("channels/backend"),
        "the set line stands: {text}"
    );
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}\"")),
        "the feed line stands too — and carries nothing of this name: {text}"
    );
    assert!(placed.exists(), "the channel-demanded copy stays in place");
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("another line here still delivers it"),
        "{note}"
    );
    assert!(note.contains("`topos list deploy` shows which"), "{note}");
    assert!(
        !note.contains("feed still delivers"),
        "no feed carries this bundle: {note}"
    );
    let tty = crate::render::remove_applied_tty(&data);
    assert!(tty.contains("the copies stay in place"), "{tty}");
    assert!(!tty.contains("leave this machine now"), "{tty}");
}

/// The same disclosure over a CROSS-WORKSPACE same-name collision: this workspace's feed line
/// stands but does not carry the bundle (its row was the only demand) — ANOTHER workspace's feed
/// delivers the name. The note names the workspace whose feed actually delivers it, so the off
/// switch it offers is the one that reaches the copies.
#[test]
fn removing_a_row_names_the_feed_that_actually_delivers_it() {
    let rig = Rig::new("row-cross-feed");
    rig.seed_session();
    seed_second_session(&rig);
    // This workspace's feed line + its explicit row; the feed itself assigns nothing.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"
    ));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let placed = rig.work.0.join("skills/deploy/SKILL.md");
    assert!(placed.exists());

    // The OTHER workspace's feed carries a bundle of the same name, and this machine adopts it.
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            "w_ops".to_owned(),
            sync_status::WorkspaceSync {
                host: Some("beta.test".to_owned()),
                workspace_name: Some("ops".to_owned()),
                last_delivery_at: Some(1),
                delivered: [(
                    "s_ops_deploy".to_owned(),
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
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n\
         \"beta.test/ops\" = \"*\"\n"
    ));
    // Dark, so the frozen sweep moves nothing and the cached provenance stands.
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/deploy")],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean row drop applies immediately: {other:?}"),
    };
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("ops's feed still delivers it here"),
        "the feed that DELIVERS it is named, not the row's own workspace: {note}"
    );
    assert!(
        !note.contains(&format!("{WS_NAME}'s feed still delivers it here")),
        "this workspace's feed line carries nothing of the kind: {note}"
    );
    assert!(placed.exists(), "the copies stay in place");
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
}

// =================================================================================================
// Placement-heal honesty: an update that CREATES a placement (a folder materialized where none
// was) says `installed` — and "all up to date" may only claim itself when nothing changed on disk.
// =================================================================================================

/// Delete a delivered skill's placement folder and run `update`: the files come back — and the
/// receipt MUST say so (`+ … installed (…)`), never `all up to date`. Around the heal, a genuinely
/// untouched update keeps the exact all-up-to-date summary, byte for byte.
#[test]
fn healing_a_deleted_placement_reads_installed_never_all_up_to_date() {
    let rig = Rig::new("healrcpt");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // First sweep installs; the second is genuinely untouched and pins the exact summary — down to
    // the header's verb, which says what the run DID: it checked, it did not update.
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.join("SKILL.md").exists());
    let clean = sweep(&ctx, &plane, &dir);
    assert_eq!(
        crate::render::pull_tty(
            &clean.data,
            &clean.decisions,
            &clean.warnings,
            &clean.advisories,
            &clean.disclosures
        ),
        "checked machine-wide\nChecked 1 skill: all up to date."
    );

    // The placement folder vanishes (a hand-delete, an agent cleanup). The next update re-creates
    // it — files materialized where none were is an INSTALL, not "up to date".
    std::fs::remove_dir_all(&placed).unwrap();
    let healed = sweep(&ctx, &plane, &dir);
    assert!(
        placed.join("SKILL.md").exists(),
        "the heal restores the files"
    );
    let row = healed
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(
        row.action,
        PullAction::Installed,
        "a created placement is an install: {row:?}"
    );
    assert_eq!(row.destinations.len(), 1, "{:?}", row.destinations);
    let out = crate::render::pull_tty(
        &healed.data,
        &healed.decisions,
        &healed.warnings,
        &healed.advisories,
        &healed.disclosures,
    );
    assert!(out.contains(&format!("+ @{WS_NAME}/deploy")), "{out}");
    assert!(out.contains("installed ("), "{out}");
    assert!(
        !out.contains("all up to date"),
        "a run that created a placement may not claim it: {out}"
    );

    // Healed and untouched again → exactly the all-up-to-date summary again.
    let again = sweep(&ctx, &plane, &dir);
    assert_eq!(
        crate::render::pull_tty(
            &again.data,
            &again.decisions,
            &again.warnings,
            &again.advisories,
            &again.disclosures
        ),
        "checked machine-wide\nChecked 1 skill: all up to date."
    );
}

/// Drop the feed line (copies retire), re-add it, update: the bytes come back — the receipt says
/// `installed`, with the destination, even though the served version never moved.
#[test]
fn re_adding_the_feed_line_reinstalls_with_a_receipt_line() {
    let rig = Rig::new("readdline");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.work.0.join("skills/deploy");
    assert!(placed.join("SKILL.md").exists());

    // The feed line is removed: the next update retires the copies.
    rig.write_global("[bundles]\n");
    let retired = sweep(&ctx, &plane, &dir);
    assert!(
        !placed.exists(),
        "the dropped line retires the copies: {:?}",
        retired.data.skills
    );

    // The line comes back: the next update re-places the bytes — an install, said as one.
    rig.seed_feed();
    let back = sweep(&ctx, &plane, &dir);
    assert!(placed.join("SKILL.md").exists());
    let row = back
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(
        row.action,
        PullAction::Installed,
        "re-placed bytes are an install: {row:?}"
    );
    assert!(!row.destinations.is_empty(), "{row:?}");
    let out = crate::render::pull_tty(
        &back.data,
        &back.decisions,
        &back.warnings,
        &back.advisories,
        &back.disclosures,
    );
    assert!(out.contains("installed ("), "{out}");
    assert!(!out.contains("all up to date"), "{out}");
}

// ---------------------------------------------------------------------------------------------
// `-a`/`--dest` on `diff` / `publish` / `update --reset`: naming ONE copy when a bundle sits in
// several folders — including the divergent-copies FREEZE, which is the case the selector exists
// for. Naming the copy IS the choice the freeze asks for, so each verb reads past it rather than
// re-refusing, and the copy nobody named is never touched.
// ---------------------------------------------------------------------------------------------

/// A locally-adopted bundle in TWO of the machine's skills folders, both edited with DIFFERENT
/// bytes — the frozen shape. Returns the rig, the bundle's name, its id (which DIFFERS from the
/// name), and the two dirs (the adopted one first). The folders are ones the registry actually
/// spells, so `-a claude-code` names the second and the `~/` display form names either.
fn frozen_copies(tag: &str) -> (Rig, String, crate::id::SkillId, PathBuf, PathBuf) {
    let rig = Rig::new(tag);
    rig.seed_session();
    // The adopted copy lives in the shared agents folder; a sibling copy sits in Claude Code's.
    let shared = rig.home.0.join(".agents/skills/coolify-deploy");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("SKILL.md"), b"# coolify-deploy\nbase\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &shared).unwrap();
    let id = crate::id::SkillId::parse(added.skill_id.as_deref().unwrap()).unwrap();
    assert_ne!(id.as_str(), "coolify-deploy", "the id and the name differ");
    // The sibling: the SAME base bytes at the same recorded baseline — a clean managed replica
    // until it is edited below.
    let native = rig.home.0.join(".claude/skills/coolify-deploy");
    add_managed_copy(&rig, &id, &native, b"# coolify-deploy\nbase\n");
    // Two DIFFERENT edits: neither copy's bytes are the other's recorded baseline, so they are
    // true competitors — the freeze.
    std::fs::write(shared.join("SKILL.md"), b"# coolify-deploy\nshared edit\n").unwrap();
    std::fs::write(native.join("SKILL.md"), b"# coolify-deploy\nnative edit\n").unwrap();
    (rig, "coolify-deploy".to_owned(), id, shared, native)
}

/// Write `body` into `dir` and record it as a second MANAGED placement of `id`, at the record's
/// current baseline — the multi-folder shape the selector narrows.
fn add_managed_copy(rig: &Rig, id: &crate::id::SkillId, dir: &std::path::Path, body: &[u8]) {
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
    let sp = rig.layout().published(id);
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    let mut map = crate::doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(dir.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: Some("claude-code".to_owned()),
        materialized_sha: Some(lock.bundle_digest),
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
    });
    crate::doc::write_map(&rig.fs, &sp.map, &map).unwrap();
}

/// The freeze REFUSES a bare read and hands back a per-copy menu — and `--dest` reads straight past
/// it. The bypass is the point: the aggregate classification refuses before anything can be picked,
/// so before this a frozen bundle could not even be INSPECTED.
#[test]
fn zz_dest_reads_one_copy_of_a_frozen_bundle_while_the_bare_diff_still_refuses() {
    let (rig, name, id, _shared, native) = frozen_copies("zz-freeze-diff");
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Bare: the typed freeze, naming BOTH copies in both spellings the menu prints.
    let err = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
    )
    .expect_err("a bare diff of divergent copies refuses");
    let ClientError::PlacementsDiverged { copies, .. } = &err else {
        panic!("the typed freeze: {err:?}");
    };
    let displays: Vec<&str> = copies.iter().map(|c| c.display.as_str()).collect();
    assert_eq!(
        displays,
        vec![
            "~/.agents/skills/coolify-deploy",
            "~/.claude/skills/coolify-deploy"
        ]
    );
    let dests: Vec<&str> = copies.iter().map(|c| c.dest.as_str()).collect();
    assert_eq!(dests, vec!["~/.agents/skills", "~/.claude/skills"]);

    // ALL THREE accepted spellings of the SAME copy read that copy — plus `-a`, the registry's
    // sugar for the row spelling.
    for sel in [
        ops::Selection::one(None, Some("~/.claude/skills")),
        ops::Selection::one(None, Some("~/.claude/skills/coolify-deploy")),
        ops::Selection::one(None, Some(&native.display().to_string())),
        ops::Selection::one(Some("claude-code"), None),
    ] {
        let d = ops::diff(&ctx, &name, None, ops::DiffBudget::unlimited(), &sel)
            .expect("the selector reads past the freeze");
        assert!(d.diff.contains("native edit"), "{}", d.diff);
        assert!(!d.diff.contains("shared edit"), "{}", d.diff);
        // The header names WHICH copy — the bundle sits in more than one folder.
        assert_eq!(d.dest.as_deref(), Some("~/.claude/skills/coolify-deploy"));
        assert_eq!(d.skill.as_deref(), Some("coolify-deploy"));
    }

    // The OTHER copy, by its own folder — and the left-hand side is the SAME applied base, which
    // is what makes two `--dest` runs comparable.
    let sp = rig.layout().published(&id);
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    let other = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::one(None, Some("~/.agents/skills")),
    )
    .unwrap();
    assert!(other.diff.contains("shared edit"), "{}", other.diff);
    assert_eq!(other.version_id, lock.base_commit);
}

/// The two refusals the selector owes a person who names the wrong folder: one holding no copy of
/// this bundle (answered with every folder that DOES), and one holding a copy with nothing to act
/// on (answered plainly, rather than doing a no-op and reporting success).
#[test]
fn zz_a_dest_naming_no_copy_or_a_clean_copy_refuses_by_name() {
    let (rig, name, id, _shared, _native) = frozen_copies("zz-dest-refusals");
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::one(None, Some("~/.cursor/skills")),
    )
    .expect_err("a folder holding no copy refuses");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    let message = crate::render::safe_message(&err);
    assert!(
        message.contains("~/.agents/skills/coolify-deploy")
            && message.contains("~/.claude/skills/coolify-deploy"),
        "the refusal names every copy it DOES have: {message}"
    );

    // A third copy at the recorded baseline — a managed replica with nothing edited in it.
    let clean = rig.home.0.join(".codex/skills/coolify-deploy");
    add_managed_copy(&rig, &id, &clean, b"# coolify-deploy\nbase\n");
    let err = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::one(None, Some("~/.codex/skills")),
    )
    .expect_err("a copy with no edits refuses rather than answering nothing");
    let message = crate::render::safe_message(&err);
    assert!(message.starts_with("that copy has no edits"), "{message}");
    assert!(
        message.contains("~/.codex/skills/coolify-deploy"),
        "{message}"
    );

    // A selector plus a `<ref>` asks two different questions at once: the selector narrows the
    // side that holds YOUR edits, and a version-to-version diff has no such side — it reads the
    // same in every folder. Refused whole, with both ways to re-spell it.
    let err = ops::diff(
        &ctx,
        &name,
        Some(&"ab".repeat(32)),
        ops::DiffBudget::unlimited(),
        &ops::Selection::one(None, Some("~/.claude/skills")),
    )
    .expect_err("a selector with a version reference refuses");
    assert_eq!(
        crate::render::safe_message(&err),
        "`--dest`/`-a` names the copy of YOUR edits a diff reads, and a version-to-version diff \
         reads the same in every folder — drop the `<ref>`, or drop the selector"
    );
}

/// The per-copy reset: the loss-led rail unchanged, narrowed to ONE folder. The described delta is
/// that copy's alone, the apply restores only that copy, and the copy left behind keeps its bytes —
/// which is what makes it the single ordinary draft afterwards.
#[test]
fn zz_a_per_copy_reset_drops_one_copys_edits_and_leaves_the_other_alone() {
    let (rig, name, _id, shared, native) = frozen_copies("zz-per-copy-reset");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let sel = ops::Selection::one(Some("claude-code"), None);

    let described = ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        false,
        ops::StoreScope::Here,
        &sel,
    )
    .unwrap();
    let ops::ResetOutcome::Described { items, yes_argv } = &described else {
        panic!("the bare reset DESCRIBES: {described:?}");
    };
    assert!(
        items[0].drop_diff.contains("native edit") && !items[0].drop_diff.contains("shared edit"),
        "the loss shown is THIS copy's: {}",
        items[0].drop_diff
    );
    // The SENTENCES around that delta say the same thing it does. The consent surface names the
    // one folder it takes and states, in the same breath, that the other copy keeps its edits —
    // a describe claiming the whole bundle's edits would be asking for the wrong yes.
    assert_eq!(
        items[0].dest.as_deref(),
        Some("~/.claude/skills/coolify-deploy")
    );
    assert_eq!(
        items[0].others_kept,
        vec!["~/.agents/skills/coolify-deploy"]
    );
    // The sentence states the loss plainly, in the same voice the receipt uses: the delta printed
    // under it is the argument, and a shouted word above it would only make the surface anxious.
    let described_tty = crate::render::reset_describe_tty(items, yes_argv);
    assert!(
        described_tty.starts_with(
            "Reset 'coolify-deploy' in ~/.claude/skills/coolify-deploy — discard \
             local edits to "
        ),
        "{described_tty}"
    );
    assert!(!described_tty.contains("DISCARDS"), "{described_tty}");
    assert!(
        described_tty
            .contains("your other copy in ~/.agents/skills/coolify-deploy keeps its edits"),
        "{described_tty}"
    );
    // The apply command carries the selector — without it `--yes` would discard both copies.
    assert_eq!(
        yes_argv,
        &vec![
            "topos".to_owned(),
            "update".to_owned(),
            name.clone(),
            "-a".to_owned(),
            "claude-code".to_owned(),
            "--reset".to_owned(),
            "--yes".to_owned(),
        ]
    );

    let applied = ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &sel,
    )
    .unwrap();
    let ops::ResetOutcome::Applied(done) = &applied else {
        panic!("`--yes` applies: {applied:?}");
    };
    // The receipt tells the same truth as the describe: the named copy, and what survived.
    let applied_tty = crate::render::reset_applied_tty(done);
    assert!(
        applied_tty.starts_with("Reset 'coolify-deploy' in ~/.claude/skills/coolify-deploy to "),
        "{applied_tty}"
    );
    // No recovery clause: the snapshot lands in whichever store owns the copy, and there is no
    // command a person could type to get it back — an offer nobody can take is not an offer.
    assert!(
        applied_tty.contains(
            "that copy's local edits discarded.\nyour other copy in \
             ~/.agents/skills/coolify-deploy keeps its edits"
        ),
        "{applied_tty}"
    );
    assert!(!applied_tty.contains("snapshot"), "{applied_tty}");
    assert_eq!(
        std::fs::read(native.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nbase\n",
        "the named copy is back at base"
    );
    assert_eq!(
        std::fs::read(shared.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nshared edit\n",
        "the copy nobody named keeps its edits"
    );
    // The freeze is gone: one edited copy left, so it is THE draft and a bare diff reads it.
    let d = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
    )
    .expect("one edited copy is the ordinary draft");
    assert!(d.diff.contains("shared edit"), "{}", d.diff);
}

/// **A reset that names ONE folder does not end the merge.**
///
/// `--dest` narrows a reset so the copies it does not name keep their edits — and a copy still
/// holding its edits is a copy still holding this unfinished merge. Clearing the record there
/// would unblock `publish` while the merge stands in another folder. Git ends a merge at the
/// commit, and refuses to commit while anything is still unresolved; so the record stands until
/// the last copy is settled, and the reset that settles it takes the block and the marked-up copy
/// with it.
#[test]
fn zz_a_per_copy_reset_ends_the_merge_only_once_no_copy_still_holds_it() {
    use topos_types::persisted::{ConflictReason, ConflictState};

    let (rig, name, id, _shared, _native) = frozen_copies("zz-per-copy-reset-conflict");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let sp = rig.layout().published(&id);
    let copy = rig.layout().conflict_copy_dir(
        &crate::sidecar::ConflictDir::parse("coolify-deploy").expect("a plain safe component"),
    );
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), b"<<<<<<< mine\n").unwrap();
    // The record names the workbench AND the bytes topos wrote into it — the untouched signal a
    // reset reads before it removes anything.
    let marked = topos_core::digest::to_hex(&crate::scan::scan(&copy).unwrap().bundle_digest);
    let record = ConflictState {
        schema_version: 1,
        base_commit: "ab".repeat(32),
        base_digest: "ab".repeat(32),
        current_commit: "ab".repeat(32),
        current_digest: "ab".repeat(32),
        draft_commit: "ab".repeat(32),
        draft_digest: "ab".repeat(32),
        result_commit: "ab".repeat(32),
        conflicted_digest: marked,
        copy_dir: Some("coolify-deploy".to_owned()),
        reason: ConflictReason::ThreeWay,
        concluded: None,
        paths: Vec::new(),
    };
    crate::doc::write_doc(&rig.fs, &sp.conflict, &record).unwrap();

    let reset_one = |sel: ops::Selection| {
        let applied = ops::reset(
            &ctx,
            std::slice::from_ref(&name),
            true,
            ops::StoreScope::Here,
            &sel,
        )
        .unwrap();
        let ops::ResetOutcome::Applied(done) = applied else {
            panic!("`--yes` applies");
        };
        done
    };

    // ONE copy taken back to base; the other still holds its edits — so the merge is not over.
    let done = reset_one(ops::Selection::one(Some("claude-code"), None));
    assert!(
        sp.conflict.exists(),
        "a copy still holding this merge keeps the block"
    );
    assert!(copy.exists(), "and the marked-up copy stays with it");
    let tty = crate::render::reset_applied_tty(&done);
    assert!(
        tty.contains("your other copy in ~/.agents/skills/coolify-deploy keeps its edits"),
        "{tty}"
    );
    assert!(
        !tty.contains("hand merge"),
        "nothing survived unread: {tty}"
    );

    // The last copy: now nothing is unresolved, so the block clears and the workbench — still
    // exactly what topos wrote there — goes with it.
    let done = reset_one(ops::Selection::default());
    assert!(!sp.conflict.exists(), "the block is gone with its cause");
    assert!(!copy.exists(), "the marked-up copy goes with the record");
    let tty = crate::render::reset_applied_tty(&done);
    assert!(!tty.contains("publish"), "{tty}");
    assert!(!tty.contains("hand merge"), "{tty}");
}

/// **A reset never deletes a by-hand merge it did not read.** The workbench folder is the only copy
/// of a hand resolution — it sits outside the placement map, so the materializer's snapshot rail
/// never sees it — and `--reset` snapshots every placement while deleting that folder blind. Now it
/// reads it first: bytes topos wrote go, anything else stays and is named on the receipt.
/// `git merge --abort` removes no untracked file either.
#[test]
fn zz_a_reset_leaves_a_hand_merge_it_never_read_and_names_it() {
    use topos_types::persisted::{ConflictReason, ConflictState};

    let (rig, name, id, shared, _native) = frozen_copies("zz-reset-hand-merge");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let sp = rig.layout().published(&id);
    let copy = rig.layout().conflict_copy_dir(
        &crate::sidecar::ConflictDir::parse("coolify-deploy").expect("a plain safe component"),
    );
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), b"<<<<<<< mine\n").unwrap();
    let marked = topos_core::digest::to_hex(&crate::scan::scan(&copy).unwrap().bundle_digest);
    // …and then the person merges it by hand, so the folder no longer holds what topos wrote.
    std::fs::write(
        copy.join("SKILL.md"),
        b"# coolify-deploy\nreconciled by hand\n",
    )
    .unwrap();
    crate::doc::write_doc(
        &rig.fs,
        &sp.conflict,
        &ConflictState {
            schema_version: 1,
            base_commit: "ab".repeat(32),
            base_digest: "ab".repeat(32),
            current_commit: "ab".repeat(32),
            current_digest: "ab".repeat(32),
            draft_commit: "ab".repeat(32),
            draft_digest: "ab".repeat(32),
            result_commit: "ab".repeat(32),
            conflicted_digest: marked,
            copy_dir: Some("coolify-deploy".to_owned()),
            reason: ConflictReason::ThreeWay,
            concluded: None,
            paths: Vec::new(),
        },
    )
    .unwrap();
    // Both copies at once, so the merge really is over and the record really does clear.
    std::fs::write(shared.join("SKILL.md"), b"# coolify-deploy\nbase\n").unwrap();

    let applied = ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    let ops::ResetOutcome::Applied(done) = &applied else {
        panic!("`--yes` applies: {applied:?}");
    };
    assert!(!sp.conflict.exists(), "the block still clears");
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nreconciled by hand\n",
        "the only copy of the hand merge must survive"
    );
    let tty = crate::render::reset_applied_tty(done);
    assert!(
        tty.contains("your hand merge is still in ~/.topos/conflicts/coolify-deploy"),
        "{tty}"
    );
}

/// Publishing ONE copy is the other way out of the freeze, and it is safe: the copy that shipped
/// advances to the new current, the copy that did not is never written, and the competitor test
/// then finds exactly one edited copy — the same shape "a teammate published while I had local
/// edits" has always produced. The receipt carries both halves.
#[test]
fn zz_a_per_copy_publish_leaves_the_other_copy_alone_and_resolves_the_freeze() {
    let (rig, name, _id, shared, native) = frozen_copies("zz-per-copy-publish");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let cc = |_base: &str, _tok: Option<&str>| -> Box<dyn crate::plane::ContributeSource> {
        Box::new(NoContribute)
    };
    let publish = |sel: &ops::Selection| {
        ops::publish(
            &ctx,
            &cc,
            None,
            Some(&session_connect),
            None,
            &name,
            false,
            None,
            None,
            None,
            sel,
        )
    };

    // A BARE publish still refuses — it must never pick for you; `--dest` is the consent.
    let bare = publish(&ops::Selection::default())
        .expect_err("a bare publish of divergent copies refuses");
    assert_eq!(bare.code(), "PLACEMENTS_DIVERGED");

    let outcome = publish(&ops::Selection::one(None, Some("~/.agents/skills")))
        .expect("the named copy publishes");
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    assert_eq!(
        data.from_placement.as_deref(),
        Some("~/.agents/skills/coolify-deploy")
    );
    assert_eq!(data.other_edited, vec!["~/.claude/skills/coolify-deploy"]);

    // THE safety property: the copy nobody named is byte-for-byte untouched.
    assert_eq!(
        std::fs::read(native.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nnative edit\n"
    );
    assert_eq!(
        std::fs::read(shared.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nshared edit\n",
        "the published copy keeps the bytes it shipped"
    );
    // And the freeze has resolved: one edited copy remains, so it is the single ordinary draft.
    let d = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
    )
    .expect("the survivor is the single draft");
    assert!(d.diff.contains("native edit"), "{}", d.diff);
}
