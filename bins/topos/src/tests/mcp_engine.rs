//! The MCP placement engine + ownership ledger, end to end: the per-scope converge over a fake
//! HOME (synthetic descriptors — every surface Home-rooted, so no test can touch a real machine's
//! config), and the reconcile integration over the same fakes the manifest suite drives.
//!
//! The trust spine under test: entries land EXACTLY per dialect and byte-identical to the pure
//! drivers' rendering; a hand-edited entry is drift — never overwritten, never removed, disclosed;
//! a foreign occupant is never touched or claimed; removal converges only what the ledger proves
//! ours; the intent journal makes every config write crash-recoverable; a project surface passes
//! the containment rail; capability and narrowing filters withhold placement honestly; and the
//! whole loop — demand → store → config → applied report → offline cache — carries the per-agent
//! states.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use topos_core::digest::{self, FileMode, ManifestEntry};
use topos_core::identity::Commit;
use topos_harness::mcp::{
    self, AuthHint, McpDialect, McpEntry, McpHarness, McpSurface, SurfaceRoot, plugin_dir,
};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireMe, WireProposalIndex, WireReach,
    WireSkillIndex, WireSkillIndexEntry, WireSkillLog,
};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FaultFs, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::mcp_engine::{self, McpDemand, ScopeIo};
use crate::mcp_ledger;
use crate::plane::{
    AppliedSkillReport, DeliverySkill, DeliverySnapshot, DeliverySource, DirectorySource,
    FetchedFile, FetchedVersion, InertFollow, InertPlane, KnownCurrent, LinkStatus, PlaneError,
    PlaneSource, PointerFetch,
};
use crate::sessions::{self, SESSION_ACTIVE, Session};
use crate::sidecar::Layout;
use crate::{ops, sync_status};

const WS: &str = "w_eng";
const HOST: &str = "acme.test";
const WS_NAME: &str = "eng";

// =================================================================================================
// The rig (the manifest_reconcile idioms, over a fake HOME).
// =================================================================================================

struct Scratch(PathBuf);
impl Scratch {
    fn new(tag: &str) -> Self {
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
    fn ctx_at<'a>(&'a self, cwd: Option<&Path>) -> Ctx<'a> {
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
                cwd: cwd.map(Path::to_path_buf),
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
    fn write_global(&self, body: &str) {
        let home = self.layout().home().to_path_buf();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(crate::manifest::MANIFEST_FILE), body).unwrap();
    }
}

// A version whose bytes reproduce a REAL commit id (the engine re-verifies on apply).
struct Version {
    id: [u8; 32],
    digest: [u8; 32],
    fetched: FetchedVersion,
}
fn mk_version(files: &[(&str, &[u8])]) -> Version {
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
fn server_json(url: &str) -> String {
    format!(
        "{{\"name\":\"io.test/x\",\"description\":\"A test server.\",\"version\":\"1.0.0\",\
         \"remotes\":[{{\"type\":\"streamable-http\",\"url\":\"{url}\",\
         \"headers\":[{{\"name\":\"X-Team\",\"value\":\"eng\"}}]}}]}}"
    )
}

type CallLog = Arc<Mutex<Vec<String>>>;
type ReportedRow = (String, String, Vec<topos_types::results::McpAgentState>);

#[derive(Clone)]
struct FakePlane {
    delivery: Arc<Mutex<Result<DeliverySnapshot, &'static str>>>,
    versions: BTreeMap<(String, String), FetchedVersion>,
    reported: Arc<Mutex<Vec<ReportedRow>>>,
    log: CallLog,
}
impl FakePlane {
    fn new() -> Self {
        Self {
            delivery: Arc::new(Mutex::new(Ok(empty_snapshot()))),
            versions: BTreeMap::new(),
            reported: Arc::new(Mutex::new(Vec::new())),
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }
    fn with_version(mut self, skill: &str, v: &Version) -> Self {
        self.versions.insert(
            (skill.to_owned(), topos_core::digest::to_hex(&v.id)),
            v.fetched.clone(),
        );
        self
    }
    fn serves(&self, skills: Vec<DeliverySkill>) {
        *self.delivery.lock().unwrap() = Ok(DeliverySnapshot {
            skills,
            ..empty_snapshot()
        });
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
        declined: Vec::new(),
    }
}
fn delivered_mcp(skill_id: &str, name: &str, v: &Version) -> DeliverySkill {
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
struct FakeDirectory {
    skills: Vec<WireSkillIndexEntry>,
    channels: Vec<WireChannelEntry>,
}
fn mcp_catalog_entry(skill_id: &str, name: &str, v: &Version) -> WireSkillIndexEntry {
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
        Ok(())
    }
}
struct NoContribute;
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
struct NoGovernance;
impl crate::plane::GovernanceSource for NoGovernance {
    fn invite(
        &self,
        _w: &str,
        _b: topos_types::requests::InvitationRequest,
    ) -> Result<topos_types::requests::InvitationData, ClientError> {
        unreachable!()
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
// Synthetic descriptors — the six real dialects, EVERY surface Home-rooted so the engine tests are
// hermetic whatever `$CLAUDE_CONFIG_DIR`/`$CODEX_HOME`/`$HERMES_HOME` say on the dev machine.
// =================================================================================================

static SYNTHETIC: &[McpHarness] = &[
    McpHarness {
        slug: "claude-code",
        display_name: "Claude Code",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".claude/skills/topos-mcp",
            dialect: McpDialect::ClaudePluginDir,
        }),
        project_surface: Some((".mcp.json", McpDialect::ClaudeProjectJson)),
        oauth_capable: true,
        reload_note: "reload claude",
    },
    McpHarness {
        slug: "codex",
        display_name: "Codex",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".codex/config.toml",
            dialect: McpDialect::CodexToml,
        }),
        project_surface: Some((".codex/config.toml", McpDialect::CodexToml)),
        oauth_capable: true,
        reload_note: "restart codex",
    },
    McpHarness {
        slug: "cursor",
        display_name: "Cursor",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".cursor/mcp.json",
            dialect: McpDialect::CursorJson,
        }),
        project_surface: Some((".cursor/mcp.json", McpDialect::CursorJson)),
        oauth_capable: true,
        reload_note: "restart cursor",
    },
    McpHarness {
        slug: "opencode",
        display_name: "OpenCode",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".opencode/opencode.json",
            dialect: McpDialect::OpencodeJson,
        }),
        project_surface: Some((".opencode/opencode.json", McpDialect::OpencodeJson)),
        oauth_capable: true,
        reload_note: "restart opencode",
    },
    McpHarness {
        slug: "openclaw",
        display_name: "OpenClaw",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".openclaw/openclaw.json",
            dialect: McpDialect::OpenclawJson,
        }),
        project_surface: None,
        oauth_capable: true,
        reload_note: "picked up automatically",
    },
    McpHarness {
        slug: "hermes-agent",
        display_name: "Hermes Agent",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".hermes/config.yaml",
            dialect: McpDialect::HermesYaml,
        }),
        project_surface: None,
        oauth_capable: true,
        reload_note: "/reload-mcp",
    },
];

fn all_slugs() -> BTreeSet<String> {
    SYNTHETIC.iter().map(|h| h.slug.to_owned()).collect()
}

