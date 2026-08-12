//! The rig every file in this suite shares: a fake `$HOME` + checkout, the fake plane/directory
//! the sweep runs against, the SYNTHETIC harness descriptors (every surface Home-rooted, so no
//! test can touch a real machine's config), and the dest/detection seeds the reconcile fixtures
//! ride.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::Commit;
use topos_harness::mcp::McpDialect;
use topos_harness::registry::{self, KnownHarness};
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireMe, WireProposalIndex, WireSkillIndex,
    WireSkillIndexEntry, WireSkillLog,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::RealFs;
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::mcp_engine::{self, DemandedBundle, McpDemand, ScopeIo};
use crate::ops;
use crate::plane::{
    AppliedSkillReport, DeliverySkill, DeliverySnapshot, DeliverySource, DirectorySource,
    FetchedFile, FetchedVersion, InertFollow, InertPlane, KnownCurrent, LinkStatus, PlaneError,
    PlaneSource, PointerFetch,
};
use crate::sessions::{self, SESSION_ACTIVE, Session};
use crate::sidecar::Layout;
use crate::test_support::MockHarness;

pub(super) const WS: &str = "w_eng";
pub(super) const HOST: &str = "acme.test";
pub(super) const WS_NAME: &str = "eng";

// =================================================================================================
// The rig (the manifest_reconcile idioms, over a fake HOME).
// =================================================================================================

pub(super) struct Scratch(pub(super) PathBuf);
impl Scratch {
    pub(super) fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-mcpe-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Canonical, so recorded/derived paths compare equal on macOS's symlinked $TMPDIR.
        let dir = dir.canonicalize().unwrap_or(dir);
        Self(dir)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The placement adapter for this suite: the naming ladder under a fixture skills root.
pub(super) fn tmp_harness(skills_root: PathBuf) -> MockHarness {
    MockHarness::ladder(skills_root)
}

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
        let work = Scratch::new(&format!("{tag}-work"));
        let harness = tmp_harness(work.0.join("skills"));
        Self {
            home,
            work,
            fs: RealFs,
            ids: SeqIds::new("s"),
            clock: FixedClock(1_700_000_000_000),
            harness,
        }
    }
    pub(super) fn layout(&self) -> Layout {
        Layout::new(&self.home.0.join(".topos"))
    }
    pub(super) fn ctx_at<'a>(&'a self, cwd: Option<&Path>) -> Ctx<'a> {
        Ctx {
            progress: crate::progress::silent(),
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
                cwd: cwd.map(Path::to_path_buf),
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
    pub(super) fn write_global(&self, body: &str) {
        let home = self.layout().home().to_path_buf();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(crate::manifest::MANIFEST_FILE), body).unwrap();
    }
}

// A version whose bytes reproduce a REAL commit id (the engine re-verifies on apply).
pub(super) struct Version {
    pub(super) id: [u8; 32],
    pub(super) digest: [u8; 32],
    pub(super) fetched: FetchedVersion,
}
pub(super) fn mk_version(files: &[(&str, &[u8])]) -> Version {
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|(p, b)| ManifestEntry {
            path: (*p).to_owned(),
            mode: FileMode::Regular,
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
                .map(|(p, b)| FetchedFile {
                    path: (*p).to_owned(),
                    mode: FileMode::Regular,
                    bytes: b.to_vec(),
                })
                .collect(),
        },
    }
}

/// The common server.json fixture (no auth hint = Unknown) — GATE-VALID: the converge re-runs
/// the full `mcp_validate` gate on every demand's bytes, exactly as real stored bundles passed
/// it at publish.
pub(super) fn server_json(url: &str) -> String {
    format!(
        "{{\"name\":\"io.test/x\",\"description\":\"A test server.\",\"version\":\"1.0.0\",\
         \"remotes\":[{{\"type\":\"streamable-http\",\"url\":\"{url}\",\
         \"headers\":[{{\"name\":\"X-Team\",\"value\":\"eng\"}}]}}]}}"
    )
}

pub(super) type CallLog = Arc<Mutex<Vec<String>>>;
pub(super) type ReportedRow = (String, String, Vec<topos_types::results::McpAgentState>);

