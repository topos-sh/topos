//! The rig every file in this suite shares: the fake `$HOME` + work dir, the plane/directory/git
//! fakes the reconcile runs against, version construction, the sweep entry points at each scope,
//! and the cross-file fixtures (the forge archive + gate, the workspace-reference and bare-name
//! add rigs, the project-custody checkout, and the contribute fakes that land a publish).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::Commit;
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireMe, WireProposalIndex,
    WireSkillIndex, WireSkillIndexEntry, WireSkillLog,
};
use topos_types::results::ExchangeFault;

use crate::ctx::Ctx;
use crate::error::{ClientError, FetchFault};
use crate::fs_seam::RealFs;
use crate::git_source::RepoHead;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::plane::{
    DeliverySkill, DeliverySnapshot, DeliverySource, DirectorySource, FetchedFile, FetchedVersion,
    InertFollow, InertPlane, KnownCurrent, LinkStatus, PlaneError, PlaneSource, PointerFetch,
};
use crate::sessions::{self, SESSION_ACTIVE, Session};
use crate::sidecar::Layout;
use crate::test_support::{MockHarness, native_skills_root};
use crate::{ops, sync_status};

pub(super) const WS: &str = "w_eng";
pub(super) const HOST: &str = "acme.test";
pub(super) const WS_NAME: &str = "eng";

pub(super) struct Scratch(pub(super) PathBuf);
impl Scratch {
    pub(super) fn new(tag: &str) -> Self {
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

/// The placement adapter for this suite: the naming ladder under the fixture's own skills root.
pub(super) fn tmp_harness(skills_root: PathBuf) -> MockHarness {
    MockHarness::ladder(skills_root)
}

/// The rig: a fake $HOME (with `.claude/` so claude-code detects), a sidecar under `<home>/.topos`,
/// and a work dir. The cwd each test chooses (a project checkout, or the bare work dir).
pub(super) struct Rig {
    pub(super) home: Scratch,
    pub(super) work: Scratch,
    pub(super) fs: RealFs,
    pub(super) ids: SeqIds,
    pub(super) clock: FixedClock,
    pub(super) harness: MockHarness,
}
impl Rig {
    pub(super) fn new(tag: &str) -> Self {
        let home = Scratch::new(&format!("{tag}-home"));
        // Make claude-code DETECTED (its config home exists) so the shared-dir-first policy
        // engages: person scope → `<home>/.agents/skills`, project scope → `<proj>/.agents/skills`.
        std::fs::create_dir_all(home.0.join(".claude")).unwrap();
        let work = Scratch::new(&format!("{tag}-work"));
        // The fake adapter answers with the ROW's root — the same answer the placement engine
        // resolves, which is the one source of a target dir. (An adapter naming some other
        // directory would not move a byte; it would only make this rig disagree with the engine
        // about where to look.)
        let harness = tmp_harness(native_skills_root(&home.0));
        Self {
            home,
            work,
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness,
        }
    }

    /// Where this rig's ACTIVE-harness (native, machine-scope) copies land.
    pub(super) fn skills(&self) -> PathBuf {
        native_skills_root(&self.home.0)
    }

    /// A path as the RECEIPTS spell it: a machine path under the home abbreviates to `~/…` (see
    /// `ops::inventory::pretty`), which is what every destination and kept row carries.
    pub(super) fn pretty(&self, path: &std::path::Path) -> String {
        match path.strip_prefix(&self.home.0) {
            Ok(rest) => format!("~/{}", rest.display()),
            Err(_) => path.display().to_string(),
        }
    }
    pub(super) fn layout(&self) -> Layout {
        Layout::new(&self.home.0.join(".topos"))
    }
    pub(super) fn ctx_at<'a>(&'a self, cwd: Option<&std::path::Path>) -> Ctx<'a> {
        self.ctx_with_progress(cwd, crate::progress::silent())
    }
    /// The same context carrying a NAMED progress sink — for the suites that assert the activity
    /// lines themselves rather than what the run left on disk.
    pub(super) fn ctx_with_progress<'a>(
        &'a self,
        cwd: Option<&std::path::Path>,
        progress: &'a dyn crate::progress::ProgressSink,
    ) -> Ctx<'a> {
        Ctx {
            progress,
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: "d_test".into(),
            layout: self.layout(),
            harness: &self.harness,
            triggers: crate::ops::Triggers::active_only(&crate::ops::INERT_TRIGGER),
            plane: &InertPlane,
            follow: &InertFollow,
            roots: Some(crate::ctx::AgentRoots {
                home: self.home.0.clone(),
                cwd: cwd.map(std::path::Path::to_path_buf),
            }),
        }
    }
    pub(super) fn seed_session(&self) {
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
    /// A SECOND login on the same host — the two-workspace machine the resolution rules are
    /// about. No default is set by this (a `topos workspace use` is the only thing that stars
    /// one), so a machine seeded with both is exactly the ambiguous case.
    pub(super) fn seed_other_session(&self, workspace_id: &str, workspace_name: &str) {
        sessions::upsert_session(
            &self.fs,
            &self.layout(),
            Session {
                host: HOST.into(),
                base_url: format!("https://{HOST}/api"),
                workspace_id: workspace_id.into(),
                workspace_name: workspace_name.into(),
                display_name: workspace_name.into(),
                session_id: format!("sn_{workspace_name}"),
                credential: "cred-2".into(),
                status: SESSION_ACTIVE.into(),
                logged_in_at: 2,
            },
        )
        .unwrap();
    }
    /// Unstar the machine default: several live logins and nothing saying which one commands act
    /// on. Reached by a sessions file this build did not write (an older topos, a hand edit) —
    /// and the one state a lookup must REFUSE in rather than guess or scan.
    pub(super) fn clear_default(&self) {
        let path = self.layout().sessions_path();
        let mut doc: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        doc.as_object_mut().unwrap().remove("default");
        std::fs::write(&path, serde_json::to_vec(&doc).unwrap()).unwrap();
    }
    /// Write the GLOBAL manifest (`~/.topos/topos.toml`) — the person scope's complete recipe.
    pub(super) fn write_global(&self, body: &str) {
        let home = self.layout().home().to_path_buf();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(crate::manifest::MANIFEST_FILE), body).unwrap();
    }
    /// The ordinary machine recipe after a login: the seeded session's feed row — exactly the
    /// line `topos login` writes on this machine's first connection to the workspace.
    pub(super) fn seed_feed(&self) {
        self.write_global(&format!(
            "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
        ));
    }
}