fn person_io<'a>(fs: &'a RealFs, layout: &'a Layout, home: &Path) -> ScopeIo<'a> {
    ScopeIo {
        fs,
        layout,
        home: home.to_path_buf(),
        project_root: None,
    }
}

fn demand(bundle_id: &str, name: &str, ws: Option<&str>, server: &str) -> McpDemand {
    McpDemand {
        bundle_id: bundle_id.to_owned(),
        name: name.to_owned(),
        workspace_slug: ws.map(str::to_owned),
        version_id: "v1".to_owned(),
        server_json: server.as_bytes().to_vec(),
        harness_filter: Vec::new(),
    }
}

fn no_hold() -> HashSet<String> {
    HashSet::new()
}

/// The state one bundle reports for a slug (panics when absent — the assertion names it).
fn state_of<'a>(
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

// =================================================================================================
// The engine, over the synthetic six.
// =================================================================================================

#[test]
fn converge_places_into_all_six_dialects_byte_identical_to_the_drivers() {
    let home = Scratch::new("six");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let d = demand(
        "s_linear",
        "linear",
        Some("eng"),
        &server_json("https://mcp.example/linear"),
    );
    let out = mcp_engine::converge(
        &person_io(&fs, &layout, &home.0),
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    // Every file dialect's bytes are EXACTLY what the pure driver renders from scratch — the
    // engine adds custody, never a dialect of its own.
    let entry = McpEntry {
        key: "topos-eng-linear".into(),
        url: "https://mcp.example/linear".into(),
        headers: vec![("X-Team".into(), "eng".into())],
        auth: AuthHint::Unknown,
    };
    for (suffix, dialect) in [
        (".codex/config.toml", McpDialect::CodexToml),
        (".cursor/mcp.json", McpDialect::CursorJson),
        (".opencode/opencode.json", McpDialect::OpencodeJson),
        (".openclaw/openclaw.json", McpDialect::OpenclawJson),
        (".hermes/config.yaml", McpDialect::HermesYaml),
    ] {
        let got = std::fs::read(home.0.join(suffix)).unwrap_or_else(|e| panic!("{suffix}: {e}"));
        let expect = match mcp::apply(
            dialect,
            None,
            std::slice::from_ref(&entry),
            &BTreeMap::new(),
        )
        .plan
        {
            mcp::EditPlan::Write(bytes) => bytes,
            other => panic!("{dialect:?}: {other:?}"),
        };
        assert_eq!(got, expect, "{suffix} differs from the driver's rendering");
    }
    // The plugin dir: both files, the .mcp.json byte-identical to the pure renderer.
    let plug = home.0.join(".claude/skills/topos-mcp");
    let rendered = plugin_dir::render_plugin_dir(std::slice::from_ref(&entry));
    assert_eq!(
        std::fs::read(plug.join(".claude-plugin/plugin.json")).unwrap(),
        rendered[0].1
    );
    assert_eq!(
        std::fs::read(plug.join(".mcp.json")).unwrap(),
        rendered[1].1
    );

    // Six states, all current, each carrying its reload note (a fresh placement).
    for h in SYNTHETIC {
        let st = state_of(&out, "s_linear", h.slug);
        assert_eq!(st.state, "current", "{}", h.slug);
        assert_eq!(st.note.as_deref(), Some(h.reload_note), "{}", h.slug);
    }
    // The ledger: one key, six entries, every fingerprint matching what the file provably holds.
    let ledger = mcp_ledger::read(&fs, &layout).unwrap();
    assert_eq!(ledger.keys["s_linear"], "topos-eng-linear");
    assert_eq!(ledger.entries.len(), 6);
    assert!(ledger.pending.is_empty());
    for (k, e) in &ledger.entries {
        let slug = k.split('/').next().unwrap();
        let h = SYNTHETIC.iter().find(|h| h.slug == slug).unwrap();
        let dialect = h.user_surface.unwrap().dialect;
        let observed = mcp::observe(dialect, std::fs::read(&e.file).ok().as_deref());
        assert_eq!(
            observed.entries.get("topos-eng-linear"),
            Some(&e.fingerprint),
            "{k}: ledger fingerprint must match the file"
        );
        assert!(e.owns_file, "{k}: a file we created is wholly ours");
    }

    // Idempotent: a second converge leaves every byte and reports Current (no reload note).
    let before: Vec<Vec<u8>> = SYNTHETIC
        .iter()
        .map(|h| std::fs::read(mcp::descriptor::user_surface_path(h, &home.0).unwrap()).ok())
        .map(Option::unwrap_or_default)
        .collect();
    let out2 = mcp_engine::converge(
        &person_io(&fs, &layout, &home.0),
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    for h in SYNTHETIC {
        let st = state_of(&out2, "s_linear", h.slug);
        assert_eq!(
            (st.state.as_str(), st.note.as_deref()),
            ("current", None),
            "{}",
            h.slug
        );
    }
    let after: Vec<Vec<u8>> = SYNTHETIC
        .iter()
        .map(|h| std::fs::read(mcp::descriptor::user_surface_path(h, &home.0).unwrap()).ok())
        .map(Option::unwrap_or_default)
        .collect();
    assert_eq!(before, after, "the second converge moved bytes");
}

#[test]
fn removal_converges_everywhere_and_deletes_only_wholly_owned_files() {
    let home = Scratch::new("removal");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    // Cursor's config PRE-EXISTS with a user server — never ours to delete. The rest are born
    // from this converge (wholly ours).
    let cursor = home.0.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(
        &cursor,
        b"{\n  \"mcpServers\": {\n    \"mine\": { \"url\": \"https://user.example\" }\n  }\n}\n",
    )
    .unwrap();
    let d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    let out = mcp_engine::converge(
        &person_io(&fs, &layout, &home.0),
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    // Drop the demand: every entry leaves; the wholly-owned files are DELETED, the user's cursor
    // file keeps its own server minus ours; the plugin dir is pruned whole.
    let out = mcp_engine::converge(
        &person_io(&fs, &layout, &home.0),
        &[],
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(out.removed.len(), 6, "{out:?}");
    assert!(
        !home.0.join(".codex/config.toml").exists(),
        "wholly-owned codex file deleted"
    );
    assert!(!home.0.join(".opencode/opencode.json").exists());
    assert!(!home.0.join(".openclaw/openclaw.json").exists());
    assert!(!home.0.join(".hermes/config.yaml").exists());
    assert!(
        !home.0.join(".claude/skills/topos-mcp").exists(),
        "plugin dir pruned"
    );
    let kept = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        kept.contains("\"mine\""),
        "the user's server survives: {kept}"
    );
    assert!(!kept.contains("topos-eng-alpha"), "ours is gone: {kept}");
    // The ledger: no entries, the key retired — and NEVER reusable by another bundle.
    let mut ledger = mcp_ledger::read(&fs, &layout).unwrap();
    assert!(ledger.entries.is_empty());
    assert_eq!(ledger.retired["topos-eng-alpha"], "s_a");
    assert_eq!(
        ledger.mint_key("s_other", "alpha", Some("eng")),
        "topos-eng-alpha-2"
    );
}

#[test]
fn a_hand_edited_entry_is_drift_never_clobbered_and_survives_removal_disclosed() {
    let home = Scratch::new("drift");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    let cursor_only = |d: &McpDemand| {
        let mut d = d.clone();
        d.harness_filter = vec!["cursor".into()];
        d
    };
    let io = person_io(&fs, &layout, &home.0);
    mcp_engine::converge(
        &io,
        &[cursor_only(&d)],
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    // The user edits OUR entry by hand.
    let cursor = home.0.join(".cursor/mcp.json");
    let edited = std::fs::read_to_string(&cursor)
        .unwrap()
        .replace("https://mcp.example/a", "https://my-fork.example");
    std::fs::write(&cursor, &edited).unwrap();

    // The next converge reads it as DRIFT: untouched, reported, the ledger keeps the OLD
    // fingerprint so drift survives re-runs.
    let out = mcp_engine::converge(
        &io,
        &[cursor_only(&d)],
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(state_of(&out, "s_a", "cursor").state, "drifted");
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        edited,
        "bytes untouched"
    );

    // Removal LEAVES the drifted entry and disclosed it (never destroys a hand edit).
    let out = mcp_engine::converge(&io, &[], SYNTHETIC, &all_slugs(), &no_hold(), true);
    assert!(
        out.removed.iter().any(|r| r.state.state == "drifted"),
        "{out:?}"
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        edited,
        "removal left the edit"
    );
}

#[test]
fn a_foreign_topos_prefixed_entry_is_never_touched_or_claimed() {
    let home = Scratch::new("foreign");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    // Someone else's `topos-…` entry occupies the very key we would mint.
    let cursor = home.0.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    let foreign = "{\n  \"mcpServers\": {\n    \"topos-eng-alpha\": { \"url\": \"https://foreign.example\" }\n  }\n}\n";
    std::fs::write(&cursor, foreign).unwrap();
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.harness_filter = vec!["cursor".into()];
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(&io, &[d], SYNTHETIC, &all_slugs(), &no_hold(), true);
    assert_eq!(state_of(&out, "s_a", "cursor").state, "conflicting");
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        foreign,
        "foreign bytes untouched"
    );
    let ledger = mcp_ledger::read(&fs, &layout).unwrap();
    assert!(
        !ledger.entries.contains_key("cursor/topos-eng-alpha"),
        "a foreign entry never enters the ledger"
    );
}

#[test]
fn the_oauth_capability_filter_withholds_unknown_and_oauth_servers() {
    let home = Scratch::new("oauth");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    static NO_OAUTH: &[McpHarness] = &[McpHarness {
        slug: "plainbot",
        display_name: "PlainBot",
        user_surface: Some(McpSurface {
            root: SurfaceRoot::Home,
            suffix: ".plainbot/mcp.json",
            dialect: McpDialect::CursorJson,
        }),
        project_surface: None,
        oauth_capable: false,
        reload_note: "restart plainbot",
    }];
    let detected: BTreeSet<String> = ["plainbot".to_owned()].into();
    let io = person_io(&fs, &layout, &home.0);
    // Unknown auth (no hint) and explicit oauth are both withheld; explicit none places.
    let unknown = demand("s_u", "u", Some("eng"), &server_json("https://u.example"));
    let oauth = {
        let mut d = demand("s_o", "o", Some("eng"), "");
        d.server_json = br#"{"name":"io.test/o","description":"O.","version":"1.0.0","remotes":[{"type":"streamable-http","url":"https://o.example"}],"_meta":{"sh.topos/auth":"oauth"}}"#
            .to_vec();
        d
    };
    let none = {
        let mut d = demand("s_n", "n", Some("eng"), "");
        d.server_json = br#"{"name":"io.test/n","description":"N.","version":"1.0.0","remotes":[{"type":"streamable-http","url":"https://n.example"}],"_meta":{"sh.topos/auth":"none"}}"#
            .to_vec();
        d
    };
    let out = mcp_engine::converge(
        &io,
        &[unknown, oauth, none],
        NO_OAUTH,
        &detected,
        &no_hold(),
        true,
    );
    assert_eq!(state_of(&out, "s_u", "plainbot").state, "not-supported");
    assert_eq!(state_of(&out, "s_o", "plainbot").state, "not-supported");
    assert_eq!(state_of(&out, "s_n", "plainbot").state, "current");
    let text = std::fs::read_to_string(home.0.join(".plainbot/mcp.json")).unwrap();
    assert!(
        text.contains("https://n.example") && !text.contains("o.example"),
        "{text}"
    );
}

#[test]
fn a_suspect_header_fails_the_demand_closed_with_a_warning() {
    let home = Scratch::new("gate");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand("s_a", "alpha", Some("eng"), "");
    d.server_json = br#"{"name":"io.test/a","description":"A.","version":"1.0.0","remotes":[{"type":"streamable-http","url":"https://a.example","headers":[{"name":"Authorization","isSecret":true}]}]}"#.to_vec();
    d.harness_filter = vec!["cursor".into()];
    let out = mcp_engine::converge(&io, &[d], SYNTHETIC, &all_slugs(), &no_hold(), true);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("MCP_SECRET_REFUSED")),
        "{:?}",
        out.warnings
    );
    assert_eq!(state_of(&out, "s_a", "cursor").state, "unprovable");
    assert!(
        !home.0.join(".cursor/mcp.json").exists(),
        "nothing was placed"
    );
}

/// F1's byte-loss belt, per dialect: a NON-prefixed user entry added to a file topos CREATED is
/// invisible to the drivers' states — the last-entry removal must still never delete the file
/// over it.
#[test]
fn a_user_entry_added_to_a_topos_created_file_survives_last_entry_removal() {
    type Inject = Box<dyn Fn(&str) -> String>;
    let json_inject = |path: &[&str]| -> Inject {
        let path: Vec<String> = path.iter().map(|s| (*s).to_owned()).collect();
        Box::new(move |text: &str| {
            let mut root: serde_json::Value = serde_json::from_str(text).unwrap();
            let mut slot = &mut root;
            for key in &path {
                slot = slot.get_mut(key).unwrap();
            }
            slot.as_object_mut().unwrap().insert(
                "mine".to_owned(),
                serde_json::json!({ "url": "https://user.example" }),
            );
            let mut out = serde_json::to_string_pretty(&root).unwrap();
            out.push('\n');
            out
        })
    };
    let cases: Vec<(&str, &str, &str, Inject)> = vec![
        (
            "cursor",
            ".cursor/mcp.json",
            "mine",
            json_inject(&["mcpServers"]),
        ),
        (
            "opencode",
            ".opencode/opencode.json",
            "mine",
            json_inject(&["mcp"]),
        ),
        (
            "openclaw",
            ".openclaw/openclaw.json",
            "mine",
            json_inject(&["mcp", "servers"]),
        ),
        (
            "codex",
            ".codex/config.toml",
            "model",
            Box::new(|text: &str| format!("model = \"o5\"\n{text}")) as Inject,
        ),
        (
            "hermes-agent",
            ".hermes/config.yaml",
            "their",
            Box::new(|text: &str| format!("{text}  their: {{url: \"https://user.example\"}}\n"))
                as Inject,
        ),
    ];
    for (slug, rel, user_marker, inject) in cases {
        let home = Scratch::new(&format!("keep-{slug}"));
        let fs = RealFs;
        let layout = Layout::new(&home.0.join(".topos"));
        let io = person_io(&fs, &layout, &home.0);
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.harness_filter = vec![slug.to_owned()];
        mcp_engine::converge(
            &io,
            std::slice::from_ref(&d),
            SYNTHETIC,
            &all_slugs(),
            &no_hold(),
            true,
        );
        let file = home.0.join(rel);
        let created = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{slug}: {e}"));
        // The USER adds a plain (non-topos) entry to the file topos created.
        std::fs::write(&file, inject(&created)).unwrap();

        // The demand drops: our entry leaves, the FILE STAYS with the user's content.
        let out = mcp_engine::converge(&io, &[], SYNTHETIC, &all_slugs(), &no_hold(), true);
        assert!(out.warnings.is_empty(), "{slug}: {:?}", out.warnings);
        let kept = std::fs::read_to_string(&file)
            .unwrap_or_else(|e| panic!("{slug}: the file was deleted over a user entry: {e}"));
        assert!(
            kept.contains(user_marker),
            "{slug}: the user's entry survives: {kept}"
        );
        assert!(
            !kept.contains("topos-eng-alpha"),
            "{slug}: ours is gone: {kept}"
        );
        let ledger = mcp_ledger::read(&fs, &layout).unwrap();
        assert!(!ledger.has_entries_for("s_a"), "{slug}");
    }
}

/// F1 belt two: the moment a converge OBSERVES user content in a file topos created, the
/// ledger's whole-file-ownership flag goes false — a later removal can never trust a flag the
/// file has outgrown.
#[test]
fn a_converge_that_sees_user_content_flips_owns_file_false() {
    let home = Scratch::new("flip");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.harness_filter = vec!["cursor".into()];
    mcp_engine::converge(
        &io,
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    let lk = mcp_ledger::placement_key("cursor", "topos-eng-alpha");
    assert!(
        mcp_ledger::read(&fs, &layout).unwrap().entries[&lk].owns_file,
        "created → wholly owned"
    );
    // The user adds a plain entry; the next converge (demand unchanged, a LEAVE) must record
    // that the file is no longer wholly ours.
    let cursor = home.0.join(".cursor/mcp.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cursor).unwrap()).unwrap();
    root["mcpServers"].as_object_mut().unwrap().insert(
        "mine".to_owned(),
        serde_json::json!({ "url": "https://user.example" }),
    );
    std::fs::write(&cursor, serde_json::to_string_pretty(&root).unwrap() + "\n").unwrap();
    let out = mcp_engine::converge(
        &io,
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(state_of(&out, "s_a", "cursor").state, "current");
    assert!(
        !mcp_ledger::read(&fs, &layout).unwrap().entries[&lk].owns_file,
        "the flag stops lying at the first sighting"
    );
}

/// F4: the engine resolves the SAME remote the publish gate approved — the first with
/// `type == "streamable-http"` AND a url, in one predicate. A url-less streamable remote ahead
/// of the real one must not fail (or redirect) the demand.
#[test]
fn the_engine_places_the_remote_the_gate_approved_not_the_first_typed_one() {
    let home = Scratch::new("remote-pick");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand("s_two", "two", Some("eng"), "");
    d.server_json = br#"{"name":"io.test/two","description":"Two remotes.","version":"1.0.0","remotes":[{"type":"streamable-http"},{"type":"streamable-http","url":"https://second.example/mcp"}]}"#.to_vec();
    d.harness_filter = vec!["cursor".into()];
    let out = mcp_engine::converge(&io, &[d], SYNTHETIC, &all_slugs(), &no_hold(), true);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(state_of(&out, "s_two", "cursor").state, "current");
    let text = std::fs::read_to_string(home.0.join(".cursor/mcp.json")).unwrap();
    assert!(text.contains("https://second.example/mcp"), "{text}");
}

#[test]
fn holds_and_targeted_runs_never_remove_standing_entries() {
    let home = Scratch::new("hold");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.harness_filter = vec!["cursor".into()];
    mcp_engine::converge(
        &io,
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    let cursor = home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();

    // Undemanded but HELD (its workspace was unreachable): byte-identical, ledger kept.
    let hold: HashSet<String> = ["s_a".to_owned()].into();
    mcp_engine::converge(&io, &[], SYNTHETIC, &all_slugs(), &hold, true);
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), placed);
    assert!(
        mcp_ledger::read(&fs, &layout)
            .unwrap()
            .has_entries_for("s_a")
    );

    // Undemanded on a run that may NOT remove (a targeted update): same freeze.
    mcp_engine::converge(&io, &[], SYNTHETIC, &all_slugs(), &no_hold(), false);
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), placed);
    assert!(
        mcp_ledger::read(&fs, &layout)
            .unwrap()
            .has_entries_for("s_a")
    );
}

#[test]
fn intent_journal_recovery_heals_both_crash_orders_through_the_engine() {
    let home = Scratch::new("recover");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.harness_filter = vec!["cursor".into()];
    mcp_engine::converge(
        &io,
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    let cursor = home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    let real_fp =
        mcp::observe(McpDialect::CursorJson, Some(placed.as_bytes())).entries["topos-eng-alpha"]
            .clone();

    // ORDER (b): the config write LANDED, the ledger promotion did not — on disk the ledger
    // still carries the old fingerprint plus the journaled intent. Without recovery the entry
    // would read as user drift forever.
    let mut ledger = mcp_ledger::read(&fs, &layout).unwrap();
    let lk = mcp_ledger::placement_key("cursor", "topos-eng-alpha");
    ledger.entries.get_mut(&lk).unwrap().fingerprint = "0".repeat(64);
    ledger.pending.insert(
        lk.clone(),
        mcp_ledger::PendingIntent {
            bundle_id: "s_a".into(),
            version_id: "v1".into(),
            file: cursor.display().to_string(),
            fingerprint: real_fp.clone(),
            owns_file: true,
        },
    );
    mcp_ledger::write(&fs, &layout, &ledger).unwrap();
    let out = mcp_engine::converge(
        &io,
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        (
            state_of(&out, "s_a", "cursor").state.as_str(),
            state_of(&out, "s_a", "cursor").note.as_deref()
        ),
        ("current", None),
        "recovery promoted the landed write instead of reading it as drift"
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        placed,
        "no rewrite"
    );
    let ledger = mcp_ledger::read(&fs, &layout).unwrap();
    assert!(ledger.pending.is_empty());
    assert_eq!(ledger.entries[&lk].fingerprint, real_fp);

    // ORDER (a): the intent was journaled, the config write never landed. Recovery drops the
    // intent; the standing entry (which matches the file) stays authoritative.
    let mut ledger = mcp_ledger::read(&fs, &layout).unwrap();
    ledger.pending.insert(
        lk.clone(),
        mcp_ledger::PendingIntent {
            bundle_id: "s_a".into(),
            version_id: "v2".into(),
            file: cursor.display().to_string(),
            fingerprint: "f".repeat(64),
            owns_file: true,
        },
    );
    mcp_ledger::write(&fs, &layout, &ledger).unwrap();
    let out = mcp_engine::converge(
        &io,
        std::slice::from_ref(&d),
        SYNTHETIC,
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(state_of(&out, "s_a", "cursor").state, "current");
    let ledger = mcp_ledger::read(&fs, &layout).unwrap();
    assert!(ledger.pending.is_empty());
    assert_eq!(ledger.entries[&lk].fingerprint, real_fp);
}

#[test]
fn a_fault_at_any_write_never_tears_state_and_the_next_converge_heals() {
    // The fault sweep through the fs seam: fail exactly one mutating op, at every op the converge
    // performs, and prove the invariant pair — nothing torn (the ledger stays decipherable), and
    // a clean re-run always ends fully placed with a file-matching ledger.
    let probe = {
        let home = Scratch::new("fault-probe");
        let layout = Layout::new(&home.0.join(".topos"));
        let fault = FaultFs::new(0);
        let io = ScopeIo {
            fs: &fault,
            layout: &layout,
            home: home.0.clone(),
            project_root: None,
        };
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.harness_filter = vec!["cursor".into()];
        mcp_engine::converge(&io, &[d], SYNTHETIC, &all_slugs(), &no_hold(), true);
        fault.ops_attempted()
    };
    assert!(probe > 0);
    for fail_at in 1..=probe {
        let home = Scratch::new(&format!("fault-{fail_at}"));
        let layout = Layout::new(&home.0.join(".topos"));
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.harness_filter = vec!["cursor".into()];
        {
            let fault = FaultFs::new(fail_at);
            let io = ScopeIo {
                fs: &fault,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            let _ = mcp_engine::converge(
                &io,
                std::slice::from_ref(&d),
                SYNTHETIC,
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        // The clean re-run heals whatever the fault interrupted.
        let fs = RealFs;
        let io = person_io(&fs, &layout, &home.0);
        let out = mcp_engine::converge(
            &io,
            std::slice::from_ref(&d),
            SYNTHETIC,
            &all_slugs(),
            &no_hold(),
            true,
        );
        assert_eq!(
            state_of(&out, "s_a", "cursor").state,
            "current",
            "fail_at={fail_at}: {out:?}"
        );
        let bytes = std::fs::read(home.0.join(".cursor/mcp.json")).unwrap();
        let observed = mcp::observe(McpDialect::CursorJson, Some(&bytes));
        let ledger = mcp_ledger::read(&fs, &layout).unwrap();
        assert!(ledger.pending.is_empty(), "fail_at={fail_at}");
        assert_eq!(
            observed.entries.get("topos-eng-alpha"),
            Some(
                &ledger.entries[&mcp_ledger::placement_key("cursor", "topos-eng-alpha")]
                    .fingerprint
            ),
            "fail_at={fail_at}: the healed ledger matches the file"
        );
    }
}

// =================================================================================================
// The reconcile integration (the delivery loop; Home-rooted real descriptors only — cursor,
// openclaw — so no test can reach outside the fake home whatever the dev env sets. opencode's
// user surface hangs off `$XDG_CONFIG_HOME` the way claude-code's / codex's / hermes's hang off
// theirs, so person scope leaves it out; its coverage is the PROJECT test below, where every
// surface is checkout-relative and hermetic by construction).
// =================================================================================================

/// The wave-1 slugs whose REAL user surfaces are Home-rooted (hermetic under a fake `$HOME`).
const SAFE: &str = "harness = [\"cursor\", \"openclaw\"]";

/// Seed the fake home so cursor + openclaw detect (their detect dirs exist).
fn seed_harness_dirs(home: &Path) {
    std::fs::create_dir_all(home.join(".cursor")).unwrap();
    std::fs::create_dir_all(home.join(".openclaw")).unwrap();
}

#[test]
fn a_workspace_mcp_bundle_lands_in_configs_reports_harnesses_and_caches_kind() {
    let rig = Rig::new("deliver");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    plane.serves(vec![delivered_mcp("s_linear", "linear", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    // The feed delivers; `[defaults.mcp]` narrows to the hermetic slugs.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\n[defaults.mcp]\n{SAFE}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // The row landed store-only: lock custody exists, NO skill-dir placement anywhere.
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let sp = rig.layout().published(&sid);
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    assert_eq!(lock.base_commit, topos_core::digest::to_hex(&v.id));
    let map = crate::doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    assert!(
        map.placements.is_empty(),
        "an mcp bundle places no dirs: {map:?}"
    );
    assert!(!rig.work.0.join("skills/linear").exists());
    assert!(
        !rig.home.0.join(".agents").exists(),
        "no shared-dir copy either"
    );

    // The two hermetic configs hold the entry.
    for path in [
        rig.home.0.join(".cursor/mcp.json"),
        rig.home.0.join(".openclaw/openclaw.json"),
    ] {
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
        assert!(
            text.contains("topos-eng-linear") && text.contains("https://mcp.example/linear"),
            "{path:?}: {text}"
        );
    }

    // The receipt row carries the per-agent outcomes.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap();
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor", "openclaw"].into());
    assert!(
        row.harnesses.iter().all(|h| h.state == "current"),
        "{row:?}"
    );

    // The applied report carried the same states over the wire.
    let reported = plane.reported.lock().unwrap().clone();
    let (_, version, harnesses) = reported
        .iter()
        .find(|(id, ..)| id == "s_linear")
        .unwrap_or_else(|| panic!("reported: {reported:?}"));
    assert_eq!(version, &topos_core::digest::to_hex(&v.id));
    assert_eq!(harnesses.len(), 2, "{harnesses:?}");

    // The offline cache carries the kind + the per-agent states.
    let cache = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    let ds = &cache.workspaces[WS].delivered["s_linear"];
    assert_eq!(ds.kind.as_deref(), Some("mcp"));
    assert_eq!(ds.harness_states.len(), 2, "{ds:?}");

    // And `list` answers the kind + the per-agent detail offline.
    let list = ops::list_with(
        &ctx,
        &ops::ListRequest {
            name: Some("linear".into()),
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let detail = list.data.detail.unwrap();
    assert_eq!(detail.kind.as_deref(), Some("mcp"));
    assert_eq!(detail.harnesses.len(), 2, "{detail:?}");
    assert!(detail.placements.is_empty());
}

#[test]
fn offline_sweeps_still_heal_configs_from_the_store() {
    let rig = Rig::new("offline");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/a").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_a", &v);
    plane.serves(vec![delivered_mcp("s_a", "alpha", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_a", "alpha", &v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\n[defaults.mcp]\n{SAFE}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists());

    // The network dies AND the entry is lost locally (the file deleted by hand): the next sweep
    // converges from the STORE's held bytes + the ledger — no dial needed.
    std::fs::remove_file(&cursor).unwrap();
    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    let text = std::fs::read_to_string(&cursor).unwrap_or_else(|e| panic!("not healed: {e}"));
    assert!(text.contains("https://mcp.example/a"), "{text}");
    // And the healed placement is disclosed on the row.
    let row = out.data.skills.iter().find(|s| s.skill == "alpha").unwrap();
    assert!(
        row.harnesses
            .iter()
            .any(|h| h.agent == "cursor" && h.state == "current"),
        "{row:?}"
    );
}

#[test]
fn a_channel_drop_removes_the_entries_everywhere() {
    let rig = Rig::new("chdrop");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/a").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_a", &v);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_a", "alpha", &v)],
        channels: vec![WireChannelEntry {
            name: "tools".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_a".into(),
                name: "alpha".into(),
            }],
        }],
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/tools\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("topos-eng-alpha")
    );

    // The channel stops carrying the bundle: the next sweep's removal convergence clears every
    // config entry (the channel still exists and expands — to nothing).
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_a", "alpha", &v)],
        channels: vec![WireChannelEntry {
            name: "tools".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: Vec::new(),
        }],
    };
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !cursor.exists()
            || !std::fs::read_to_string(&cursor)
                .unwrap()
                .contains("topos-eng-alpha"),
        "the entry left cursor's config"
    );
    assert!(
        out.disclosures.iter().any(|d| d.contains("MCP_REMOVED")),
        "{:?}",
        out.disclosures
    );
    let ledger = mcp_ledger::read(&rig.fs, &rig.layout()).unwrap();
    assert!(!ledger.has_entries_for("s_a"));
}

#[test]
fn a_project_row_lands_only_in_project_surfaces_and_openclaw_hermes_read_not_supported() {
    let rig = Rig::new("project");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    // claude-code + codex + opencode engage hermetically at PROJECT scope: their project surfaces
    // are checkout-relative. Seed codex's and opencode's project files so they engage without a
    // detect dir (opencode's sits at the checkout ROOT).
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("project-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".codex")).unwrap();
    std::fs::write(proj.0.join(".codex/config.toml"), b"").unwrap();
    std::fs::write(proj.0.join("opencode.json"), b"").unwrap();
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!("[bundles]\n\"{HOST}/{WS_NAME}/linear\" = \"*\"\n"),
    )
    .unwrap();
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);

    // The FOUR project surfaces (and nothing under the home).
    for rel in [
        ".mcp.json",
        ".codex/config.toml",
        ".cursor/mcp.json",
        "opencode.json",
    ] {
        let text =
            std::fs::read_to_string(proj.0.join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        assert!(text.contains("topos-eng-linear"), "{rel}: {text}");
    }
    assert!(
        !rig.home.0.join(".cursor/mcp.json").exists(),
        "person scope untouched"
    );

    // openclaw / hermes have no project-level config: withheld, honestly.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap();
    for slug in ["openclaw", "hermes-agent"] {
        let st = row
            .harnesses
            .iter()
            .find(|h| h.agent == slug)
            .unwrap_or_else(|| panic!("{slug} state: {row:?}"));
        assert_eq!(st.state, "not-supported", "{slug}");
        assert_eq!(
            st.note.as_deref(),
            Some("no project-level config"),
            "{slug}"
        );
    }
    // The ledger lives in the PROJECT store.
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(playout.mcp_ledger_path().exists());
    assert!(!rig.layout().mcp_ledger_path().exists());
}

#[test]
fn a_project_config_symlink_escaping_the_checkout_is_refused_and_disclosed() {
    let rig = Rig::new("escape");
    rig.seed_session();
    let outside = Scratch::new("escape-outside");
    let proj = Scratch::new("escape-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    // `.cursor` is a committed symlink aiming OUT of the checkout.
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".cursor")).unwrap();
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!("[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ version = \"*\", harness = [\"cursor\"] }}\n"),
    )
    .unwrap();
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("PLACEMENT_ESCAPES_PROJECT")),
        "{:?}",
        out.warnings
    );
    assert!(
        std::fs::read_dir(&outside.0).unwrap().next().is_none(),
        "nothing may land outside the checkout"
    );
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap();
    assert!(
        row.harnesses
            .iter()
            .any(|h| h.agent == "cursor" && h.state == "unprovable"),
        "{row:?}"
    );
}

#[test]
fn row_harness_narrowing_beats_defaults_and_unknown_slugs_warn_once() {
    let rig = Rig::new("narrow");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/a").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_a", &v);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_a", "alpha", &v)],
        channels: Vec::new(),
    };
    // The default says cursor+openclaw; the ROW narrows to cursor alone and adds a bogus slug.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/alpha\" = {{ version = \"*\", harness = [\"cursor\", \"notepad\"] }}\n\
         \n[defaults.mcp]\nharness = [\"cursor\", \"openclaw\"]\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(rig.home.0.join(".cursor/mcp.json").exists());
    assert!(
        !rig.home.0.join(".openclaw/openclaw.json").exists(),
        "the row narrows PAST the default"
    );
    let unknown: Vec<&String> = out
        .warnings
        .iter()
        .filter(|w| w.contains("MCP_HARNESS_UNKNOWN"))
        .collect();
    assert_eq!(
        unknown.len(),
        1,
        "one warning per unknown slug: {unknown:?}"
    );
}

/// F3: a local `kind = "mcp"` row's `server.json` is re-read from disk EVERY sweep — a
/// post-adopt edit smuggling a credential, an insecure endpoint, or a template must be refused
/// at the converge boundary with the typed code: nothing new placed, the standing entries left
/// as-is (removal only on demand-drop, never on a now-invalid source).
#[test]
fn a_tampered_local_row_is_held_with_the_typed_refusal_and_prior_entries_stay() {
    let rig = Rig::new("tamper");
    seed_harness_dirs(&rig.home.0);
    let dir = rig.home.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    let good = |url: &str, header_value: &str| {
        format!(
            "{{\"name\":\"io.test/w\",\"description\":\"W.\",\"version\":\"1.0.0\",\
             \"remotes\":[{{\"type\":\"streamable-http\",\"url\":\"{url}\",\
             \"headers\":[{{\"name\":\"X-R\",\"value\":\"{header_value}\"}}]}}]}}"
        )
    };
    std::fs::write(dir.join("server.json"), good("https://w.example/mcp", "eu")).unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = {{ kind = \"mcp\", harness = [\"cursor\"] }}\n",
        dir.display()
    ));
    let plane = FakePlane::new();
    let fdir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &fdir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    assert!(placed.contains("https://w.example/mcp"), "{placed}");

    // Each tamper: the sweep warns with the TYPED refusal code, the config file stays
    // byte-identical (the credential never reaches it), and the ledger keeps the entry.
    let token = format!("ghp_{}", "A1b2C3d4E5".repeat(4));
    for (tampered, code) in [
        (good("https://w.example/mcp", &token), "MCP_SECRET_REFUSED"),
        (good("http://w.example/mcp", "eu"), "MCP_INSECURE_URL"),
        (
            good("https://{tenant}.example/mcp", "eu"),
            "MCP_URL_TEMPLATE",
        ),
    ] {
        std::fs::write(dir.join("server.json"), &tampered).unwrap();
        let out = sweep(&ctx, &plane, &fdir);
        assert!(
            out.warnings.iter().any(|w| w.contains(code)),
            "{code}: {:?}",
            out.warnings
        );
        let now = std::fs::read_to_string(&cursor).unwrap();
        assert_eq!(now, placed, "{code}: the placed config never moves");
        assert!(!now.contains("ghp_"), "{code}: no credential lands");
        assert!(
            mcp_ledger::read(&rig.fs, &rig.layout())
                .unwrap()
                .has_entries_for("local:weather"),
            "{code}: the standing entry is held, not dropped"
        );
    }
}

#[test]
fn a_github_sourced_mcp_row_is_refused_with_the_typed_constraint() {
    let rig = Rig::new("ghmcp");
    rig.seed_session();
    let plane = FakePlane::new();
    let dir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    rig.write_global("[bundles]\n\"github.com/o/r/tool\" = { version = \"*\", kind = \"mcp\" }\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("MCP_GITHUB_UNSUPPORTED")),
        "{:?}",
        out.warnings
    );
}

/// F6: the SAME folder adopted in BOTH scopes must keep each scope's config key stable. The
/// reconcile resolves a local row's ledger identity against THE SCOPE'S OWN store — never the
/// other scope's — because add-time minted the key under the scope's tracked id, and a
/// cross-scope answer would retire that key and re-mint `-2` (the one path an entry name could
/// move, orphaning any OAuth token filed under it).
#[test]
fn dual_scope_adoption_keeps_each_scopes_config_key_stable() {
    let rig = Rig::new("dual");
    seed_harness_dirs(&rig.home.0);
    rig.write_global("[bundles]\n");
    let proj = Scratch::new("dual-proj");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    let dir = proj.0.join("weather");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        "{\"name\":\"io.test/w\",\"description\":\"W.\",\"version\":\"1.0.0\",\
         \"remotes\":[{\"type\":\"streamable-http\",\"url\":\"https://w.example/mcp\"}]}",
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    // Adopt at PROJECT scope (the checkout's store tracks the dir under the project id), then
    // the SAME folder with `-g` (the home store tracks it under its own id).
    let ops::AddMcpOutcome::Applied(_) =
        ops::add_mcp(&ctx, None, dir.to_str().unwrap(), false, false).expect("project adopt")
    else {
        panic!("a local folder applies immediately");
    };
    let ops::AddMcpOutcome::Applied(_) =
        ops::add_mcp(&ctx, None, dir.to_str().unwrap(), true, false).expect("global adopt")
    else {
        panic!("a local folder applies immediately");
    };
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the project adopt minted the checkout's store");
    let before = mcp_ledger::read(&rig.fs, &playout).unwrap();
    assert_eq!(before.keys.len(), 1, "{before:?}");
    let (proj_bundle, proj_key) = before.keys.iter().next().unwrap();
    let (proj_bundle, proj_key) = (proj_bundle.clone(), proj_key.clone());
    let cursor = proj.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    assert!(placed.contains(&proj_key), "{placed}");

    // The sweep must resolve the SAME identity the add minted the key under: no retire, no
    // `-2` re-mint, the config entry's name never moves.
    let plane = FakePlane::new();
    let fdir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    sweep(&ctx, &plane, &fdir);
    let after = mcp_ledger::read(&rig.fs, &playout).unwrap();
    assert_eq!(
        after.keys.get(&proj_bundle),
        Some(&proj_key),
        "the project scope's key must survive the sweep: {after:?}"
    );
    assert!(after.retired.is_empty(), "nothing was retired: {after:?}");
    let now = std::fs::read_to_string(&cursor).unwrap();
    assert!(now.contains(&proj_key), "{now}");
    assert!(
        !now.contains(&format!("{proj_key}-2")),
        "the entry name never moves: {now}"
    );
}

#[test]
fn remove_of_an_mcp_row_converges_inline_and_the_receipt_names_the_removals() {
    let rig = Rig::new("rm");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/a").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_a", &v);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_a", "alpha", &v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/alpha\" = {{ version = \"*\", {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("topos-eng-alpha")
    );

    // `remove -g` drops the row AND converges the scope inline — the receipt names the per-agent
    // removals; nothing waits for the next sweep.
    let outcome = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/alpha")],
        None,
        true,
    )
    .unwrap();
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("applies under --yes");
    };
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("server entry removed") && note.contains("cursor"),
        "{note}"
    );
    assert!(
        !cursor.exists()
            || !std::fs::read_to_string(&cursor)
                .unwrap()
                .contains("topos-eng-alpha"),
        "the entry is gone now, not at the next sweep"
    );
    let ledger = mcp_ledger::read(&rig.fs, &rig.layout()).unwrap();
    assert!(!ledger.has_entries_for("s_a"));
    assert_eq!(
        ledger.retired.get("topos-eng-alpha").map(String::as_str),
        Some("s_a")
    );
}