#[derive(Clone)]
pub(super) struct FakePlane {
    pub(super) delivery: Arc<Mutex<Result<DeliverySnapshot, &'static str>>>,
    pub(super) versions: BTreeMap<(String, String), FetchedVersion>,
    pub(super) reported: Arc<Mutex<Vec<ReportedRow>>>,
    pub(super) log: CallLog,
}
impl FakePlane {
    pub(super) fn new() -> Self {
        Self {
            delivery: Arc::new(Mutex::new(Ok(empty_snapshot()))),
            versions: BTreeMap::new(),
            reported: Arc::new(Mutex::new(Vec::new())),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
    pub(super) fn with_version(mut self, skill: &str, v: &Version) -> Self {
        self.versions.insert(
            (skill.to_owned(), topos_core::digest::to_hex(&v.id)),
            v.fetched.clone(),
        );
        self
    }
    pub(super) fn serves(&self, skills: Vec<DeliverySkill>) {
        *self.delivery.lock().unwrap() = Ok(DeliverySnapshot {
            skills,
            ..empty_snapshot()
        });
    }
    pub(super) fn serve_unreachable(&self) {
        *self.delivery.lock().unwrap() = Err("unreachable");
    }
}
pub(super) fn empty_snapshot() -> DeliverySnapshot {
    DeliverySnapshot {
        skills: Vec::new(),
        proposals_awaiting: 0,
        notices: Vec::new(),
        staleness_window_ms: 604_800_000,
        link_status: LinkStatus::Active,
        declined: Vec::new(),
    }
}
pub(super) fn delivered_mcp(skill_id: &str, name: &str, v: &Version) -> DeliverySkill {
    DeliverySkill {
        skill_id: skill_id.into(),
        name: name.into(),
        kind: "mcp".into(),
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
            Err(_) => Err(PlaneError::Unreachable("network down".into())),
        }
    }
    fn report_applied(&self, _ws: &str, applied: &[AppliedSkillReport]) -> Result<(), PlaneError> {
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
        self.log.lock().unwrap().push("report".into());
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct FakeDirectory {
    pub(super) skills: Vec<WireSkillIndexEntry>,
    pub(super) channels: Vec<WireChannelEntry>,
}
pub(super) fn mcp_catalog_entry(skill_id: &str, name: &str, v: &Version) -> WireSkillIndexEntry {
    WireSkillIndexEntry {
        skill_id: skill_id.into(),
        name: name.into(),
        kind: "mcp".into(),
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
        Err(ClientError::Plane("no me in this fake".into()))
    }
    fn channels_index(&self, _ws: &str) -> Result<WireChannelIndex, ClientError> {
        Ok(WireChannelIndex {
            channels: self.channels.clone(),
        })
    }
    fn skills_index(&self, _ws: &str) -> Result<WireSkillIndex, ClientError> {
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
    fn protect_skill(&self, _ws: &str, _s: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!()
    }
    fn protect_channel(&self, _ws: &str, _c: &str, _l: &str) -> Result<(), ClientError> {
        unreachable!()
    }
}
pub(super) struct NoContribute;
impl crate::plane::ContributeSource for NoContribute {
    fn publish(
        &self,
        _b: topos_types::requests::PublishRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!()
    }
    fn propose(
        &self,
        _b: topos_types::requests::ProposeRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!()
    }
    fn revert(
        &self,
        _b: topos_types::requests::RevertRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!()
    }
    fn review(
        &self,
        _b: topos_types::requests::ReviewRequest,
    ) -> Result<crate::plane::WriteReceipt, ClientError> {
        unreachable!()
    }
}
pub(super) struct NoGovernance;
impl crate::plane::GovernanceSource for NoGovernance {
    fn invite(
        &self,
        _w: &str,
        _b: topos_types::requests::InvitationRequest,
    ) -> Result<topos_types::requests::InvitationData, ClientError> {
        unreachable!()
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
pub(super) fn sweep(ctx: &Ctx<'_>, plane: &FakePlane, dir: &FakeDirectory) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap()
}

// =================================================================================================
// Synthetic descriptors — the six real dialects, EVERY surface Home-rooted so the engine tests are
// hermetic whatever `$CLAUDE_CONFIG_DIR`/`$CODEX_HOME`/`$HERMES_HOME` say on the dev machine.
// =================================================================================================

pub(super) static SYNTHETIC: &[KnownHarness] = &[
    registry::home_rooted_mcp_row(
        "claude-code",
        "Claude Code",
        ".claude/skills/topos-mcp",
        McpDialect::ClaudePluginDir,
        Some((".mcp.json", McpDialect::ClaudeProjectJson)),
        "reload claude",
    ),
    registry::home_rooted_mcp_row(
        "codex",
        "Codex",
        ".codex/config.toml",
        McpDialect::CodexToml,
        Some((".codex/config.toml", McpDialect::CodexToml)),
        "restart codex",
    ),
    registry::home_rooted_mcp_row(
        "cursor",
        "Cursor",
        ".cursor/mcp.json",
        McpDialect::CursorJson,
        Some((".cursor/mcp.json", McpDialect::CursorJson)),
        "restart cursor",
    ),
    registry::home_rooted_mcp_row(
        "opencode",
        "OpenCode",
        ".opencode/opencode.json",
        McpDialect::OpencodeJson,
        Some((".opencode/opencode.json", McpDialect::OpencodeJson)),
        "restart opencode",
    ),
    registry::home_rooted_mcp_row(
        "openclaw",
        "OpenClaw",
        ".openclaw/openclaw.json",
        McpDialect::OpenclawJson,
        None,
        "picked up automatically",
    ),
    registry::home_rooted_mcp_row(
        "hermes-agent",
        "Hermes Agent",
        ".hermes/config.yaml",
        McpDialect::HermesYaml,
        None,
        "/reload-mcp",
    ),
];

/// The synthetic table as the engine takes it — the same `&[&KnownHarness]` view the real
/// [`mcp_harnesses`](topos_harness::mcp::descriptor::mcp_harnesses) hands production.
pub(super) fn synthetic() -> Vec<&'static KnownHarness> {
    SYNTHETIC.iter().collect()
}

pub(super) fn all_slugs() -> BTreeSet<String> {
    synthetic().iter().map(|h| h.slug.to_owned()).collect()
}

pub(super) fn person_io<'a>(fs: &'a RealFs, layout: &'a Layout, home: &Path) -> ScopeIo<'a> {
    ScopeIo {
        fs,
        layout,
        home: home.to_path_buf(),
        project_root: None,
    }
}

pub(super) fn demand(
    bundle_id: &str,
    name: &str,
    ws: Option<&str>,
    server: &str,
) -> DemandedBundle {
    DemandedBundle {
        bundle_id: bundle_id.to_owned(),
        name: name.to_owned(),
        workspace_slug: ws.map(str::to_owned),
        version_id: "v1".to_owned(),
        server_json: server.as_bytes().to_vec(),
        reach: None,
    }
}

/// Plan a scope's demanded rows onto the SYNTHETIC harness table — the production seam
/// ([`DemandedBundle::planned`]) with the tests' own rows, so a converge here reads its demand
/// off plans exactly as the sweep does.
pub(super) fn plan(io: &ScopeIo<'_>, rows: Vec<DemandedBundle>) -> Vec<McpDemand> {
    rows.into_iter()
        .map(|r| r.planned(io, &synthetic(), &all_slugs()))
        .collect()
}

pub(super) fn no_hold() -> HashSet<String> {
    HashSet::new()
}

/// The state one bundle reports for a slug (panics when absent — the assertion names it).
pub(super) fn state_of<'a>(
    out: &'a mcp_engine::ConvergeOutcome,
    bundle: &str,
    slug: &str,
) -> &'a topos_types::results::McpAgentState {
    out.bundles
        .iter()
        .find(|b| b.bundle_id == bundle)
        .unwrap_or_else(|| panic!("bundle {bundle} reported: {out:?}"))
        .states
        .iter()
        .find(|s| s.agent == slug)
        .unwrap_or_else(|| panic!("state for {slug}: {out:?}"))
}

/// The wave-1 config files whose REAL user surfaces are Home-rooted (hermetic under a fake
/// `$HOME`) — the dest narrowing every machine-scope fixture rides.
pub(super) const SAFE: &str = "dest = [\"~/.cursor/mcp.json\", \"~/.openclaw/openclaw.json\"]";

/// The same two files spelled for a CHANNEL row, whose MCP members are narrowed by `mcp_dest`
/// alone — its `dest` names folders for the skill members.
pub(super) const SAFE_CHANNEL: &str =
    "mcp_dest = [\"~/.cursor/mcp.json\", \"~/.openclaw/openclaw.json\"]";

/// Seed the fake home so cursor + openclaw detect (their detect dirs exist).
pub(super) fn seed_harness_dirs(home: &Path) {
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::create_dir_all(home.join(".openclaw")).unwrap();
}