/// A project checkout carrying `body` as its `topos.toml`.
pub(super) fn project(tag: &str, body: &str) -> Scratch {
    let proj = Scratch::new(tag);
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), body).unwrap();
    proj
}

/// A version whose bytes reproduce a REAL commit id (the engine re-verifies on apply).
pub(in crate::tests) struct Version {
    pub(super) id: [u8; 32],
    pub(super) digest: [u8; 32],
    pub(super) fetched: FetchedVersion,
}
pub(in crate::tests) fn mk_version(files: &[(&str, FileMode, &[u8])]) -> Version {
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
pub(super) fn one_file(body: &[u8]) -> Version {
    mk_version(&[("SKILL.md", FileMode::Regular, body)])
}

/// Every file under a dir as sorted `(rel-path, bytes)` rows — the byte-destruction witness.
pub(super) fn snapshot_dir(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
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
pub(super) fn store_versions(layout: &Layout, skill_id: &str) -> usize {
    let sid = crate::id::SkillId::parse(skill_id).unwrap();
    let sp = layout.published(&sid);
    topos_gitstore::Store::open(&sp.store)
        .unwrap()
        .list_versions()
        .unwrap()
        .len()
}

pub(super) type CallLog = Arc<Mutex<Vec<String>>>;

/// One reported applied row as the fake records it: `(skill_id, version hex, mcp states)`.
pub(super) type ReportedRow = (String, String, Vec<topos_types::results::McpAgentState>);

/// The per-session plane fake: a delivery script + versions keyed by `(skill, version-hex)`.
#[derive(Clone)]
pub(super) struct FakePlane {
    pub(super) delivery: Arc<Mutex<Result<DeliverySnapshot, &'static str>>>,
    pub(super) versions: HashMap<(String, String), FetchedVersion>,
    pub(super) log: CallLog,
    /// The LAST applied report's rows, `(skill_id, version hex)` — the wire carries exactly one
    /// row per (session, bundle), so this is what the cross-store pick actually reported —
    /// `(skill_id, version hex, per-agent mcp states)`.
    pub(super) reported: Arc<Mutex<Vec<ReportedRow>>>,
}
impl FakePlane {
    pub(super) fn new(log: CallLog) -> Self {
        Self {
            delivery: Arc::new(Mutex::new(Ok(empty_snapshot()))),
            versions: HashMap::new(),
            log,
            reported: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub(super) fn with_version(mut self, skill: &str, v: &Version) -> Self {
        self.versions.insert(
            (skill.to_owned(), topos_core::digest::to_hex(&v.id)),
            v.fetched.clone(),
        );
        self
    }
    pub(super) fn serve(&self, snap: DeliverySnapshot) {
        *self.delivery.lock().unwrap() = Ok(snap);
    }
    /// Serve exactly these bundles (the common case).
    pub(super) fn serves(&self, skills: Vec<DeliverySkill>) {
        self.serve(DeliverySnapshot {
            skills,
            ..empty_snapshot()
        });
    }
    pub(super) fn serve_not_found(&self) {
        *self.delivery.lock().unwrap() = Err("nf");
    }
    pub(super) fn serve_unreachable(&self) {
        *self.delivery.lock().unwrap() = Err("unreachable");
    }
    /// The plane ANSWERED, with a failure (a 5xx) — not the same fact as an unreachable server.
    pub(super) fn serve_unavailable(&self) {
        *self.delivery.lock().unwrap() = Err("unavailable");
    }
    /// The plane answered and the answer got CUT OFF mid-body — the other half of the same
    /// variant, where nothing says the server itself failed.
    pub(super) fn serve_truncated(&self) {
        *self.delivery.lock().unwrap() = Err("truncated");
    }
    /// The plane ANSWERED, unreadably (garbled bytes) — not a network fault at all.
    pub(super) fn serve_malformed(&self) {
        *self.delivery.lock().unwrap() = Err("malformed");
    }
}
pub(super) fn empty_snapshot() -> DeliverySnapshot {
    DeliverySnapshot {
        mcp_servers: Vec::new(),
        skills: Vec::new(),
        proposals_awaiting: 0,
        notices: Vec::new(),
        staleness_window_ms: 604_800_000,
        link_status: LinkStatus::Active,
        declined: Vec::new(),
    }
}
/// [`delivered`] at an explicit pointer generation — the real server bumps it on every move.
pub(super) fn delivered_at(
    skill_id: &str,
    name: &str,
    v: &Version,
    generation: u64,
) -> DeliverySkill {
    DeliverySkill {
        generation,
        ..delivered(skill_id, name, v)
    }
}

pub(super) fn delivered(skill_id: &str, name: &str, v: &Version) -> DeliverySkill {
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
                    r.version_id.clone(),
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
pub(in crate::tests) struct FakeDirectory {
    pub(super) skills: Vec<WireSkillIndexEntry>,
    /// The catalog's SECOND list — the workspace's connected servers, carrying their documents.
    pub(super) mcp_servers: Vec<topos_types::requests::WireMcpIndexEntry>,
    /// The server revisions this workspace still SERVES BY ID (the by-revision lane's stock).
    /// Empty is a workspace that serves none — which is also every server older than the lane.
    pub(super) mcp_revisions: Vec<topos_types::requests::WireMcpIndexEntry>,
    pub(super) channels: Vec<WireChannelEntry>,
    /// When set, the index reads fail (a transport fault) — the freeze suites flip it.
    pub(super) unavailable: Arc<Mutex<bool>>,
}

impl FakeDirectory {
    pub(in crate::tests) fn new(
        skills: Vec<WireSkillIndexEntry>,
        channels: Vec<WireChannelEntry>,
    ) -> Self {
        Self {
            skills,
            mcp_servers: Vec::new(),
            mcp_revisions: Vec::new(),
            channels,
            unavailable: Arc::new(Mutex::new(false)),
        }
    }

    /// The same fake, serving one connected SERVER beside whatever file bundles it already has.
    /// The served revision is by-revision readable too: a workspace serves what it has stored.
    pub(in crate::tests) fn with_server(
        mut self,
        entry: topos_types::requests::WireMcpIndexEntry,
    ) -> Self {
        self.mcp_revisions.push(entry.clone());
        self.mcp_servers.push(entry);
        self
    }

    /// One further revision this workspace still serves BY ID — the older document a `topos.lock`
    /// names. It is not in the catalog list: the catalog serves `current`, this lane serves any.
    pub(in crate::tests) fn with_revision(
        mut self,
        entry: topos_types::requests::WireMcpIndexEntry,
    ) -> Self {
        self.mcp_revisions.push(entry);
        self
    }

    /// A workspace whose server predates the by-revision lane: it serves NO revision by id, so
    /// every such read answers the lane's uniform miss.
    pub(in crate::tests) fn serving_no_revisions(mut self) -> Self {
        self.mcp_revisions.clear();
        self
    }
    pub(super) fn set_unavailable(&self, v: bool) {
        *self.unavailable.lock().unwrap() = v;
    }
    pub(super) fn check_reachable(&self) -> Result<(), ClientError> {
        if *self.unavailable.lock().unwrap() {
            return Err(ClientError::Plane("directory unreachable".into()));
        }
        Ok(())
    }
}
pub(in crate::tests) fn catalog_entry(
    skill_id: &str,
    name: &str,
    v: &Version,
) -> WireSkillIndexEntry {
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
            mcp_servers: self.mcp_servers.clone(),
            skills: self.skills.clone(),
        })
    }
    fn mcp_revision(
        &self,
        _ws: &str,
        skill_id: &str,
        revision_id: &str,
    ) -> Result<Option<topos_types::requests::WireMcpIndexEntry>, ClientError> {
        self.check_reachable()?;
        Ok(self
            .mcp_revisions
            .iter()
            .find(|e| e.skill_id == skill_id && e.revision_id == revision_id)
            .cloned())
    }
    fn proposals_index(&self, _ws: &str) -> Result<WireProposalIndex, ClientError> {
        unreachable!()
    }
    fn skill_log(&self, _ws: &str, skill_id: &str) -> Result<WireSkillLog, ClientError> {
        // A catalog with no PLANE history: `log`'s plane half reaches this fake now that it takes
        // the bundle's own workspace lane instead of building the whole session universe (whose
        // `me` read this fake refuses). An empty history leaves the local log exactly as it was.
        self.check_reachable()?;
        Ok(WireSkillLog {
            skill_id: skill_id.to_owned(),
            name: String::new(),
            kind: "skill".to_owned(),
            status: "active".to_owned(),
            base_name: None,
            versions: Vec::new(),
            proposals: Vec::new(),
        })
    }
    fn protect_skill(&self, _ws: &str, _s: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn protect_channel(&self, _ws: &str, _c: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn add_mcp_server(
        &self,
        _w: &str,
        _b: topos_types::requests::McpAddRequest,
    ) -> Result<topos_types::requests::McpAddedData, ClientError> {
        unreachable!()
    }
}

/// Write lanes the reconcile suites never exercise.
pub(super) struct NoContribute;
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
pub(in crate::tests) struct NoGovernance;
impl crate::plane::GovernanceSource for NoGovernance {
    fn invite(
        &self,
        _w: &str,
        _b: topos_types::requests::InvitationRequest,
    ) -> Result<topos_types::requests::InvitationData, ClientError> {
        unreachable!("no governance in these flows")
    }
}

pub(super) fn connect<'a>(
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

/// The same connector, RECORDING which workspaces a verb built a lane for — the one way to prove
/// a lookup acted on one workspace instead of sweeping every login.
pub(super) fn connect_recording<'a>(
    plane: &'a FakePlane,
    dir: &'a FakeDirectory,
    seen: &'a Mutex<Vec<String>>,
) -> impl Fn(&Session) -> ops::SessionTransports + 'a {
    move |s: &Session| {
        seen.lock().unwrap().push(s.workspace_id.clone());
        ops::SessionTransports {
            plane: Box::new(plane.clone()),
            directory: Box::new(dir.clone()),
            contribute: Box::new(NoContribute),
            governance: Box::new(NoGovernance),
        }
    }
}

/// A bare hand-run `topos update` (no forge lane — the background posture): the scope rule applies,
/// so it converges the PROJECT when a manifest covers the cwd and the MACHINE otherwise. It is the
/// verb `update` IS, so it carries [`ops::LockMode::Update`] — every follow row re-resolves to
/// current and the lock is rewritten; the install-mode postures are `quiet_sweep` and the
/// scope-selected sweeps below.
pub(super) fn sweep(ctx: &Ctx<'_>, plane: &FakePlane, dir: &FakeDirectory) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        None,
        &ops::ManifestUpdateOpts {
            lock: ops::LockMode::Update,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap()
}

/// The reconcile under an explicit scope selector — `-g` (machine), or the hook sweep's BOTH.
pub(super) fn sweep_scoped(
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
pub(super) fn sweep_both(
    ctx: &Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
) -> ops::PullOutcome {
    sweep_scoped(ctx, plane, dir, ops::UpdateScope::Both)
}

/// THE HOOK'S ACTUAL POSTURE — what `topos update --quiet` really runs: both scopes, the forge
/// lane present, and the forge dialed only when its own clock says the interval has elapsed.
pub(super) fn quiet_sweep(
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
pub(super) fn update_now(
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
pub(super) fn forge_interval_elapsed(rig: &Rig) {
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
pub(super) fn forge_next_due(rig: &Rig, source: &str, git_ref: &str, scope: &str) -> i64 {
    crate::forge_check::read(&rig.fs, &rig.layout())
        .sources
        .get(&crate::forge_check::question(source, git_ref))
        .and_then(|c| c.next_check_at.get(scope).copied())
        .unwrap_or(0)
}

/// Build a real `.tar.gz` with a `TOP/` prefix over `(repo-relative path, bytes)` entries.
pub(super) fn build_repo_targz(top: &str, entries: &[(&str, &[u8])]) -> Vec<u8> {
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
pub(super) struct FakeGit {
    pub(super) archive: Arc<Mutex<Vec<u8>>>,
    pub(super) fetches: Arc<Mutex<u32>>,
    pub(super) probes: Arc<Mutex<u32>>,
    /// What every call answers with instead of the archive, once set.
    pub(super) fault: Arc<Mutex<Option<FetchFault>>>,
    /// What the probe reports the repo has been renamed to.
    pub(super) renamed: Arc<Mutex<Option<(String, String)>>>,
}
impl FakeGit {
    pub(super) fn new(targz: Vec<u8>) -> Self {
        Self {
            archive: Arc::new(Mutex::new(targz)),
            fetches: Arc::new(Mutex::new(0)),
            probes: Arc::new(Mutex::new(0)),
            fault: Arc::new(Mutex::new(None)),
            renamed: Arc::new(Mutex::new(None)),
        }
    }
    pub(super) fn serve(&self, targz: Vec<u8>) {
        *self.archive.lock().unwrap() = targz;
    }
    pub(super) fn fetches(&self) -> u32 {
        *self.fetches.lock().unwrap()
    }
    pub(super) fn probes(&self) -> u32 {
        *self.probes.lock().unwrap()
    }
    /// Every call from now on fails this way.
    pub(super) fn fail_with(&self, fault: FetchFault) {
        *self.fault.lock().unwrap() = Some(fault);
    }
    pub(super) fn rename_to(&self, owner: &str, repo: &str) {
        *self.renamed.lock().unwrap() = Some((owner.to_owned(), repo.to_owned()));
    }
    /// The commit the current archive carries — what an honest probe of it reports.
    pub(super) fn head(&self) -> String {
        crate::git_source::extract_tree(&self.archive.lock().unwrap().clone())
            .ok()
            .and_then(|t| t.commit)
            .unwrap_or_default()
    }
    pub(super) fn injected(&self) -> Option<ClientError> {
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
pub(super) fn gate_add(
    ctx: &Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    git: &FakeGit,
    raw: &str,
) {
    match ops::add_reference(
        ctx,
        &connect(plane, dir),
        Some(git as &dyn crate::git_source::GitTarballSource),
        raw,
        true,
        true,
        &Default::default(),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied { .. } => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
}

/// The rig's fixed wall clock, as the millis the freshness cache stamps.
pub(super) fn rig_now(rig: &Rig) -> i64 {
    i64::try_from(rig.clock.0).expect("the rig clock fits an i64")
}

/// The fault the freshness cache recorded for a workspace's LAST exchange — by ID, the way the
/// cache is keyed.
pub(super) fn recorded_fault(rig: &Rig, workspace_id: &str) -> Option<ExchangeFault> {
    sync_status::read(&rig.fs, &rig.layout())
        .unwrap()
        .workspaces
        .get(workspace_id)
        .and_then(|e| e.last_exchange_fault)
}

pub(super) const DAY_MS: i64 = 86_400_000;

/// The catalog + session fixture the workspace-reference add arms resolve through.
pub(super) fn add_rig(tag: &str) -> (Rig, FakePlane, FakeDirectory, Version) {
    let rig = Rig::new(tag);
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    (rig, plane, dir, v)
}

pub(super) fn applied_add(
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
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied { data: d, .. } => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    }
}

/// [`applied_add`] carrying a `--dest` selection — the destination-extending form.
pub(super) fn applied_dest_add(
    ctx: &Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    raw: &str,
    dests: &[&str],
) -> topos_types::results::AddData {
    applied_selected_add(ctx, plane, dir, raw, &[], dests)
}

/// [`applied_add`] carrying the WHOLE `-a`/`--dest` selection, agents and folders alike.
pub(super) fn applied_selected_add(
    ctx: &Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    raw: &str,
    agents: &[&str],
    dests: &[&str],
) -> topos_types::results::AddData {
    let selection = crate::ops::dest_select::Selection::new(
        &agents.iter().map(|a| (*a).to_owned()).collect::<Vec<_>>(),
        &dests.iter().map(|d| (*d).to_owned()).collect::<Vec<_>>(),
    );
    match ops::add_reference(
        ctx,
        &connect(plane, dir),
        None,
        raw,
        true,
        false,
        &selection,
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied { data: d, .. } => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    }
}

/// Install the feed's one bundle and return the placed dir.
pub(super) fn install_feed_deploy(rig: &Rig, plane: &FakePlane, dir: &FakeDirectory) -> PathBuf {
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, plane, dir);
    let placed = rig.skills().join("deploy");
    assert!(placed.join("SKILL.md").exists());
    placed
}

/// A contribute fake that lands every publish: it re-derives the candidate's commit id exactly
/// as the plane would (server rehash) and answers OK with the moved pointer.
pub(super) struct OkPublish;
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

/// A contribute fake for the `--propose` arm: the proposal LANDS remotely (NEEDS_REVIEW);
/// `current` does not move, so the local sync stays GENESIS-observed.
pub(super) struct OkPropose;
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

/// The ways out an ambiguity refusal offers, as the runnable `topos remove …` lines both surfaces
/// print. The candidates ride the ACTIONS (one per candidate, the failing verb re-spelled), not the
/// sentence — so this, not `err.to_string()`, is where "every way out is named, paste-ready" is
/// proven.
pub(super) fn ways_out(err: &ClientError) -> String {
    // The invocation these suites make: `topos remove <token>` — one verb, one target, so the
    // reconstruction is faithful and the ways out are offered.
    let argv = vec!["remove".to_owned(), "deploy".to_owned()];
    crate::render::err_hint_tty("remove", &argv, err).unwrap_or_default()
}

/// A channel entry carrying `skills`.
pub(super) fn channel(name: &str, skills: &[(&str, &str)]) -> WireChannelEntry {
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

/// The probe name the bare-name ladder resolves — deliberately IMPROBABLE. Discovery also walks
/// agent roots ambient env configures (a developer's real machine), and a genuine skill named
/// like `deploy` there would flip a Subscribe into an Adopt under `cargo test`.
pub(super) const BARE: &str = "zx-bare-probe";

/// [`add_rig`]'s twin for the bare-name ladder: the one catalog skill is named [`BARE`].
pub(super) fn bare_rig(tag: &str) -> (Rig, FakePlane, FakeDirectory, Version) {
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
pub(super) fn untracked_skill(root: &std::path::Path, name: &str, body: &[u8]) -> PathBuf {
    let dir = root.join(".aider-desk/skills").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
    dir
}

/// Plan a bare `add <target>` exactly as the composition root does: the machine's discovery roots
/// (home + the working directory) against every connected session's catalog.
pub(super) fn bare_plan(
    rig: &Rig,
    plane: &FakePlane,
    dir: &FakeDirectory,
    target: &str,
    subscribe: bool,
) -> Result<ops::BareAddPlan, ClientError> {
    plan_bare_add_in(rig, plane, dir, target, subscribe, None)
}

/// [`bare_plan`] with the global `--workspace` selector (a canonicalized workspace id).
pub(super) fn plan_bare_add_in(
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
        ops::BareAdd {
            workspace,
            ..bare_opts(subscribe)
        },
    )
}

/// A project-scope invocation naming NO destinations — the ordinary bare `add <name>`.
pub(super) fn bare_opts(subscribe: bool) -> ops::BareAdd<'static> {
    ops::BareAdd {
        subscribe,
        dest_selected: false,
        global: false,
        workspace: None,
    }
}

/// A SECOND connected workspace, on its own server — what makes a bare name ambiguous across
/// teams rather than across harnesses.
pub(super) fn seed_second_session(rig: &Rig) {
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

/// Every candidate spelling an ambiguity answer offers, in print order.
pub(super) fn chooser_candidates(err: &ClientError) -> Vec<String> {
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], err);
    envelope.data["candidates"]
        .as_array()
        .expect("the chooser carries its candidates")
        .iter()
        .map(|c| c.as_str().expect("a spelling").to_owned())
        .collect()
}

/// A one-file skill source at `dir` — the adopt-in-place fixture.
pub(super) fn skill_source(dir: &std::path::Path, body: &[u8]) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
}

/// The composition root's OWN path-adopt sequence, in one call (mirrors `app.rs`): the scope is
/// resolved BEFORE any byte moves, the adopt runs against that scope's store, and the row is
/// written into the SAME resolved target — never re-resolved in between.
pub(super) fn scoped_path_add(
    ctx: &Ctx<'_>,
    source: &std::path::Path,
    global: bool,
) -> Result<topos_types::results::AddData, ClientError> {
    let scope = ops::add_scope(ctx, global)?;
    let mut data = ops::adopt_path(ctx, &scope, source, ops::KindDeclared::No)?;
    ops::note_added_path_in(ctx, &mut data, &scope.target, source)?;
    Ok(data)
}

/// [`FakeDirectory`] that also ANSWERS `me` — the one read the resolver universe needs before a
/// workspace's names exist at all. (The bare fake leaves `me` absent on purpose: a publish's
/// post-landing read is best-effort there.)
#[derive(Clone)]
pub(super) struct NamedDirectory(pub(super) FakeDirectory);
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
        })
    }
    fn channels_index(&self, ws: &str) -> Result<WireChannelIndex, ClientError> {
        self.0.channels_index(ws)
    }
    fn skills_index(&self, ws: &str) -> Result<WireSkillIndex, ClientError> {
        self.0.skills_index(ws)
    }
    fn mcp_revision(
        &self,
        ws: &str,
        s: &str,
        r: &str,
    ) -> Result<Option<topos_types::requests::WireMcpIndexEntry>, ClientError> {
        self.0.mcp_revision(ws, s, r)
    }
    fn proposals_index(&self, ws: &str) -> Result<WireProposalIndex, ClientError> {
        self.0.proposals_index(ws)
    }
    fn skill_log(&self, ws: &str, s: &str) -> Result<WireSkillLog, ClientError> {
        self.0.skill_log(ws, s)
    }
    fn protect_skill(&self, ws: &str, s: &str, l: &str) -> Result<(), ClientError> {
        self.0.protect_skill(ws, s, l)
    }
    fn protect_channel(&self, ws: &str, c: &str, l: &str) -> Result<(), ClientError> {
        self.0.protect_channel(ws, c, l)
    }

    fn add_mcp_server(
        &self,
        ws: &str,
        b: topos_types::requests::McpAddRequest,
    ) -> Result<topos_types::requests::McpAddedData, ClientError> {
        self.0.add_mcp_server(ws, b)
    }
}

/// A checkout whose `topos.toml` delivers `deploy`, swept to `v` — custody in the PROJECT's store,
/// nothing in the machine's. Returns the checkout and the placed dir.
pub(super) fn project_custody(
    tag: &str,
    rig: &Rig,
    plane: &FakePlane,
    dir: &FakeDirectory,
) -> Scratch {
    let proj = project(
        tag,
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
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

pub(super) fn sid(id: &str) -> crate::id::SkillId {
    crate::id::SkillId::parse(id).unwrap()
}

/// A gate-valid `server.json` for the MCP rows below (the converge re-runs the whole gate).
/// One CONNECTED SERVER as the catalog serves it — the document inline, at a revision of the
/// shape the catalog mints.
pub(in crate::tests) fn catalog_server(
    skill_id: &str,
    name: &str,
    url: &str,
) -> topos_types::requests::WireMcpIndexEntry {
    topos_types::requests::WireMcpIndexEntry {
        skill_id: skill_id.into(),
        name: name.into(),
        kind: "mcp".into(),
        status: "active".into(),
        display_name: None,
        revision_id: format!("mcpr_{}", "a".repeat(32)),
        document: serde_json::from_str(&mcp_server_json(url)).expect("a server document"),
        pinned: None,
        revoked: None,
        updated_at: 0,
    }
}

/// The same served row at a NAMED revision — what a `topos.lock` records, and what the fixtures
/// that exercise one need two of.
pub(in crate::tests) fn catalog_server_at(
    skill_id: &str,
    name: &str,
    url: &str,
    revision_id: &str,
) -> topos_types::requests::WireMcpIndexEntry {
    topos_types::requests::WireMcpIndexEntry {
        revision_id: revision_id.to_owned(),
        ..catalog_server(skill_id, name, url)
    }
}

pub(super) fn mcp_server_json(url: &str) -> String {
    format!(
        "{{\"name\":\"io.test/x\",\"description\":\"A test server.\",\"version\":\"1.0.0\",\
         \"remotes\":[{{\"type\":\"streamable-http\",\"url\":\"{url}\"}}]}}"
    )
}

pub(super) fn sel(agents: &[&str], dests: &[&str]) -> ops::Selection {
    ops::Selection {
        agents: agents.iter().map(|s| (*s).to_owned()).collect(),
        dests: dests.iter().map(|s| (*s).to_owned()).collect(),
    }
}

/// The machine-wide manifest as it stands right now.
pub(super) fn global_text(rig: &Rig) -> String {
    std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE))
        .unwrap_or_default()
}