// =================================================================================================
// The durable kind marker: classification survives a lost ledger, fails closed without evidence,
// a targeted go-back converges configs, and the applied report never claims an empty-map skill.
// =================================================================================================

/// Deliver ONE mcp bundle at machine scope over the hermetic slugs, returning the plane + dir the
/// follow-up sweeps and targeted verbs reuse.
fn deliver_linear(rig: &Rig, v: &Version) -> (FakePlane, FakeDirectory) {
    let plane = FakePlane::new().with_version("s_linear", v);
    plane.serves(vec![delivered_mcp("s_linear", "linear", v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\n[defaults.mcp]\n{SAFE}\n"
    ));
    (plane, dir)
}

/// ITEM PAIR (kind fails durable): the store + empty map stand, the LEDGER is gone — a targeted
/// go-back must still classify the record as config-placed and materialize NO skill dirs. Before
/// the fix, classification hung on the ledger alone: its loss let the skill planner run and
/// `server.json` landed in skill dirs.
#[test]
fn a_lost_ledger_never_lets_a_targeted_go_back_materialize_skill_dirs() {
    let rig = Rig::new("lost-ledger");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    // The failure shape: the ledger is gone, and so is the delivery cache — the MARKER alone must
    // answer.
    std::fs::remove_file(rig.layout().mcp_ledger_path()).unwrap();
    std::fs::remove_file(rig.layout().sync_status_path()).unwrap();

    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the marker classifies the record; the go-back applies store-only");
    assert_eq!(out.data.skills.len(), 1);

    // NOTHING materialized into skill dirs, and the map still records zero placements.
    assert!(
        !rig.work.0.join("skills").exists(),
        "no harness skill dir was created"
    );
    assert!(!rig.home.0.join(".agents").exists(), "no shared-dir copy");
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert!(map.placements.is_empty(), "{map:?}");
}

/// ITEM PAIR (fail closed): with the map EMPTY and every kind source gone — marker, cache, ledger
/// — the targeted verb REFUSES with the typed placement error instead of guessing "skill" and
/// materializing. Before the fix the guess ran the planner.
#[test]
fn an_empty_map_with_no_kind_evidence_fails_closed_on_targeted_verbs() {
    let rig = Rig::new("no-evidence");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    std::fs::remove_file(rig.layout().mcp_ledger_path()).unwrap();
    std::fs::remove_file(rig.layout().sync_status_path()).unwrap();
    std::fs::remove_file(rig.layout().published(&sid).kind).unwrap();

    let Err(err) = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    ) else {
        panic!("kind indeterminate over an empty map must refuse");
    };
    assert_eq!(err.code(), "PLACEMENT_UNSUPPORTED");
    assert!(err.detail().contains("kind"), "{}", err.detail());
    assert!(
        !rig.work.0.join("skills").exists(),
        "nothing materialized on the refusal"
    );
}

/// ITEM PAIR (go-back converges configs): a targeted `update <mcp>@<version>` must leave the
/// agent configs carrying the RESTORED document before it returns success. Before the fix only
/// the store/lock moved — the configs kept the newer URL until the next sweep.
#[test]
fn a_targeted_go_back_converges_the_configs_to_the_restored_document() {
    let rig = Rig::new("goback-converge");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v1 = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/v1").as_bytes(),
    )]);
    let v2 = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/v2").as_bytes(),
    )]);
    let plane = FakePlane::new()
        .with_version("s_linear", &v1)
        .with_version("s_linear", &v2);
    plane.serves(vec![delivered_mcp("s_linear", "linear", &v1)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v1)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\n[defaults.mcp]\n{SAFE}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v1")
    );

    // The team moves to v2 (generation 2 — a publish moved the pointer); the sweep converges the
    // configs onto it.
    let mut served2 = delivered_mcp("s_linear", "linear", &v2);
    served2.generation = 2;
    plane.serves(vec![served2]);
    let mut entry2 = mcp_catalog_entry("s_linear", "linear", &v2);
    entry2.generation = 2;
    let dir2 = FakeDirectory {
        skills: vec![entry2],
        channels: Vec::new(),
    };
    let out2 = sweep(&ctx, &plane, &dir2);
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v2"),
        "warnings: {:?} rows: {:?}",
        out2.warnings,
        out2.data
            .skills
            .iter()
            .map(|s| (&s.skill, &s.action))
            .collect::<Vec<_>>()
    );

    // The deliberate go-back: the configs carry v1's document again BEFORE the verb returns.
    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v1.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the go-back applies");
    let text = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        text.contains("https://mcp.example/v1") && !text.contains("https://mcp.example/v2"),
        "the configs carry the restored document immediately: {text}"
    );
    // The row reports the per-agent outcomes of the converge it just ran.
    let row = &out.data.skills[0];
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor", "openclaw"].into());
}

/// ITEM PAIR (targeted go-back honors narrowing): a bundle narrowed to ONE harness must stay in
/// that one harness through a targeted `update <mcp>@<version>` — the go-back's converge reaches
/// only the harnesses that already hold a ledger entry here, so a harness the narrowing excluded
/// never gains one. Before the fix the targeted demand carried an EMPTY filter, which
/// `filter_admits` reads as ALL harnesses: openclaw (detected, engaged, excluded by the row)
/// gained an entry the sweep then had to claw back.
#[test]
fn a_targeted_go_back_never_reaches_a_narrowing_excluded_harness() {
    let rig = Rig::new("goback-narrow");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0); // BOTH hermetic slugs detect; the narrowing admits one.
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    plane.serves(vec![delivered_mcp("s_linear", "linear", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\n[defaults.mcp]\nharness = [\"cursor\"]\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let openclaw = rig.home.0.join(".openclaw/openclaw.json");
    assert!(
        rig.home.0.join(".cursor/mcp.json").exists() && !openclaw.exists(),
        "the sweep placed cursor alone"
    );

    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the go-back applies");

    // The excluded harness stayed excluded — no config was born for it.
    assert!(
        !openclaw.exists(),
        "a narrowing-excluded harness must not gain an entry on a targeted go-back"
    );
    let text = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap();
    assert!(text.contains("https://mcp.example/linear"), "{text}");
    let row = &out.data.skills[0];
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor"].into(), "{row:?}");
}

/// ITEM PAIR (keyless targeted converge skips): with the LEDGER deleted, a targeted go-back has
/// no ownership record to reuse — it must write NOTHING and say so, leaving the heal to the next
/// sweep. Before the fix it minted a fresh `topos-local-linear` key and placed a DUPLICATE entry
/// beside the original `topos-eng-linear` one (now foreign and unremovable).
#[test]
fn a_deleted_ledger_makes_the_targeted_go_back_skip_with_a_warning() {
    let rig = Rig::new("goback-keyless");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let openclaw = rig.home.0.join(".openclaw/openclaw.json");
    let before = (
        std::fs::read(&cursor).unwrap(),
        std::fs::read(&openclaw).unwrap(),
    );

    // The failure shape: the ownership record is GONE (the kind marker still classifies).
    std::fs::remove_file(rig.layout().mcp_ledger_path()).unwrap();

    // The converge the go-back hand-runs: zero writes, one honest warning.
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let (states, warnings) = crate::mcp_engine::converge_bundle_now(&ctx, &sid, "linear");
    assert!(states.is_empty(), "{states:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("MCP_OWNERSHIP_MISSING") && w.contains("next update heals")),
        "{warnings:?}"
    );

    // And through the whole verb: the configs are byte-identical — no duplicate key appeared.
    ops::pull(
        &ctx,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
            store: ops::StoreScope::Here,
        },
    )
    .expect("the go-back still applies store-side");
    let after = (
        std::fs::read(&cursor).unwrap(),
        std::fs::read(&openclaw).unwrap(),
    );
    assert_eq!(
        before, after,
        "no config byte moved without an ownership record"
    );
    assert!(
        !String::from_utf8_lossy(&after.0).contains("topos-local"),
        "no duplicate locally-minted entry"
    );
}

/// ITEM PAIR (empty map ≠ mcp): a SKILL delivered at project scope with NO detected agent records
/// an empty placement map — it must NOT be reported to the fleet as held (there are no bytes
/// anywhere an agent reads). Before the fix the empty map rode the config-placed exemption and
/// the report claimed it current.
#[test]
fn a_scoped_out_skill_with_an_empty_map_is_not_reported_held() {
    let rig = Rig::new("scoped-out");
    rig.seed_session();
    // Deliberately NO detect dirs anywhere: the project plan has no agent to place for.
    let v = mk_version(&[("SKILL.md", b"# alpha\n")]);
    let plane = FakePlane::new().with_version("s_alpha", &v);
    let mut delivered = delivered_mcp("s_alpha", "alpha", &v);
    delivered.kind = "skill".into();
    plane.serves(vec![delivered]);
    let mut entry = mcp_catalog_entry("s_alpha", "alpha", &v);
    entry.kind = "skill".into();
    let dir = FakeDirectory {
        skills: vec![entry],
        channels: Vec::new(),
    };
    // Person scope demands nothing; the PROJECT manifest carries the feed row.
    rig.write_global("[bundles]\n");
    std::fs::write(
        rig.work.0.join("topos.toml"),
        format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    let reported = plane.reported.lock().unwrap().clone();
    assert!(
        !reported.iter().any(|(id, ..)| id == "s_alpha"),
        "an empty-map skill placed nowhere must not be reported held: {reported:?}"
    );
}

/// ITEM PAIR (bare diff): a delivered, store-only mcp bundle's bare `diff` answers the honest
/// no-local-draft shape — an empty diff whose endpoint is the held current — instead of the
/// "placement map has no placement" corruption error the pre-fix path tripped.
#[test]
fn a_bare_diff_of_a_config_placed_bundle_answers_the_empty_no_draft_shape() {
    let rig = Rig::new("bare-diff");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    let d = ops::diff(&ctx, "linear", None, ops::DiffBudget::resolve(None, true)).unwrap();
    assert_eq!(d.diff, "", "no working tree — no draft to show");
    assert!(!d.truncated);
    assert_eq!(d.version_id, topos_core::digest::to_hex(&v.id));
    assert_eq!(d.bundle_digest, topos_core::digest::to_hex(&v.digest));
    assert!(d.files.is_empty());
}
