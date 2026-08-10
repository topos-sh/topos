//! The MCP placement engine + ownership custody, end to end: the per-scope converge over a fake
//! HOME (synthetic descriptors — every surface Home-rooted, so no test can touch a real machine's
//! config), and the reconcile integration over the same fakes the manifest suite drives.
//!
//! The trust spine under test: entries land EXACTLY per dialect and byte-identical to the pure
//! drivers' rendering; a hand-edited entry is drift — never overwritten, never removed, disclosed;
//! a foreign occupant is never touched or claimed; removal converges only what the custody proves
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
use topos_harness::mcp::{self, AuthHint, McpDialect, McpEntry, plugin_dir};
use topos_harness::registry::{self, KnownHarness};
use topos_harness::triggers::{TriggerAdapter, TriggerArtifact};
use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireMe, WireProposalIndex,
    WireSkillIndex, WireSkillIndexEntry, WireSkillLog,
};
use topos_types::results::TargetOutcome;
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::config_custody::{self, ScopeEntries};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::fs_seam::{FaultFs, FsOps as _, RealFs};
use crate::ids::test_sources::{FixedClock, SeqIds};
use crate::mcp_engine::{self, DemandedBundle, McpDemand, ScopeIo};
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
}

impl TriggerAdapter for TmpHarness {
    fn slug(&self) -> &'static str {
        HarnessId::ClaudeCode.slug()
    }

    fn install(&self) -> TriggerReport {
        no_trigger()
    }

    fn remove(&self) -> TriggerReport {
        no_trigger()
    }

    fn artifacts(&self) -> Vec<TriggerArtifact> {
        Vec::new()
    }

    fn present(&self) -> bool {
        !self.artifacts().is_empty()
    }
}
fn no_trigger() -> TriggerReport {
    TriggerReport {
        agent: "claude-code".to_owned(),
        currency_kind: CurrencyKind::ExplicitPullOnly,
        touched_path: None,
        marker_id: "test".into(),
        state: TriggerState::Inactive,

        note: None,
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
            triggers: crate::ops::Triggers::active_only(&self.harness),
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
        mcp_server_name: None,
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

static SYNTHETIC: &[KnownHarness] = &[
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

/// **Z1 — the containment rail is re-run at the WRITE boundary.** A plan resolves a project
/// surface once and the converge writes it later; `replace_config` follows symlinks, so a path
/// component swapped for an outward symlink in that window would put managed bytes outside the
/// checkout. The proof therefore re-runs immediately before any byte moves: ZERO writes outside,
/// the `unprovable` state spoken with the same note the planner's withheld line carries, and the
/// escape disclosed.
#[test]
fn a_surface_symlinked_out_between_plan_and_write_is_refused_with_zero_writes() {
    let fs = RealFs;
    let project = Scratch::new("wb-project");
    let outside = Scratch::new("wb-outside");
    std::fs::create_dir_all(project.0.join(".cursor")).unwrap();
    let layout = Layout::new(&project.0.join(".topos"));
    let io = ScopeIo {
        fs: &fs,
        layout: &layout,
        home: project.0.clone(),
        project_root: Some(project.0.clone()),
    };
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    // The plan proves containment HERE, while `.cursor` is an ordinary dir inside the checkout.
    let demands = plan(&io, vec![d]);
    assert!(
        demands[0].plan.entries_for("cursor").is_some(),
        "the surface planned clean: {:?}",
        demands[0].plan
    );

    // ...and the checkout is rewritten under it before the write lands.
    std::fs::remove_dir_all(project.0.join(".cursor")).unwrap();
    std::os::unix::fs::symlink(&outside.0, project.0.join(".cursor")).unwrap();

    let out = mcp_engine::converge(&io, &demands, &synthetic(), &all_slugs(), &no_hold(), true);

    assert!(
        !outside.0.join("mcp.json").exists(),
        "no managed byte may land outside the checkout"
    );
    let st = state_of(&out, "s_a", "cursor");
    assert_eq!(st.state, TargetOutcome::Unprovable, "{st:?}");
    assert_eq!(
        st.note.as_deref(),
        Some("the config path does not resolve inside this checkout")
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.starts_with("PLACEMENT_ESCAPES_PROJECT")),
        "{:?}",
        out.warnings
    );
}

/// **Z2 — a targeted converge whose whole reach is WITHHELD still speaks, and still recovers.**
/// The reach resolves to one harness whose surface no longer proves inside the checkout, so there
/// is nothing to write anywhere. Returning early there would drop the per-agent line the receipt
/// owes AND skip the intent journal's crash recovery, which runs inside the converge — so a crash
/// left by an earlier run would survive every targeted verb.
#[test]
fn a_targeted_converge_with_only_withheld_surfaces_reports_and_still_recovers() {
    let rig = Rig::new("withheld-targeted");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let proj = Scratch::new("withheld-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".cursor")).unwrap();
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        format!("[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"./.cursor/mcp.json\"] }}\n"),
    )
    .unwrap();
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
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    assert!(
        proj.0.join(".cursor/mcp.json").exists(),
        "the sweep placed the one narrowed surface"
    );

    // The checkout is rewritten: the ONLY harness the record reaches no longer proves inside it.
    let outside = Scratch::new("withheld-outside");
    std::fs::remove_dir_all(proj.0.join(".cursor")).unwrap();
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".cursor")).unwrap();

    // A crash-left intent standing in the scope journal — only a run that ENTERS the converge
    // resolves it.
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    let mut custody = crate::config_custody::ScopeEntries::load(&rig.fs, &playout).unwrap();
    let mut intents = std::collections::BTreeMap::new();
    intents.insert(
        crate::config_custody::placement_key("codex", "topos-eng-ghost"),
        crate::config_custody::PendingIntent {
            bundle_id: "s_ghost".to_owned(),
            version_id: "v1".to_owned(),
            file: proj.0.join(".codex/config.toml").display().to_string(),
            fingerprint: "deadbeef".to_owned(),
            owns_file: false,
        },
    );
    custody.journal(&rig.fs, &playout, intents).unwrap();
    assert!(
        crate::config_custody::ScopeEntries::load(&rig.fs, &playout)
            .unwrap()
            .has_pending(),
        "the crash-left intent stands before the verb"
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

    // It SPOKE: the per-agent line survives a run that wrote nothing.
    let row = &out.data.skills[0];
    let st = row
        .harnesses
        .iter()
        .find(|h| h.agent == "cursor")
        .unwrap_or_else(|| panic!("cursor state: {row:?}"));
    assert_eq!(st.state, TargetOutcome::Unprovable, "{st:?}");
    assert!(
        !outside.0.join("mcp.json").exists(),
        "nothing lands outside the checkout"
    );
    // And it RECOVERED: the journal the crash left is resolved by observation, not carried.
    assert!(
        !crate::config_custody::ScopeEntries::load(&rig.fs, &playout)
            .unwrap()
            .has_pending(),
        "the converge ran its crash recovery"
    );
}

/// **Z5 — a `dest` move is a wholly successful sweep, and it says so.** Moving a bundle's config
/// destination from one agent to another removes its entry from the old surface and deletes the
/// file topos wholly owned there. Both are work that WORKED: routed through the warning channel
/// the deletion made a clean run count itself FAILED and exit non-zero while `--json` still said
/// `ok: true`. It rides disclosures now — and the row reporting the surfaces the bundle left names
/// it the way a person knows it. For a LOCALLY ADOPTED bundle no delivery cache describes, that
/// name comes from the bundle's own record; the opaque store id is the last resort, not the first
/// answer.
#[test]
fn a_dest_move_is_a_clean_sweep_that_names_the_bundle_it_moved() {
    let rig = Rig::new("dest-move");
    seed_harness_dirs(&rig.home.0);
    rig.write_global("[bundles]\n");
    let dir = rig.home.0.join("demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        "{\"name\":\"io.test/demo\",\"description\":\"D.\",\"version\":\"1.0.0\",\
         \"remotes\":[{\"type\":\"streamable-http\",\"url\":\"https://demo.example/mcp\"}]}",
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // Adopted in place: the store tracks the folder under an opaque id, and no delivery cache
    // will ever describe it — the shape whose receipt used to print that id at a person.
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default())
        .expect("the local mcp folder adopts");
    let plane = FakePlane::new();
    let fdir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = {{ kind = \"mcp\", dest = [\"~/.cursor/mcp.json\"] }}\n",
        dir.display()
    ));
    sweep(&ctx, &plane, &fdir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists(), "the first sweep placed cursor");

    // The row's destination MOVES. The old surface loses the entry, and the file topos created
    // there goes with it.
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = {{ kind = \"mcp\", dest = [\"~/.openclaw/openclaw.json\"] }}\n",
        dir.display()
    ));
    let out = sweep(&ctx, &plane, &fdir);

    assert!(
        rig.home.0.join(".openclaw/openclaw.json").exists(),
        "the new destination gained the entry"
    );
    assert!(!cursor.exists(), "the wholly-owned old file was deleted");
    // NOTHING failed — the whole point: a successful move exits 0.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // The deletion is a DISCLOSURE, and it still names the file it deleted. The MACHINE lane keeps
    // the code (an agent branches on it); the TTY prints the sentence, because a person reading a
    // receipt should not have to skip a machine word to reach English.
    assert!(
        out.disclosures
            .iter()
            .any(|d| d.starts_with("MCP_FILE_REMOVED") && d.contains("mcp.json")),
        "{:?}",
        out.disclosures
    );
    let note_tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(
        note_tty.contains("held only topos entries and was deleted")
            && !note_tty.contains("MCP_FILE_REMOVED"),
        "the note reads as English on the TTY: {note_tty}"
    );
    // The row for the surfaces the bundle left names `demo` — never the store's id for it.
    let left = out
        .data
        .skills
        .iter()
        .find(|r| r.action == topos_types::results::PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert_eq!(left.skill, "demo", "{left:?}");

    // BOTH ROWS STAND — the move and the surface it vacated are each true, and each worth seeing.
    let for_demo: Vec<_> = out
        .data
        .skills
        .iter()
        .filter(|r| r.skill == "demo")
        .collect();
    assert_eq!(for_demo.len(), 2, "{for_demo:?}");

    // …AND THE SUMMARY COUNTS ONE BUNDLE. The tally used to count rows, so one bundle moving
    // destinations reported "Checked 2 bundles" over a machine holding one — the receipt above it
    // naming that single bundle twice. Distinct bundles are counted once now, under the primary
    // outcome: the bundle was UPDATED, and the vacated surface is a detail of the move.
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(
        tty.contains("Checked 1 bundle: 1 updated."),
        "one bundle, counted once, under what happened to it: {tty}"
    );
}

/// **The entries plan is what decides reach — and what says what reach COST.** Four facts, over
/// the one planner every demand is built through:
///
/// - no narrowing ⇒ every engaged harness with a surface at this scope gets a target;
/// - a narrowing admits exactly what it names, and stays SILENT about the rest — a row that never
///   asked for a harness is owed no disclosure about it;
/// - a harness with no surface AT THIS SCOPE is WITHHELD, carrying the phrase the receipt prints
///   (this is what keeps the per-agent `not-supported` lines alive now that the converge no longer
///   derives them);
/// - a harness that is neither detected nor already configured earns neither a target nor a line:
///   there is no agent here to reach, and nothing was withheld from anyone.
#[test]
fn the_entries_plan_carries_reach_and_names_what_it_withheld() {
    let home = Scratch::new("entries-plan");
    let fs = RealFs;
    let plan_at =
        |detected: &BTreeSet<String>, project: Option<&Path>, reach: Option<&[String]>| {
            crate::placement::entries_plan_at(&fs, &synthetic(), &home.0, detected, project, reach)
        };
    let slugs = |p: &crate::placement::PlacementPlan| -> Vec<String> {
        p.entries().map(|e| e.agent.clone()).collect()
    };
    let withheld = |p: &crate::placement::PlacementPlan| -> Vec<(String, TargetOutcome)> {
        p.withheld
            .iter()
            .map(|w| (w.agent.clone(), w.state))
            .collect()
    };

    // Every engaged harness, person scope: six targets, nothing withheld.
    let all = plan_at(&all_slugs(), None, None);
    assert_eq!(slugs(&all).len(), synthetic().len(), "{:?}", slugs(&all));
    assert!(withheld(&all).is_empty(), "{:?}", withheld(&all));
    // The target names the DRIVER file — the plugin dialect's own `.mcp.json`, not its dir.
    let plugin = all.entries_for("claude-code").expect("claude-code planned");
    assert!(
        plugin.file.ends_with(".mcp.json"),
        "{}",
        plugin.file.display()
    );

    // A narrowing admits exactly what it names — and says nothing about the rest.
    let narrowed = plan_at(&all_slugs(), None, Some(&["cursor".to_owned()]));
    assert_eq!(slugs(&narrowed), vec!["cursor".to_owned()]);
    assert!(withheld(&narrowed).is_empty(), "{:?}", withheld(&narrowed));

    // PROJECT scope: the two harnesses with no project surface are withheld, by name and phrase.
    let project = Scratch::new("entries-plan-co");
    let proj = plan_at(&all_slugs(), Some(&project.0), None);
    for slug in ["openclaw", "hermes-agent"] {
        assert!(
            proj.entries_for(slug).is_none(),
            "{slug} must not be planned"
        );
        let w = proj.withheld_for(slug).unwrap_or_else(|| panic!("{slug}"));
        assert_eq!(
            (w.state, w.note.as_str()),
            (TargetOutcome::Withheld, "no project-level config")
        );
    }
    assert!(proj.entries_for("cursor").is_some());

    // Nothing detected and no config on disk: no target, and nothing withheld from anyone.
    let cold = Scratch::new("entries-plan-cold");
    let cold_plan =
        crate::placement::entries_plan_at(&fs, &synthetic(), &cold.0, &BTreeSet::new(), None, None);
    assert!(slugs(&cold_plan).is_empty(), "{:?}", slugs(&cold_plan));
    assert!(
        withheld(&cold_plan).is_empty(),
        "{:?}",
        withheld(&cold_plan)
    );
}

/// The synthetic table as the engine takes it — the same `&[&KnownHarness]` view the real
/// [`mcp_harnesses`](topos_harness::mcp::descriptor::mcp_harnesses) hands production.
fn synthetic() -> Vec<&'static KnownHarness> {
    SYNTHETIC.iter().collect()
}

fn all_slugs() -> BTreeSet<String> {
    synthetic().iter().map(|h| h.slug.to_owned()).collect()
}

fn person_io<'a>(fs: &'a RealFs, layout: &'a Layout, home: &Path) -> ScopeIo<'a> {
    ScopeIo {
        fs,
        layout,
        home: home.to_path_buf(),
        project_root: None,
    }
}

fn demand(bundle_id: &str, name: &str, ws: Option<&str>, server: &str) -> DemandedBundle {
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
fn plan(io: &ScopeIo<'_>, rows: Vec<DemandedBundle>) -> Vec<McpDemand> {
    rows.into_iter()
        .map(|r| r.planned(io, &synthetic(), &all_slugs()))
        .collect()
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
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
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

    // Six states, all `placed` — this converge wrote every one of them — each carrying its
    // reload note (a fresh placement).
    for h in synthetic() {
        let st = state_of(&out, "s_linear", h.slug);
        assert!(st.state.wrote(), "{}", h.slug);
        assert_eq!(
            st.note.as_deref(),
            h.mcp().map(|m| m.reload_note),
            "{}",
            h.slug
        );
    }
    // The custody: one key, six entries, every fingerprint matching what the file provably holds.
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert_eq!(custody.key_of("s_linear").unwrap(), "topos-eng-linear");
    assert_eq!(custody.row_count(), 6);
    assert!(custody.doc.pending.is_empty());
    for (k, _b, e) in custody.iter() {
        let slug = k.split('/').next().unwrap();
        let h = synthetic().into_iter().find(|h| h.slug == slug).unwrap();
        let dialect = h.mcp().unwrap().user.unwrap().dialect;
        let observed = mcp::observe(dialect, std::fs::read(&e.file).ok().as_deref());
        assert_eq!(
            observed.entries.get("topos-eng-linear"),
            Some(&e.fingerprint),
            "{k}: custody fingerprint must match the file"
        );
        assert!(e.owns_file, "{k}: a file we created is wholly ours");
    }

    // Idempotent: a second converge leaves every byte and reports Current (no reload note).
    let before: Vec<Vec<u8>> = synthetic()
        .iter()
        .map(|h| std::fs::read(h.mcp_user_path(&home.0).unwrap()).ok())
        .map(Option::unwrap_or_default)
        .collect();
    let out2 = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    for h in synthetic() {
        let st = state_of(&out2, "s_linear", h.slug);
        assert_eq!(
            (st.state, st.note.as_deref()),
            (TargetOutcome::Current, None),
            "{}",
            h.slug
        );
    }
    let after: Vec<Vec<u8>> = synthetic()
        .iter()
        .map(|h| std::fs::read(h.mcp_user_path(&home.0).unwrap()).ok())
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
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
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
        &synthetic(),
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
    // The custody: no entries, the key retired — and NEVER reusable by another bundle.
    let mut custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert_eq!(custody.row_count(), 0);
    assert_eq!(custody.doc.retired["topos-eng-alpha"], "s_a");
    assert_eq!(
        custody.mint_key("s_other", "alpha", Some("eng")),
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
    let cursor_only = |d: &DemandedBundle| {
        let mut d = d.clone();
        d.reach = Some(vec!["cursor".into()]);
        d
    };
    let io = person_io(&fs, &layout, &home.0);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![cursor_only(&d)]),
        &synthetic(),
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

    // The next converge reads it as DRIFT: untouched, reported, the custody keeps the OLD
    // fingerprint so drift survives re-runs.
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![cursor_only(&d)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Drifted
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        edited,
        "bytes untouched"
    );

    // Removal LEAVES the drifted entry and disclosed it (never destroys a hand edit).
    let out = mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    assert!(
        out.removed
            .iter()
            .any(|r| r.state.state == TargetOutcome::Drifted),
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
    d.reach = Some(vec!["cursor".into()]);
    let io = person_io(&fs, &layout, &home.0);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Conflicting
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        foreign,
        "foreign bytes untouched"
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(
        !custody.holds("cursor/topos-eng-alpha"),
        "a foreign entry never enters the custody"
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
    d.reach = Some(vec!["cursor".into()]);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let line = out
        .warnings
        .iter()
        .find(|w| w.contains("MCP_SECRET_REFUSED"))
        .unwrap_or_else(|| panic!("the typed refusal is named: {:?}", out.warnings));
    // ONE code per line. The gate's code used to be printed INSIDE this line's own
    // (`MCP_UNPLACEABLE alpha: MCP_SECRET_REFUSED: …`), so a reader met two machine words
    // before the first English one and a parser found two codes on one line.
    assert!(
        line.starts_with("MCP_SECRET_REFUSED alpha: "),
        "the code leads, once: {line}"
    );
    assert!(!line.contains("MCP_UNPLACEABLE"), "{line}");
    assert_eq!(
        line.split_whitespace()
            .filter(
                |t| t.starts_with("MCP_") && t.chars().all(|c| c.is_ascii_uppercase() || c == '_')
            )
            .count(),
        1,
        "exactly one machine code on the line: {line}"
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Unprovable
    );
    assert!(
        !home.0.join(".cursor/mcp.json").exists(),
        "nothing was placed"
    );
}

/// A user's sibling top-level key in the plugin dir's `.mcp.json` is content topos did not
/// write: the surface backs off whole — unprovable, byte-identical, disclosed — and the key is
/// NEVER destroyed, neither by an update rewrite nor by removal deleting the file.
#[test]
fn a_sibling_key_in_the_plugin_mcp_json_backs_the_surface_off_and_survives() {
    let home = Scratch::new("plugin-sibling");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let claude_only = |d: &DemandedBundle| {
        let mut d = d.clone();
        d.reach = Some(vec!["claude-code".into()]);
        d
    };
    let v1 = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/v1"),
    );
    mcp_engine::converge(
        &io,
        &plan(&io, vec![claude_only(&v1)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    // The user adds a sibling top-level key beside mcpServers.
    let mcp_path = home.0.join(".claude/skills/topos-mcp/.mcp.json");
    let mut root: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&mcp_path).unwrap()).unwrap();
    root.as_object_mut()
        .unwrap()
        .insert("theme".to_owned(), serde_json::json!("dark"));
    let edited = serde_json::to_string_pretty(&root).unwrap() + "\n";
    std::fs::write(&mcp_path, &edited).unwrap();

    // An update (the url moved) must not rewrite the file over the sibling key.
    let v2 = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/v2"),
    );
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![claude_only(&v2)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let now = std::fs::read_to_string(&mcp_path).unwrap();
    assert!(
        now.contains("\"theme\""),
        "the user's sibling key survives an update: {now}"
    );
    assert_eq!(now, edited, "the surface backs off byte-identical");
    assert_eq!(
        state_of(&out, "s_a", "claude-code").state,
        TargetOutcome::Unprovable
    );

    // A removal (the demand drops) must not delete the file over the sibling key either.
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    let kept = std::fs::read_to_string(&mcp_path)
        .unwrap_or_else(|e| panic!("the plugin .mcp.json was deleted over a user key: {e}"));
    assert!(kept.contains("\"theme\""), "{kept}");
}

/// The constant `.claude-plugin/plugin.json` rides the foreign-occupant rule too: a hand-edited
/// manifest is never rewritten by the next entry change and never deleted by the last entry's
/// removal — kept byte-identical, the dir left standing over it, both disclosed. (A pristine
/// manifest still heals and prunes exactly as before — see the six-dialect and removal tests.)
#[test]
fn a_hand_edited_plugin_manifest_survives_update_and_removal_disclosed() {
    let home = Scratch::new("plugin-manifest");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let claude_only = |d: &DemandedBundle| {
        let mut d = d.clone();
        d.reach = Some(vec!["claude-code".into()]);
        d
    };
    let v1 = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/v1"),
    );
    mcp_engine::converge(
        &io,
        &plan(&io, vec![claude_only(&v1)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    // The user edits the manifest by hand.
    let manifest = home
        .0
        .join(".claude/skills/topos-mcp/.claude-plugin/plugin.json");
    let edited = std::fs::read_to_string(&manifest)
        .unwrap()
        .replace("Topos MCP", "My MCP");
    std::fs::write(&manifest, &edited).unwrap();

    // An entry UPDATE (the url moved) rewrites the .mcp.json — and must not touch the manifest.
    let v2 = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/v2"),
    );
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![claude_only(&v2)]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(state_of(&out, "s_a", "claude-code").state.wrote());
    let mcp_path = home.0.join(".claude/skills/topos-mcp/.mcp.json");
    assert!(
        std::fs::read_to_string(&mcp_path).unwrap().contains("/v2"),
        "the entry itself still updates"
    );
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        edited,
        "the hand-edited manifest survives an entry update byte-identical"
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("MCP_PLUGIN_MANIFEST_KEPT")),
        "the kept manifest is disclosed: {:?}",
        out.warnings
    );

    // The LAST entry's removal deletes the wholly-owned .mcp.json — and keeps the edited
    // manifest, the dir standing over it, disclosed.
    let out = mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    assert!(!mcp_path.exists(), "the wholly-owned entries file leaves");
    assert_eq!(
        std::fs::read_to_string(&manifest)
            .unwrap_or_else(|e| panic!("the hand-edited manifest was destroyed on removal: {e}")),
        edited,
        "the hand edit survives the last entry's removal"
    );
    assert!(
        home.0.join(".claude/skills/topos-mcp").exists(),
        "the dir stays with its foreign occupant"
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("MCP_PLUGIN_MANIFEST_KEPT")),
        "{:?}",
        out.warnings
    );
}

/// The heal path stays: a hand-DELETED manifest (absent, so provably nobody's) is re-written
/// pristine beside entries that remain.
#[test]
fn a_hand_deleted_plugin_manifest_heals_back_beside_remaining_entries() {
    let home = Scratch::new("plugin-heal");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["claude-code".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let manifest = home
        .0
        .join(".claude/skills/topos-mcp/.claude-plugin/plugin.json");
    std::fs::remove_file(&manifest).unwrap();

    // The next converge (nothing changed — a Leave) re-heals the constant file.
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        std::fs::read(&manifest).unwrap_or_else(|e| panic!("the manifest was not healed: {e}")),
        plugin_dir::manifest_bytes()
    );
}

/// Every converge entry point serializes on the scope's `locks/mcp.lock`: a run that starts
/// while another process holds it WAITS instead of interleaving the custody + config
/// read-modify-write (flock contends across open file descriptions, so a second in-process
/// guard stands in for a sibling process here).
#[test]
fn converges_serialize_on_the_per_scope_mcp_lock() {
    let home = Scratch::new("mcp-lock");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    std::fs::create_dir_all(layout.locks_dir()).unwrap();
    let held = fs
        .lock_exclusive(&layout.locks_dir().join("mcp.lock"))
        .unwrap();

    let home_path = home.0.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        let fs = RealFs;
        let layout = Layout::new(&home_path.join(".topos"));
        let io = ScopeIo {
            fs: &fs,
            layout: &layout,
            home: home_path.clone(),
            project_root: None,
        };
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        let out = mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        tx.send(out).unwrap();
    });

    // While the lock is held the converge must not complete (and must not have written).
    assert!(
        rx.recv_timeout(std::time::Duration::from_millis(400))
            .is_err(),
        "a converge must wait for the scope's mcp lock"
    );
    assert!(
        !home.0.join(".cursor/mcp.json").exists(),
        "no config byte moves while another converge holds the lock"
    );
    drop(held);
    let out = rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("the released lock lets the converge finish");
    worker.join().unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(state_of(&out, "s_a", "cursor").state.wrote());
    assert!(home.0.join(".cursor/mcp.json").exists());
}

/// A surface path move (an env-override change) leaves the custody row recorded at the OLD file:
/// the converge must not silently drop or re-point it — the row is a disclosed stale class,
/// warned with the old path, the row and the old file's bytes both left in place.
#[test]
fn a_moved_surface_path_discloses_the_stale_row_and_never_drops_it() {
    let home = Scratch::new("moved");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let old_file = home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&old_file).unwrap();

    // The surface resolves ELSEWHERE now (the descriptor's suffix moved).
    static MOVED: &[KnownHarness] = &[registry::home_rooted_mcp_row(
        "cursor",
        "Cursor",
        ".cursor-next/mcp.json",
        McpDialect::CursorJson,
        None,
        "restart cursor",
    )];
    // An undemanded removal run over the moved surface: the row is NOT dropped, the old file is
    // NOT touched, and the stale class is disclosed naming the old path.
    let out = mcp_engine::converge(
        &io,
        &[],
        &MOVED.iter().collect::<Vec<_>>(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("MCP_ENTRY_STALE_PATH") && w.contains(".cursor/mcp.json")),
        "{:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read_to_string(&old_file).unwrap(),
        placed,
        "the old file's bytes stay untouched"
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(
        custody.has_entries_for("s_a"),
        "the stale row is kept, never silently dropped: {custody:?}"
    );
    assert_eq!(
        Path::new(
            &custody
                .row(&config_custody::placement_key("cursor", "topos-eng-alpha"))
                .unwrap()
                .file
        ),
        old_file,
        "the row still names the old file"
    );
}

/// A hand-deleted plugin dir must not leave phantom custody entries: the next removal-allowed
/// converge stale-drops the rows the surface no longer holds, so the key retires and the bundle
/// can actually leave.
#[test]
fn a_hand_deleted_plugin_dir_sheds_its_ledger_entries_on_the_next_converge() {
    let home = Scratch::new("plugin-deleted");
    let fs = RealFs;
    let layout = Layout::new(&home.0.join(".topos"));
    let io = person_io(&fs, &layout, &home.0);
    let mut d = demand(
        "s_a",
        "alpha",
        Some("eng"),
        &server_json("https://mcp.example/a"),
    );
    d.reach = Some(vec!["claude-code".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(
        ScopeEntries::load(&fs, &layout)
            .unwrap()
            .has_entries_for("s_a")
    );
    // The user deletes the whole plugin dir by hand.
    std::fs::remove_dir_all(home.0.join(".claude/skills/topos-mcp")).unwrap();

    // The demand drops: the custody must shed the phantom rows and retire the key.
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(
        !custody.has_entries_for("s_a"),
        "phantom entries survive a hand-deleted plugin dir: {custody:?}"
    );
    assert_eq!(
        custody
            .doc
            .retired
            .get("topos-eng-alpha")
            .map(String::as_str),
        Some("s_a"),
        "{custody:?}"
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
        d.reach = Some(vec![slug.to_owned()]);
        mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        let file = home.0.join(rel);
        let created = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{slug}: {e}"));
        // The USER adds a plain (non-topos) entry to the file topos created.
        std::fs::write(&file, inject(&created)).unwrap();

        // The demand drops: our entry leaves, the FILE STAYS with the user's content.
        let out = mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
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
        let custody = ScopeEntries::load(&fs, &layout).unwrap();
        assert!(!custody.has_entries_for("s_a"), "{slug}");
    }
}

/// F1 belt two: the moment a converge OBSERVES user content in a file topos created, the
/// custody's whole-file-ownership flag goes false — a later removal can never trust a flag the
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
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let lk = config_custody::placement_key("cursor", "topos-eng-alpha");
    assert!(
        ScopeEntries::load(&fs, &layout)
            .unwrap()
            .row(&lk)
            .unwrap()
            .owns_file,
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
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Current
    );
    assert!(
        !ScopeEntries::load(&fs, &layout)
            .unwrap()
            .row(&lk)
            .unwrap()
            .owns_file,
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
    d.reach = Some(vec!["cursor".into()]);
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(state_of(&out, "s_two", "cursor").state.wrote());
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
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let cursor = home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();

    // Undemanded but HELD (its workspace was unreachable): byte-identical, custody kept.
    let hold: HashSet<String> = ["s_a".to_owned()].into();
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &hold, true);
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), placed);
    assert!(
        ScopeEntries::load(&fs, &layout)
            .unwrap()
            .has_entries_for("s_a")
    );

    // Undemanded on a run that may NOT remove (a targeted update): same freeze.
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), false);
    assert_eq!(std::fs::read_to_string(&cursor).unwrap(), placed);
    assert!(
        ScopeEntries::load(&fs, &layout)
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
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let cursor = home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    let real_fp =
        mcp::observe(McpDialect::CursorJson, Some(placed.as_bytes())).entries["topos-eng-alpha"]
            .clone();

    // ORDER (b): the config write LANDED, the custody promotion did not — on disk the custody
    // still carries the old fingerprint plus the journaled intent. Without recovery the entry
    // would read as user drift forever.
    let mut custody = ScopeEntries::load(&fs, &layout).unwrap();
    let lk = config_custody::placement_key("cursor", "topos-eng-alpha");
    let mut stale = custody.row(&lk).unwrap().clone();
    stale.fingerprint = "0".repeat(64);
    custody.put(lk.clone(), "s_a".to_owned(), stale);
    custody
        .journal(
            &fs,
            &layout,
            std::iter::once((
                lk.clone(),
                config_custody::PendingIntent {
                    bundle_id: "s_a".into(),
                    version_id: "v1".into(),
                    file: cursor.display().to_string(),
                    fingerprint: real_fp.clone(),
                    owns_file: true,
                },
            ))
            .collect(),
        )
        .unwrap();
    assert!(custody.flush(&fs, &layout).is_empty());
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        (
            state_of(&out, "s_a", "cursor").state,
            state_of(&out, "s_a", "cursor").note.as_deref()
        ),
        (TargetOutcome::Current, None),
        "recovery promoted the landed write instead of reading it as drift"
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        placed,
        "no rewrite"
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(custody.doc.pending.is_empty());
    assert_eq!(custody.row(&lk).unwrap().fingerprint, real_fp);

    // ORDER (a): the intent was journaled, the config write never landed. Recovery drops the
    // intent; the standing entry (which matches the file) stays authoritative.
    let mut custody = ScopeEntries::load(&fs, &layout).unwrap();
    custody
        .journal(
            &fs,
            &layout,
            std::iter::once((
                lk.clone(),
                config_custody::PendingIntent {
                    bundle_id: "s_a".into(),
                    version_id: "v2".into(),
                    file: cursor.display().to_string(),
                    fingerprint: "f".repeat(64),
                    owns_file: true,
                },
            ))
            .collect(),
        )
        .unwrap();
    let out = mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    assert_eq!(
        state_of(&out, "s_a", "cursor").state,
        TargetOutcome::Current
    );
    let custody = ScopeEntries::load(&fs, &layout).unwrap();
    assert!(custody.doc.pending.is_empty());
    assert_eq!(custody.row(&lk).unwrap().fingerprint, real_fp);
}

/// ONE JOURNAL, MANY SURFACES. A converge walks every engaged harness in one run, and each
/// surface that writes journals its own intents — but there is only one journal, and journalling
/// REPLACES it. So intents a surface left standing (its record write failed, so the work has not
/// landed) must survive the surfaces that come after it.
///
/// The scenario: bundle X places on surface A, X's `entries.json` write FAILS there, and its
/// intents are re-journalled. Bundle Y then converges on surface B in the same run. Without the
/// guard, B's `journal()` overwrites X's intents in memory and on disk, B promotes and flushes,
/// and X is left live in A's config with no row and no journal — a state nothing can heal.
///
/// With the guard, B is refused for this run (the fail-closed posture the converge-start recovery
/// already takes), X's intents stay on disk, and the NEXT clean run heals X's row and places Y.
#[test]
fn a_later_surface_never_journals_over_intents_an_earlier_one_left_standing() {
    let probe = {
        let home = Scratch::new("xsurface-probe");
        let layout = Layout::new(&home.0.join(".topos"));
        let fault = FaultFs::new(0);
        let io = ScopeIo {
            fs: &fault,
            layout: &layout,
            home: home.0.clone(),
            project_root: None,
        };
        mcp_engine::converge(
            &io,
            &plan(&io, two_bundle_demands()),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        fault.ops_attempted()
    };
    assert!(probe > 0);

    let mut proven = 0usize;
    for fail_at in 1..=probe {
        let fs = RealFs;
        let home = Scratch::new(&format!("xsurface-{fail_at}"));
        let layout = Layout::new(&home.0.join(".topos"));
        let codex = home.0.join(".codex/config.toml");
        // Both bundles hold RECORDS of their own, so their row writes are real I/O that can fail.
        for id in ["s_x", "s_y"] {
            let sid = crate::id::SkillId::parse(id).unwrap();
            std::fs::create_dir_all(layout.skill_dir(&sid)).unwrap();
        }
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
                &plan(&io, two_bundle_demands()),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        // The ordering this guards: X's entry is LIVE in its config file, and X's record does not
        // know about it.
        let x_live = std::fs::read_to_string(&codex)
            .map(|t| t.contains("topos-eng-ex"))
            .unwrap_or(false);
        let x_recorded = !crate::config_custody::entries_of(&fs, &layout, "s_x").is_empty();
        if !x_live || x_recorded {
            continue;
        }
        proven += 1;
        // THE INVARIANT: X's intents survived every later surface in that run.
        let doc = crate::config_custody::read(&fs, &layout).expect("decipherable");
        assert!(
            doc.pending.values().any(|i| i.bundle_id == "s_x"),
            "fail_at={fail_at}: a later surface journalled over X's outstanding intents — the \
             entry is live with no row and nothing to recover from: {doc:?}"
        );
        // And the next clean run heals it.
        {
            let io = ScopeIo {
                fs: &fs,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            mcp_engine::converge(
                &io,
                &plan(&io, two_bundle_demands()),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        assert!(
            !crate::config_custody::entries_of(&fs, &layout, "s_x").is_empty(),
            "fail_at={fail_at}: the next run must heal X's row"
        );
        assert!(
            crate::config_custody::read(&fs, &layout)
                .unwrap()
                .pending
                .is_empty(),
            "fail_at={fail_at}: and clear the journal once it has"
        );
    }
    assert!(
        proven > 0,
        "no fault point produced the live-entry / unrecorded-row ordering this guards"
    );
}

/// Two bundles that converge on DIFFERENT surfaces in one run — the cross-surface fixture.
fn two_bundle_demands() -> Vec<DemandedBundle> {
    // X rides the EARLIER surface in table order (codex) and Y the later one (cursor), so Y's
    // journal write genuinely comes after X has left intents standing — the ordering the guard is
    // about. Reversed, X is the last surface and nothing follows it to overwrite anything.
    let mut x = demand(
        "s_x",
        "ex",
        Some("eng"),
        &server_json("https://mcp.example/x"),
    );
    x.reach = Some(vec!["codex".into()]);
    let mut y = demand(
        "s_y",
        "why",
        Some("eng"),
        &server_json("https://mcp.example/y"),
    );
    y.reach = Some(vec!["cursor".into()]);
    vec![x, y]
}

/// A REMOVAL must not swallow a crash left by an earlier run. `remove` is often the FIRST command
/// after a crash, and it journals intents of its own — and journalling REPLACES the journal
/// wholesale, so a recovery promotion left only in memory can be overwritten before it is durable.
/// `remove_bundle` therefore flushes its recovery pass before converging anything.
///
/// What this test guards is the OUTCOME, not that ordering: a crash-left intent, a removal as the
/// first command, one injected failure anywhere in its run, and then a clean retry — which must
/// finish the removal. An entry that survives the retry is stranded forever, because nothing is
/// left to prove it was topos's.
///
/// NOTE, honestly: this does not discriminate the early flush on its own. `remove_bundle`'s
/// trailing flush already persists a recovered promotion under a single injected fault, so the
/// early flush is defense in depth — it makes the durability ordering explicit instead of
/// incidental to where the last write happens to sit.
#[test]
fn a_removal_never_swallows_a_crash_left_intent_before_it_is_durable() {
    let probe = {
        let home = Scratch::new("rm-recover-probe");
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
        d.reach = Some(vec!["cursor".into()]);
        mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        fault.ops_attempted()
    };

    for fail_at in 1..=probe {
        let fs = RealFs;
        let home = Scratch::new(&format!("rm-recover-{fail_at}"));
        let layout = Layout::new(&home.0.join(".topos"));
        let cursor = home.0.join(".cursor/mcp.json");
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
        // A clean placement first.
        {
            let io = ScopeIo {
                fs: &fs,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            mcp_engine::converge(
                &io,
                &plan(&io, vec![d.clone()]),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        // A CRASH-LEFT intent: the previous run journalled and died before promoting. Its
        // fingerprint is what the file provably holds, so recovery must promote it.
        let real_fp = mcp::observe(
            McpDialect::CursorJson,
            std::fs::read(&cursor).ok().as_deref(),
        )
        .entries["topos-eng-alpha"]
            .clone();
        let mut custody = ScopeEntries::load(&fs, &layout).unwrap();
        let ck = config_custody::placement_key("cursor", "topos-eng-alpha");
        custody
            .journal(
                &fs,
                &layout,
                std::iter::once((
                    ck.clone(),
                    config_custody::PendingIntent {
                        bundle_id: "s_a".into(),
                        version_id: "v-crashed".into(),
                        file: cursor.display().to_string(),
                        fingerprint: real_fp.clone(),
                        owns_file: true,
                    },
                ))
                .collect(),
            )
            .unwrap();
        assert!(
            !crate::config_custody::read(&fs, &layout)
                .unwrap()
                .pending
                .is_empty()
        );

        // The removal runs as the FIRST command, and something in it fails.
        {
            let fault = FaultFs::new(fail_at);
            let io = ScopeIo {
                fs: &fault,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            let _ = mcp_engine::remove_bundle(&io, &synthetic(), &all_slugs(), "s_a", "a");
        }

        // The custody document must still be decipherable whatever failed.
        crate::config_custody::read(&fs, &layout).expect("decipherable");

        // THE INVARIANT, stated where a person would feel it: a clean retry FINISHES the removal.
        // If the crash-left promotion was swallowed — cleared in memory, then overwritten on disk
        // by the removal's own journal write — the entry loses its record, and no later run can
        // prove it was topos's: the retry leaves it in the config file forever.
        {
            let io = ScopeIo {
                fs: &fs,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            mcp_engine::remove_bundle(&io, &synthetic(), &all_slugs(), "s_a", "a");
        }
        let left = std::fs::read_to_string(&cursor).unwrap_or_default();
        assert!(
            !left.contains("topos-eng-alpha"),
            "fail_at={fail_at}: a clean retry must finish the removal — the entry is stranded: \
             {left}"
        );
    }
}

/// A DRIFTED entry survives the removal of the record that owned it. Drift is never clobbered, so
/// a removal legitimately leaves the entry standing — and a classic `remove` then deletes the
/// record directory, taking `entries.json` with it. If the row went with it, the hand-edited entry
/// would sit in the person's config forever with nothing left to prove it was ever topos's: no
/// later sweep could disclose it and none could ever take it out.
///
/// So the surviving rows move to the scope document first. They stay disclosable, the bundle keeps
/// its key (it still has entries, so retirement has not fired), and once the hand edit is reverted
/// an ordinary sweep removes the entry — the eventual cleanup, restored.
#[test]
fn a_drifted_entry_outlives_the_record_and_is_still_cleaned_up_later() {
    let fs = RealFs;
    let home = Scratch::new("drift-outlives");
    let layout = Layout::new(&home.0.join(".topos"));
    let cursor = home.0.join(".cursor/mcp.json");
    let sid = crate::id::SkillId::parse("s_a").unwrap();
    // A bundle WITH a record of its own — the case the detach exists for.
    std::fs::create_dir_all(layout.skill_dir(&sid)).unwrap();

    let io = ScopeIo {
        fs: &fs,
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
    d.reach = Some(vec!["cursor".into()]);
    mcp_engine::converge(
        &io,
        &plan(&io, vec![d.clone()]),
        &synthetic(),
        &all_slugs(),
        &no_hold(),
        true,
    );
    let pristine = std::fs::read_to_string(&cursor).unwrap();
    assert!(pristine.contains("topos-eng-alpha"));
    assert!(
        !crate::config_custody::entries_of(&fs, &layout, "s_a").is_empty(),
        "the row lives in the record while the record lives"
    );

    // The person hand-edits the entry: it is now DRIFTED and will never be clobbered.
    std::fs::write(
        &cursor,
        pristine.replace("mcp.example/a", "mcp.example/hand"),
    )
    .unwrap();

    // The classic remove: converge the removal, move what survived, then delete the record.
    let out = mcp_engine::remove_bundle(&io, &synthetic(), &all_slugs(), "s_a", "a");
    assert!(
        out.removed
            .iter()
            .any(|r| r.state.state == TargetOutcome::Drifted),
        "the hand-edited entry is left in place and disclosed: {out:?}"
    );
    // A detach that CANNOT land must say so: losing custody of a drifted row is the person's
    // business, not a silent fact. The faulted attempt reports, and the warning is the shape the
    // receipt folds into its lines.
    {
        let fault = FaultFs::new(1);
        let faulted = ScopeIo {
            fs: &fault,
            layout: &layout,
            home: home.0.clone(),
            project_root: None,
        };
        let warnings = mcp_engine::detach_bundle_rows(&faulted, "s_a");
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("MCP_CUSTODY_WRITE_FAILED")
                    || w.contains("MCP_LOCK_UNAVAILABLE")),
            "a detach that cannot land must report it: {warnings:?}"
        );
    }

    mcp_engine::detach_bundle_rows(&io, "s_a");
    std::fs::remove_dir_all(layout.skill_dir(&sid)).unwrap();

    // The row outlived its record, in the scope document, under the same bundle identity.
    let doc = crate::config_custody::read(&fs, &layout).unwrap();
    assert!(
        doc.unrecorded.contains_key("s_a"),
        "the surviving row moved out of the record before it was deleted: {doc:?}"
    );
    assert!(
        doc.keys.contains_key("s_a"),
        "the key is NOT retired while an entry of its still stands: {doc:?}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("mcp.example/hand"),
        "the hand edit is untouched"
    );

    // A later sweep with the bundle undemanded still SEES it — and still leaves the drift alone.
    let out = mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    assert!(
        out.removed
            .iter()
            .any(|r| r.bundle_id == "s_a" && r.state.state == TargetOutcome::Drifted),
        "a sweep still discloses the entry it can no longer remove: {out:?}"
    );
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("mcp.example/hand")
    );

    // The person reverts the hand edit. Now the entry matches what topos recorded, so the next
    // ordinary sweep takes it out — the cleanup that the lost row would have made impossible.
    std::fs::write(&cursor, &pristine).unwrap();
    mcp_engine::converge(&io, &[], &synthetic(), &all_slugs(), &no_hold(), true);
    assert!(
        !std::fs::read_to_string(&cursor)
            .unwrap_or_default()
            .contains("topos-eng-alpha"),
        "the reverted entry is finally removed"
    );
    let doc = crate::config_custody::read(&fs, &layout).unwrap();
    assert!(!doc.unrecorded.contains_key("s_a"), "{doc:?}");
    assert_eq!(
        doc.retired.get("topos-eng-alpha").map(String::as_str),
        Some("s_a"),
        "and only now does the key retire: {doc:?}"
    );
}

/// THE JOURNAL MUST DESCRIBE EXACTLY THE WORK THAT HAS NOT LANDED. A config write lands, then the
/// write of the bundle's own custody document FAILS. The promotion is therefore only in memory —
/// so the intents that produced it are still outstanding, and the scope document written on that
/// failure path must still carry them. Clearing the journal there would strand a live entry with
/// no record, permanently: every later run reads it as a hand edit and refuses to touch it.
///
/// Sweeps the fault point to find the exact ordering (config landed, record not), then asserts the
/// journal survived on disk and that a clean re-run's recovery promotes the row.
#[test]
fn a_failed_record_write_keeps_its_intents_in_the_durable_journal() {
    let probe = {
        let home = Scratch::new("keep-intent-probe");
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
        d.reach = Some(vec!["cursor".into()]);
        mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        fault.ops_attempted()
    };
    assert!(probe > 0);

    let mut proven = 0usize;
    for fail_at in 1..=probe {
        let home = Scratch::new(&format!("keep-intent-{fail_at}"));
        let layout = Layout::new(&home.0.join(".topos"));
        let cursor = home.0.join(".cursor/mcp.json");
        let mut d = demand(
            "s_a",
            "alpha",
            Some("eng"),
            &server_json("https://mcp.example/a"),
        );
        d.reach = Some(vec!["cursor".into()]);
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
                &plan(&io, vec![d.clone()]),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        // The ordering this test is about: the ENTRY is in the config file, but the bundle's own
        // custody document does not record it.
        let landed = std::fs::read_to_string(&cursor)
            .map(|t| t.contains("topos-eng-alpha"))
            .unwrap_or(false);
        let fs = RealFs;
        let recorded = !crate::config_custody::entries_of(&fs, &layout, "s_a").is_empty();
        if !landed || recorded {
            continue;
        }
        proven += 1;
        // THE INVARIANT: the journal on disk still describes the unlanded work.
        let doc = crate::config_custody::read(&fs, &layout).expect("the scope doc is decipherable");
        assert!(
            !doc.pending.is_empty(),
            "fail_at={fail_at}: the config write landed and the record did not — the journal must \
             still carry the intent, or the entry is stranded forever"
        );
        // And recovery finishes it: a clean re-run promotes the row into the record.
        {
            let io = ScopeIo {
                fs: &fs,
                layout: &layout,
                home: home.0.clone(),
                project_root: None,
            };
            mcp_engine::converge(
                &io,
                &plan(&io, vec![d.clone()]),
                &synthetic(),
                &all_slugs(),
                &no_hold(),
                true,
            );
        }
        assert!(
            !crate::config_custody::entries_of(&fs, &layout, "s_a").is_empty(),
            "fail_at={fail_at}: recovery must promote the journaled row"
        );
        assert!(
            crate::config_custody::read(&fs, &layout)
                .unwrap()
                .pending
                .is_empty(),
            "fail_at={fail_at}: and clear the journal once it has"
        );
    }
    assert!(
        proven > 0,
        "no fault point produced the landed-config / unrecorded-custody ordering this guards"
    );
}

#[test]
fn a_fault_at_any_write_never_tears_state_and_the_next_converge_heals() {
    // The fault sweep through the fs seam: fail exactly one mutating op, at every op the converge
    // performs, and prove the invariant pair — nothing torn (the custody stays decipherable), and
    // a clean re-run always ends fully placed with a file-matching custody.
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
        d.reach = Some(vec!["cursor".into()]);
        mcp_engine::converge(
            &io,
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
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
        d.reach = Some(vec!["cursor".into()]);
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
                &plan(&io, vec![d.clone()]),
                &synthetic(),
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
            &plan(&io, vec![d.clone()]),
            &synthetic(),
            &all_slugs(),
            &no_hold(),
            true,
        );
        // Fully placed either way: `placed` where this clean re-run did the writing, `current`
        // where the faulted run had already landed the entry and recovery promoted it. The
        // file-vs-custody check below is what proves the placement itself.
        let st = state_of(&out, "s_a", "cursor").state;
        assert!(
            st.wrote() || st == TargetOutcome::Current,
            "fail_at={fail_at}: {out:?}"
        );
        let bytes = std::fs::read(home.0.join(".cursor/mcp.json")).unwrap();
        let observed = mcp::observe(McpDialect::CursorJson, Some(&bytes));
        let custody = ScopeEntries::load(&fs, &layout).unwrap();
        assert!(custody.doc.pending.is_empty(), "fail_at={fail_at}");
        assert_eq!(
            observed.entries.get("topos-eng-alpha"),
            Some(
                &custody
                    .row(&config_custody::placement_key("cursor", "topos-eng-alpha"))
                    .unwrap()
                    .fingerprint
            ),
            "fail_at={fail_at}: the healed custody matches the file"
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

/// The wave-1 config files whose REAL user surfaces are Home-rooted (hermetic under a fake
/// `$HOME`) — the dest narrowing every machine-scope fixture rides.
const SAFE: &str = "dest = [\"~/.cursor/mcp.json\", \"~/.openclaw/openclaw.json\"]";

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
    // The explicit row delivers; its `dest` names the hermetic config files.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
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
        row.harnesses.iter().all(|h| h.state.wrote()),
        "a fresh placement: this run wrote every one of them — {row:?}"
    );

    // The applied report carried the states over the wire — as the STANDING answer both it and
    // the cache exist to give: where the entries live, one word for a file whatever sweep last
    // touched it (`placed` is the RUN's own word, and stays on the receipt above).
    let reported = plane.reported.lock().unwrap().clone();
    let (_, version, harnesses) = reported
        .iter()
        .find(|(id, ..)| id == "s_linear")
        .unwrap_or_else(|| panic!("reported: {reported:?}"));
    assert_eq!(version, &topos_core::digest::to_hex(&v.id));
    assert_eq!(harnesses.len(), 2, "{harnesses:?}");
    assert!(
        harnesses.iter().all(|h| h.state == TargetOutcome::Current),
        "the fleet's standing picture: {harnesses:?}"
    );

    // The offline cache carries the kind + the same per-agent standing states.
    let cache = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    let ds = &cache.workspaces[WS].delivered["s_linear"];
    assert_eq!(ds.kind.as_deref(), Some("mcp"));
    assert_eq!(ds.harness_states.len(), 2, "{ds:?}");
    assert!(
        ds.harness_states
            .iter()
            .all(|h| h.state == TargetOutcome::Current),
        "{ds:?}"
    );

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

/// ITEM (the subscribe receipt's typed block): `topos add` of a workspace mcp bundle — the same
/// act a workspace mcp reference rides — delivers in the same invocation and folds
/// the typed `mcp` block from the document that LANDED: the embedded identity, the endpoint,
/// the narrowed agents — and NO `bundle` folder, because a workspace bundle's bytes live in the
/// store.
#[test]
fn a_workspace_mcp_subscribe_receipt_carries_the_typed_block() {
    // PROJECT scope, so the breadth is hermetic without narrowing (a fresh subscribe row spells
    // no `dest`, and project surfaces are checkout-relative whatever the dev env sets): the four
    // project-capable agents engage deterministically — claude-code + cursor via detection under
    // the fake home, codex + opencode via their seeded project files.
    let rig = Rig::new("sub-receipt");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    std::fs::create_dir_all(rig.home.0.join(".claude")).unwrap();
    let proj = Scratch::new("sub-receipt-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::create_dir_all(proj.0.join(".codex")).unwrap();
    std::fs::write(proj.0.join(".codex/config.toml"), b"").unwrap();
    std::fs::write(proj.0.join("opencode.json"), b"").unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
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
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));

    let outcome = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("{HOST}/{WS_NAME}/linear"),
        false,
        false,
        &Default::default(),
        None,
    )
    .unwrap();
    let ops::AddRefOutcome::Applied(data) = outcome else {
        panic!("a workspace subscribe applies");
    };
    let mcp = data.mcp.expect("the receipt carries the typed block");
    assert_eq!(
        mcp.server, "io.test/x",
        "the EMBEDDED identity, not the catalog name"
    );
    assert_eq!(mcp.url, "https://mcp.example/linear");
    assert_eq!(mcp.bundle, None, "no folder — the bytes live in the store");
    assert_eq!(
        mcp.agents.len(),
        4,
        "the project-scope breadth line: {:?}",
        mcp.agents
    );
    let note = data.note.clone().unwrap_or_default();
    assert!(note.contains("MCP server io.test/x"), "{note}");
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
        "[bundles]\n\"{HOST}/{WS_NAME}/alpha\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists());

    // The network dies AND the entry is lost locally (the file deleted by hand): the next sweep
    // converges from the STORE's held bytes + the custody — no dial needed.
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
            .any(|h| h.agent == "cursor" && h.state.wrote()),
        "the repaired entry says this run wrote it: {row:?}"
    );
}

/// THE RECEIPT'S OWN VERB, AND WHAT IT COUNTS. A run that puts a hand-deleted config entry back
/// CHANGED this machine, and the header must say so: the store never moved (the sync reads "up to
/// date"), so a header built from the sync's answer alone said `checked` over a run that rewrote a
/// person's agent config. A genuinely idle sweep still says `checked`, and the repaired row reads
/// as the ordinary catch-up it is.
///
/// The row is then held to ONE truth on both channels, over TWO config files of which this run
/// wrote exactly one: the destination column and `destinations` name that ONE file (never the one
/// that merely already held the entry), the file list prints ONCE (the per-agent lines carry it,
/// and say what happened in each), and the JSON tells the two files apart exactly as the TTY does
/// — `placed` beside `current`, never one word for both.
#[test]
fn a_repaired_config_entry_makes_the_run_an_update_not_a_check() {
    let rig = Rig::new("repair");
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
    // TWO hermetic config files: only one of them is deleted below, so the receipt has something
    // to be wrong about.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/alpha\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let tty = |out: &ops::PullOutcome| {
        crate::render::pull_tty(
            &out.data,
            &out.decisions,
            &out.warnings,
            &out.advisories,
            &out.disclosures,
            out.failed_bundles.len(),
        )
    };
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists());

    // A sweep that finds everything in place only LOOKED.
    let idle = tty(&sweep(&ctx, &plane, &dir));
    assert!(idle.starts_with("checked "), "{idle}");

    // ONE entry is deleted by hand; the next sweep writes THAT ONE back.
    std::fs::remove_file(&cursor).unwrap();
    let out = sweep(&ctx, &plane, &dir);
    assert!(cursor.exists(), "the config was not repaired");
    let row = out.data.skills.iter().find(|s| s.skill == "alpha").unwrap();
    assert_eq!(
        row.action,
        topos_types::results::PullAction::Refreshed,
        "{row:?}"
    );
    // THE ROW BLOCK, byte for byte. The column counts the config files it HEADS — the same two
    // its detail lines name — never the subset this run wrote: a count over a longer list is a
    // number the reader has to reconcile with the lines under it. Which file moved is the lines'
    // own job, and they say it.
    assert_eq!(
        tty(&out),
        "updated machine-wide\n\
         alpha   updated (2 config files)\n    \
             ~/.openclaw/openclaw.json: unchanged\n    \
             ~/.cursor/mcp.json: created — restart Cursor\n\
         Checked 1 bundle: 1 updated."
    );
    // The wire says the same thing the receipt does: the written file, and only it, is the
    // destination — and the two files' states differ.
    assert_eq!(row.destinations, vec!["~/.cursor/mcp.json".to_owned()]);
    let states: Vec<(&str, TargetOutcome)> = row
        .harnesses
        .iter()
        .map(|h| (h.agent.as_str(), h.state))
        .collect();
    assert_eq!(
        states,
        vec![
            ("openclaw", TargetOutcome::Current),
            // The entry was hand-DELETED, so this run put one where none stood: `created` at the
            // target, while the ROW above is still an `updated` — the bundle was already
            // installed here. Two levels, both true.
            ("cursor", TargetOutcome::Created),
        ],
        "the rewritten file and the merely-found one are distinguishable: {row:?}"
    );
}

/// A FIRST-EVER PLACEMENT IS AN INSTALL. A brand-new `kind = "mcp"` manifest line syncs
/// store-only — nothing to place, so the sync answers `up to date` — and the converge then writes
/// the config entries for the first time. That is not a repair of anything: the row must lead
/// with `+` and read `installed`, exactly as a delivered mcp bundle's first row does. The scope's
/// custody is the durable signal, so the SAME bundle re-healed later reads `updated` instead.
#[test]
fn a_first_ever_mcp_placement_reads_installed_not_a_repair() {
    let rig = Rig::new("first");
    seed_harness_dirs(&rig.home.0);
    let src = rig.home.0.join("weather");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("server.json"),
        server_json("https://w.example/mcp").as_bytes(),
    )
    .unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = {{ kind = \"mcp\", {SAFE} }}\n",
        src.display()
    ));
    let plane = FakePlane::new();
    let dir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let tty = |out: &ops::PullOutcome| {
        crate::render::pull_tty(
            &out.data,
            &out.decisions,
            &out.warnings,
            &out.advisories,
            &out.disclosures,
            out.failed_bundles.len(),
        )
    };

    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "weather")
        .unwrap();
    assert_eq!(
        row.action,
        topos_types::results::PullAction::Installed,
        "{row:?}"
    );
    assert_eq!(
        tty(&out),
        // Agent lines follow the ONE harness table's row order.
        "updated machine-wide\n\
         + weather   installed (2 config files)\n    \
             ~/.openclaw/openclaw.json: created — picked up automatically; sign in with \
             `openclaw mcp login <name>`\n    \
             ~/.cursor/mcp.json: created — restart Cursor\n\
         Checked 1 bundle: 1 installed."
    );

    // The next sweep has nothing to do at all, and a later re-heal of the SAME bundle is the
    // repair — `updated`, never a second install.
    assert!(tty(&sweep(&ctx, &plane, &dir)).starts_with("checked "));
    std::fs::remove_file(rig.home.0.join(".cursor/mcp.json")).unwrap();
    let healed = sweep(&ctx, &plane, &dir);
    let row = healed
        .data
        .skills
        .iter()
        .find(|s| s.skill == "weather")
        .unwrap();
    assert_eq!(
        row.action,
        topos_types::results::PullAction::Refreshed,
        "a bundle this scope had already placed re-heals as a repair: {row:?}"
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
    // The receipt says it as a `removed` ROW counting the config FILES the entries left —
    // destinations, never agents.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "alpha" && s.action == topos_types::results::PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert_eq!(row.kind.as_deref(), Some("mcp"));
    assert!(
        row.destinations
            .iter()
            .any(|d| d.ends_with(".cursor/mcp.json")),
        "{:?}",
        row.destinations
    );
    let custody = ScopeEntries::load(&rig.fs, &rig.layout()).unwrap();
    assert!(!custody.has_entries_for("s_a"));
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
        assert_eq!(st.state, TargetOutcome::Withheld, "{slug}");
        assert_eq!(
            st.note.as_deref(),
            Some("no project-level config"),
            "{slug}"
        );
    }
    // The custody lives in the PROJECT store.
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(playout.config_custody_path().exists());
    assert!(!rig.layout().config_custody_path().exists());
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
        format!("[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ version = \"*\", dest = [\".cursor/mcp.json\"] }}\n"),
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
            .any(|h| h.agent == "cursor" && h.state == TargetOutcome::Unprovable),
        "{row:?}"
    );
}

#[test]
fn a_rows_dest_files_narrow_the_placement_and_unknown_files_warn_once() {
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
    // BOTH hermetic agents are detected; the row's dest names cursor's file alone, plus a file
    // no harness claims.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/alpha\" = {{ version = \"*\", dest = [\"~/.cursor/mcp.json\", \"~/.notepad/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(rig.home.0.join(".cursor/mcp.json").exists());
    assert!(
        !rig.home.0.join(".openclaw/openclaw.json").exists(),
        "a dest row is frozen to the files it names — detection adds nothing"
    );
    // The line is an ADVISORY — the bundle itself delivered, so it must never join the counted
    // failure channel.
    assert!(
        !out.warnings.iter().any(|w| w.contains("MCP_DEST_UNKNOWN")),
        "an advisory about a delivered bundle is not a counted failure: {:?}",
        out.warnings
    );
    let unknown: Vec<&String> = out
        .advisories
        .iter()
        .filter(|w| w.contains("MCP_DEST_UNKNOWN"))
        .collect();
    assert_eq!(
        unknown.len(),
        1,
        "one warning per unknown file: {unknown:?}"
    );
    assert!(
        unknown[0].contains("`~/.notepad/mcp.json` is not a known MCP config file"),
        "{unknown:?}"
    );
    assert!(
        unknown[0].contains("~/.codex/config.toml"),
        "the refusal lists the known files: {unknown:?}"
    );
    // The warning's subject is the BUNDLE the row delivers — never the bare scope label standing
    // where the name belongs.
    assert!(
        unknown[0].contains("\"alpha\""),
        "the warning names the bundle: {unknown:?}"
    );
    // And the counts reconcile: ONE bundle was checked. A delivered bundle with a config nit is
    // not a second, failed bundle — the warning still prints, uncounted.
    let clean = sweep(&ctx, &plane, &dir);
    let receipt = crate::render::pull_tty(
        &clean.data,
        &clean.decisions,
        &clean.warnings,
        &clean.advisories,
        &clean.disclosures,
        clean.failed_bundles.len(),
    );
    assert!(
        receipt.contains("MCP_DEST_UNKNOWN"),
        "still warned: {receipt}"
    );
    assert!(
        receipt.contains("Checked 1 bundle: all up to date."),
        "{receipt}"
    );
    assert!(
        !receipt.contains("failed"),
        "a delivered bundle is not a failure: {receipt}"
    );
}

/// A dest row is FROZEN to what it names — so a row that names ONLY files no harness claims
/// (one typo) costs the bundle every agent. That is fail-closed and stays fail-closed: nothing is
/// placed anywhere. What must not stay is the SILENCE. It is a counted warning naming the entry
/// and the files that would have worked, the receipt row says the bundle reaches no agent instead
/// of printing a bare install, and `list` says the same rather than "no entries recorded yet" —
/// which would promise entries that are never coming.
#[test]
fn a_dest_naming_only_unknown_files_reaches_no_agent_and_says_so() {
    let rig = Rig::new("dest-none");
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
        "[bundles]\n\"{HOST}/{WS_NAME}/alpha\" = {{ version = \"*\", dest = [\"~/.codex/config.yaml\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // Fail-closed: not one config file was written, detected agents and all.
    assert!(!rig.home.0.join(".cursor/mcp.json").exists());
    assert!(!rig.home.0.join(".openclaw/openclaw.json").exists());
    assert!(!rig.home.0.join(".codex/config.toml").exists());

    // LOUD: the counted channel, not the advisory one — the bundle delivered nowhere.
    let loud: Vec<&String> = out
        .warnings
        .iter()
        .filter(|w| w.contains("MCP_DEST_NO_AGENT"))
        .collect();
    assert_eq!(loud.len(), 1, "{:?}", out.warnings);
    assert!(loud[0].contains("\"alpha\" reaches no agent"), "{loud:?}");
    assert!(
        loud[0].contains("\"~/.codex/config.yaml\" is not a known MCP config file"),
        "{loud:?}"
    );
    assert!(
        loud[0].contains("~/.codex/config.toml") && loud[0].contains("~/.cursor/mcp.json"),
        "the line teaches the files that would have worked: {loud:?}"
    );
    assert!(
        !out.advisories.iter().any(|w| w.contains("MCP_DEST")),
        "a bundle reaching nothing is never filed as an advisory: {:?}",
        out.advisories
    );

    // The ROW says it too — on the settled sweep, where the row is otherwise up to date and a
    // compact receipt would have dropped it entirely.
    let clean = sweep(&ctx, &plane, &dir);
    let receipt = crate::render::pull_tty(
        &clean.data,
        &clean.decisions,
        &clean.warnings,
        &clean.advisories,
        &clean.disclosures,
        clean.failed_bundles.len(),
    );
    assert!(
        receipt.contains("alpha") && receipt.contains("reaches no agent"),
        "{receipt}"
    );

    // And so does the deep dive, offline.
    let list = ops::list_with(
        &ctx,
        &ops::ListRequest {
            name: Some("alpha".into()),
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let detail = list.data.detail.clone().unwrap();
    assert!(
        detail
            .mcp_unreachable
            .as_deref()
            .is_some_and(|w| w.contains("~/.codex/config.yaml")),
        "{detail:?}"
    );
    let text = crate::render::list_tty(&list);
    assert!(
        text.contains("an MCP server bundle that reaches no agent"),
        "{text}"
    );
    assert!(
        !text.contains("no agent config entries recorded yet"),
        "nothing is coming — the line must not promise entries: {text}"
    );
}

/// **A live entry outranks the row's arithmetic.** An entry topos placed and then found
/// hand-edited is LEFT in place — drift is never clobbered — and it keeps its custody entry. If the
/// row's `dest` is later changed to files topos cannot edit, the reach arithmetic says "no agent"
/// while that entry is still sitting in the config, quite possibly still being loaded by the agent.
/// `list` must not tell that lie: the per-agent states say where the bytes actually are, and the
/// sweep's own MCP_DEST_NO_AGENT warning stays the causality carrier.
#[test]
fn a_drifted_entry_keeps_list_from_claiming_the_bundle_reaches_no_agent() {
    let rig = Rig::new("dest-none-drift");
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
        "[bundles]\n\"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");

    // The person edits the placed entry by hand.
    let edited = std::fs::read_to_string(&cursor)
        .unwrap()
        .replace("https://mcp.example/a", "https://my-fork.example");
    std::fs::write(&cursor, &edited).unwrap();

    // …and then the row's dest is changed to a file topos cannot edit. The sweep warns, and leaves
    // the hand edit exactly where it is.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.codex/config.yaml\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        out.warnings.iter().any(|w| w.contains("MCP_DEST_NO_AGENT")),
        "the causality still rides the sweep warning: {:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read_to_string(&cursor).unwrap(),
        edited,
        "the hand edit is never clobbered"
    );

    let list = ops::list_with(
        &ctx,
        &ops::ListRequest {
            name: Some("alpha".into()),
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let detail = list.data.detail.clone().unwrap();
    assert_eq!(
        detail.mcp_unreachable, None,
        "something of this bundle's is still in a config: {detail:?}"
    );
    assert!(
        detail.harnesses.iter().any(|h| h.agent == "cursor"),
        "the states name the agent whose config still holds it: {detail:?}"
    );
    let text = crate::render::list_tty(&list);
    assert!(
        !text.contains("reaches no agent"),
        "an entry still in a config file is not 'no agent': {text}"
    );
}

/// Two bundles, ONE typo'd spelling: the first still delivers (the typo is an advisory beside a
/// working row), the second reaches nothing. The once-per-run dedupe must not let the first one's
/// advisory swallow the second one's warning — they answer different questions about different
/// bundles, and the swallowed one is the one that means "this reaches nobody".
#[test]
fn one_bundles_dest_advisory_never_swallows_anothers_reaches_no_agent() {
    let rig = Rig::new("dest-dedupe");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let va = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/a").as_bytes(),
    )]);
    let vb = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/b").as_bytes(),
    )]);
    let plane = FakePlane::new()
        .with_version("s_a", &va)
        .with_version("s_b", &vb);
    let dir = FakeDirectory {
        skills: vec![
            mcp_catalog_entry("s_a", "alpha", &va),
            mcp_catalog_entry("s_b", "beta", &vb),
        ],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\
         \"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.cursor/mcp.json\", \"~/.typo/mcp.json\"] }}\n\
         \"{HOST}/{WS_NAME}/beta\" = {{ dest = [\"~/.typo/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert!(
        out.advisories
            .iter()
            .any(|w| w.contains("MCP_DEST_UNKNOWN") && w.contains("\"alpha\"")),
        "the delivering bundle's dropped entry stays an advisory: {:?}",
        out.advisories
    );
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("MCP_DEST_NO_AGENT") && w.contains("\"beta\" reaches no agent")),
        "the bundle reaching nothing is warned in its own right: {:?}",
        out.warnings
    );
}

/// A workspace SKILL row whose `dest` names a skills FOLDER — exactly what
/// `add -g @ws/skill -a codex` writes (`dest = ["~/.codex/skills"]`) — is not an MCP demand:
/// no MCP_DEST_UNKNOWN warning, no failure count, and the untouched update reads exactly clean.
#[test]
fn a_skill_rows_folder_dest_never_warns_mcp_dest_unknown() {
    let rig = Rig::new("skilldest");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[("SKILL.md", b"# deploy\n")]);
    let plane = FakePlane::new().with_version("s_dep", &v);
    let mut entry = mcp_catalog_entry("s_dep", "deploy", &v);
    entry.kind = "skill".into();
    let dir = FakeDirectory {
        skills: vec![entry],
        channels: Vec::new(),
    };
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ version = \"*\", dest = [\"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let first = sweep(&ctx, &plane, &dir);
    assert!(
        !first
            .warnings
            .iter()
            .chain(&first.advisories)
            .any(|w| w.contains("MCP_DEST_UNKNOWN")),
        "a skill row's folder dest is not an MCP config file: {:?} / {:?}",
        first.warnings,
        first.advisories
    );
    assert!(
        rig.home.0.join(".codex/skills/deploy/SKILL.md").exists(),
        "delivery itself lands"
    );
    // The second sweep is untouched — and wholly clean: one skill, up to date, nothing failed.
    let clean = sweep(&ctx, &plane, &dir);
    assert!(
        !clean
            .warnings
            .iter()
            .chain(&clean.advisories)
            .any(|w| w.contains("MCP_DEST_UNKNOWN")),
        "{:?} / {:?}",
        clean.warnings,
        clean.advisories
    );
    assert_eq!(
        crate::render::pull_tty(
            &clean.data,
            &clean.decisions,
            &clean.warnings,
            &clean.advisories,
            &clean.disclosures,
            0,
        ),
        "checked machine-wide\nChecked 1 skill: all up to date."
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
        "[bundles]\n\"{}\" = {{ kind = \"mcp\", dest = [\"~/.cursor/mcp.json\"] }}\n",
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
    // byte-identical (the credential never reaches it), and the custody keeps the entry.
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
            ScopeEntries::load(&rig.fs, &rig.layout())
                .unwrap()
                .has_entries_for("local:weather"),
            "{code}: the standing entry is held, not dropped"
        );
        // AND THE SUMMARY AGREES WITH THE STATUS. The refused bundle is a bundle that could not
        // be carried forward, so it is counted as one: the gate's line alone left the run exiting
        // non-zero while the receipt said "1 already up to date".
        assert_eq!(
            out.failed_bundles.len(),
            1,
            "{code}: the refused bundle is counted: {:?}",
            out.failed_bundles
        );
        let tty = crate::render::pull_tty(
            &out.data,
            &out.decisions,
            &out.warnings,
            &out.advisories,
            &out.disclosures,
            out.failed_bundles.len(),
        );
        assert!(
            !tty.contains("already up to date"),
            "{code}: a run that placed nothing never claims it was already current: {tty}"
        );
    }
}

/// INVISIBLE AND UNFIXABLE. An MCP bundle whose manifest row is hand-deleted while its config
/// entry carries a hand edit leaves a record nothing demands whose ENTRY is still live in the
/// agent's config — and neither surface said so: `list` builds its inventory from manifest rows,
/// and the sweep's orphan resolution deliberately passes over a record whose entries still stand
/// (they are placed, not abandoned). `update` read "all up to date" while the server sat in
/// Cursor's config. One line in `list` now names it and the command that ends it.
#[test]
fn an_orphaned_record_whose_entries_still_stand_gets_one_line_in_list() {
    let rig = Rig::new("orphan-visible");
    seed_harness_dirs(&rig.home.0);
    let plane = FakePlane::new();
    let fdir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    let dir = rig.home.0.join("wx");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        server_json("https://wx.example/mcp"),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // The ordinary door: the adopt mints the record AND places the entry.
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).unwrap();
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(cursor.exists(), "the entry landed");

    // The row goes; the ENTRY stays. A hand-edited entry is never destroyed, so its custody row
    // survives the removal — and a record that still holds entries is exactly what the sweep's
    // orphan pass declines to retire.
    let placed = std::fs::read_to_string(&cursor).unwrap();
    std::fs::write(&cursor, placed.replace("wx.example", "edited.example")).unwrap();
    rig.write_global("[bundles]\n");
    let _ = sweep(&ctx, &plane, &fdir);
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("edited.example"),
        "the hand-edited entry survives — that is what makes the record stick"
    );

    let listing = |ctx: &Ctx<'_>| {
        ops::list_with(
            ctx,
            &ops::ListRequest {
                view: ops::ScopeView::Machine,
                ..Default::default()
            },
            None,
            None,
            ops::RowPage::unlimited(),
        )
        .unwrap()
    };

    // THE LINE. One, naming the bundle, where it still stands, and the command that ends it.
    let listed = listing(&ctx);
    let orphans = &listed.data.scopes[0].orphans;
    assert_eq!(orphans.len(), 1, "{orphans:?}");
    assert_eq!(orphans[0].name, "wx");
    assert!(
        orphans[0].standing.iter().any(|p| p.contains("mcp.json")),
        "it names WHERE it still is: {orphans:?}"
    );
    let tty = crate::render::list_tty(&listed);
    assert!(
        tty.contains(
            "wx  no longer in this file — still in ~/.cursor/mcp.json (`topos remove wx` ends it)"
        ),
        "{tty}"
    );

    // AND THE WAY OUT WORKS — the whole point of naming a command. The record goes, and the line
    // goes with it: it reports a state, never a permanent mark.
    let named = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(fdir.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let dir_connect = |_: &str| -> Box<dyn DirectorySource> { Box::new(fdir.clone()) };
    let outcome = ops::remove(
        &ctx,
        &ops::RemoveConnectors {
            session: &named,
            directory: &dir_connect,
        },
        &["wx".to_owned()],
        &[],
        None,
        true,
    )
    .expect("the offered command is runnable");
    assert!(
        listing(&ctx).data.scopes[0].orphans.is_empty(),
        "{:?}",
        listing(&ctx).data.scopes[0].orphans
    );
    // AND IT COSTS NO BYTES OF THEIRS. The folder is the one the person named to `add` — topos
    // adopted it in place and never created it, so the record and the config entry are the whole
    // of what this command may end. A line that offered `remove` and deleted the source folder
    // would be the listing talking somebody into losing their own work.
    assert!(
        dir.is_dir() && dir.join("server.json").is_file(),
        "the adopted source folder survives the command the listing offered"
    );
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("--yes applies");
    };
    assert!(
        data.items[0].bytes_kept,
        "…and the receipt says so: {data:?}"
    );
    let tty = crate::render::remove_applied_tty(&data);
    assert!(
        tty.contains("stays") && tty.contains("wx"),
        "the receipt names the folder it left alone: {tty}"
    );
}

/// Adopt `name` as an MCP bundle rooted in the home dir, machine-wide — the ordinary door: a row,
/// a record, and a config entry in every detected agent.
fn adopt_mcp(rig: &Rig, ctx: &Ctx<'_>, name: &str) -> PathBuf {
    let dir = rig.home.0.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("server.json"),
        server_json(&format!("https://{name}.example/mcp")),
    )
    .unwrap();
    ops::add_mcp(ctx, dir.to_str().unwrap(), true, &Default::default()).unwrap();
    dir
}

/// A PAGE IS NOT A DEMAND. The orphan pass asks "does anything still demand this record?" and it
/// was asking the row vector the listing had already narrowed and CUT — so `--limit`/`--offset`,
/// which exist only to shorten output, turned every row that fell off the page into a record
/// reported as abandoned, under a line offering `topos remove <name>`. Following that on a
/// perfectly healthy bundle would have retired its config entries. The question is about the
/// scope's whole resolution now, and paging cannot reach it.
#[test]
fn paging_the_rows_never_invents_an_orphan() {
    let rig = Rig::new("orphan-paged");
    seed_harness_dirs(&rig.home.0);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    adopt_mcp(&rig, &ctx, "aaa");
    adopt_mcp(&rig, &ctx, "zzz");

    let listing = |page: ops::RowPage| {
        ops::list_with(
            &ctx,
            &ops::ListRequest {
                view: ops::ScopeView::Machine,
                ..Default::default()
            },
            None,
            None,
            page,
        )
        .unwrap()
    };
    // Both rows demanded, both records live: nothing is abandoned at any page size.
    let full = listing(ops::RowPage::unlimited());
    assert_eq!(full.data.scopes[0].rows.len(), 2, "{:?}", full.data.scopes);
    assert!(
        full.data.scopes[0].orphans.is_empty(),
        "{:?}",
        full.data.scopes[0].orphans
    );
    for page in [
        ops::RowPage {
            offset: 0,
            limit: Some(1),
        },
        ops::RowPage {
            offset: 1,
            limit: Some(1),
        },
    ] {
        let listed = listing(page);
        assert_eq!(listed.data.scopes[0].rows.len(), 1, "the page cut one row");
        assert!(
            listed.data.scopes[0].orphans.is_empty(),
            "the row that fell off the page is still demanded, not abandoned: {:?}",
            listed.data.scopes[0].orphans
        );
    }
}

/// A NAME IS NOT A RECORD. Two non-retired records in one scope can carry one display name — a
/// workspace copy beside a local one is the everyday case — and the orphan pass suppressed by
/// name, so a healthy `wx` silenced an ABANDONED `wx` whose config entry was sitting live in
/// Cursor with nothing naming it. Suppression keys on the record the row actually resolved.
#[test]
fn a_same_named_healthy_record_never_silences_an_abandoned_one() {
    let rig = Rig::new("orphan-twin");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let mcp_dir = adopt_mcp(&rig, &ctx, "wx");
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    std::fs::write(&cursor, placed.replace("wx.example", "edited.example")).unwrap();

    // The local MCP row goes — its record is now abandoned with a live, hand-edited entry. A
    // WORKSPACE bundle of the same name is demanded in its place; its row names its OWN record
    // (the delivery carries the id), which is the whole point: one name, two records.
    let v = mk_version(&[("SKILL.md", b"# wx\n")]);
    let plane = FakePlane::new().with_version("s_wx", &v);
    let mut ds = delivered_mcp("s_wx", "wx", &v);
    ds.kind = "skill".into();
    plane.serves(vec![ds]);
    let mut ce = mcp_catalog_entry("s_wx", "wx", &v);
    ce.kind = "skill".into();
    let fdir = FakeDirectory {
        skills: vec![ce],
        channels: Vec::new(),
    };
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/wx\" = \"*\"\n"));
    let _ = sweep(&ctx, &plane, &fdir);

    let listed = ops::list_with(
        &ctx,
        &ops::ListRequest {
            view: ops::ScopeView::Machine,
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let scope = &listed.data.scopes[0];
    assert!(
        scope.rows.iter().any(|r| r.skill == "wx"),
        "the healthy twin is demanded: {:?}",
        scope.rows
    );
    let orphans = &scope.orphans;
    assert_eq!(
        orphans.len(),
        1,
        "the abandoned twin earns its own line: {orphans:?}"
    );
    assert_eq!(orphans[0].name, "wx");
    assert!(
        orphans[0].standing.iter().any(|p| p.contains("mcp.json")),
        "it names the live entry: {orphans:?}"
    );
    assert!(
        mcp_dir.is_dir(),
        "the adopted folder is untouched by a read"
    );
}

/// THE OFFERED COMMAND HAS TO RUN — and hit the record the line was about. A project store's
/// orphan printed `topos remove <name>` like any other, but the classic arm resolved names in the
/// HOME store alone: from inside the checkout the command answered "no such skill", and on a
/// machine that happened to hold a same-named record it described THAT one instead — a listing
/// line about one bundle offering a delete of another. `remove` resolves where you stand now, and
/// runs every per-record read and write against the store that answered.
#[test]
fn a_project_orphans_offered_command_reaches_the_project_record() {
    let rig = Rig::new("orphan-proj");
    seed_harness_dirs(&rig.home.0);
    rig.write_global("[bundles]\n");
    let proj = Scratch::new("orphan-proj-checkout");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));

    // A MACHINE record of the same name stands beside it — the twin the home-only resolver used
    // to answer with. Its row is dropped so no demand-guard stands between the test and the
    // resolution being proven; the RECORD is what must come through untouched.
    let machine_dir = adopt_mcp(&rig, &ctx, "wx");
    let machine_records: Vec<PathBuf> = std::fs::read_dir(rig.layout().skills_dir())
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(machine_records.len(), 1, "{machine_records:?}");
    rig.write_global("[bundles]\n");

    // The project adopt: the checkout's own store, its own row, its own config entry.
    let proj_dir = proj.0.join("wx");
    std::fs::create_dir_all(&proj_dir).unwrap();
    std::fs::write(
        proj_dir.join("server.json"),
        server_json("https://wx-proj.example/mcp"),
    )
    .unwrap();
    ops::add_mcp(&ctx, proj_dir.to_str().unwrap(), false, &Default::default())
        .expect("project adopt");
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the project adopt minted the checkout's store");
    let cursor = proj.0.join(".cursor/mcp.json");
    let placed = std::fs::read_to_string(&cursor).unwrap();
    std::fs::write(&cursor, placed.replace("wx-proj.example", "edited.example")).unwrap();

    // The project row goes; the hand-edited entry stays, so the record sticks — an orphan.
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    let plane = FakePlane::new();
    let fdir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    let _ = sweep(&ctx, &plane, &fdir);

    let listing = || {
        ops::list_with(
            &ctx,
            &ops::ListRequest {
                view: ops::ScopeView::Here,
                ..Default::default()
            },
            None,
            None,
            ops::RowPage::unlimited(),
        )
        .unwrap()
    };
    let listed = listing();
    let project_scope = listed
        .data
        .scopes
        .iter()
        .find(|s| s.scope == "project")
        .expect("a project scope");
    assert_eq!(
        project_scope.orphans.len(),
        1,
        "{:?}",
        project_scope.orphans
    );
    assert_eq!(project_scope.orphans[0].name, "wx");
    let tty = crate::render::list_tty(&listed);
    assert!(tty.contains("`topos remove wx` ends it"), "{tty}");

    // RUN EXACTLY WHAT IT OFFERED, standing where the listing was read.
    let named = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(fdir.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let dir_connect = |_: &str| -> Box<dyn DirectorySource> { Box::new(fdir.clone()) };
    ops::remove(
        &ctx,
        &ops::RemoveConnectors {
            session: &named,
            directory: &dir_connect,
        },
        &["wx".to_owned()],
        &[],
        None,
        true,
    )
    .expect("the offered command is runnable from inside the checkout");

    // The PROJECT record is what ended.
    let after = listing();
    let project_scope = after
        .data
        .scopes
        .iter()
        .find(|s| s.scope == "project")
        .expect("a project scope");
    assert!(
        project_scope.orphans.is_empty(),
        "{:?}",
        project_scope.orphans
    );
    assert!(
        std::fs::read_dir(playout.skills_dir())
            .map(|d| d.count())
            .unwrap_or(0)
            == 0,
        "the project store's record is gone"
    );
    // Both folders are the person's own; neither was ever topos's to delete.
    assert!(proj_dir.is_dir() && machine_dir.is_dir());
    // And the MACHINE twin — the record the home-only resolver would have hit — still stands.
    assert!(
        machine_records.iter().all(|p| p.is_dir()),
        "the machine record was not the one deleted: {machine_records:?}"
    );
}

/// A repo row tagged `kind = "mcp"` never becomes a demand: the grammar refuses it when the file
/// LOADS, exactly like any other field the shape does not take — so the update refuses with the
/// teaching instead of syncing a row whose bytes no config converge could ever place.
#[test]
fn a_github_sourced_mcp_row_refuses_when_the_manifest_loads() {
    let rig = Rig::new("ghmcp");
    rig.seed_session();
    let plane = FakePlane::new();
    let dir = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    rig.write_global("[bundles]\n\"github.com/o/r/tool\" = { version = \"*\", kind = \"mcp\" }\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("`github.com/o/r/tool` cannot deliver an MCP server")
            && msg.contains("publish the bundle to a workspace"),
        "{msg}"
    );
}

/// F6: the SAME folder adopted in BOTH scopes must keep each scope's config key stable. The
/// reconcile resolves a local row's custody identity against THE SCOPE'S OWN store — never the
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
    ops::add_mcp(&ctx, dir.to_str().unwrap(), false, &Default::default()).expect("project adopt");
    ops::add_mcp(&ctx, dir.to_str().unwrap(), true, &Default::default()).expect("global adopt");
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the project adopt minted the checkout's store");
    let before = ScopeEntries::load(&rig.fs, &playout).unwrap();
    assert_eq!(before.doc.keys.len(), 1, "{before:?}");
    let (proj_bundle, proj_key) = before.doc.keys.iter().next().unwrap();
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
    let after = ScopeEntries::load(&rig.fs, &playout).unwrap();
    assert_eq!(
        after.doc.keys.get(&proj_bundle),
        Some(&proj_key),
        "the project scope's key must survive the sweep: {after:?}"
    );
    assert!(
        after.doc.retired.is_empty(),
        "nothing was retired: {after:?}"
    );
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
        &Default::default(),
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
    let custody = ScopeEntries::load(&rig.fs, &rig.layout()).unwrap();
    assert!(!custody.has_entries_for("s_a"));
    assert_eq!(
        custody
            .doc
            .retired
            .get("topos-eng-alpha")
            .map(String::as_str),
        Some("s_a")
    );
}

// =================================================================================================
// The durable kind marker: classification survives a lost custody, fails closed without evidence,
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
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
    ));
    (plane, dir)
}

/// ITEM PAIR (kind fails durable): the store + empty map stand, the LEDGER is gone — a targeted
/// go-back must still classify the record as config-placed and materialize NO skill dirs. Before
/// the fix, classification hung on the custody alone: its loss let the skill planner run and
/// `server.json` landed in skill dirs.
#[test]
fn a_lost_ledger_never_lets_a_targeted_go_back_materialize_skill_dirs() {
    let rig = Rig::new("lost-custody");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let (plane, dir) = deliver_linear(&rig, &v);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    // The failure shape: the custody is gone, and so is the delivery cache — the MARKER alone must
    // answer.
    std::fs::remove_file(rig.layout().config_custody_path()).unwrap();
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

/// ITEM PAIR (fail closed): with the map EMPTY and every kind source gone — marker, cache, custody
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
    std::fs::remove_file(rig.layout().config_custody_path()).unwrap();
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

/// THE TWO-DOMAIN RACE, made single-process. A bundle's DIR custody (`map.json`) and its CONFIG
/// custody (`entries.json`) are written under DIFFERENT locks — the per-skill flock and the scope's
/// mcp lock — and a targeted verb legitimately holds the skill lock across a converge, so the two
/// can never be made to serialize against each other without deadlocking. Safety therefore comes
/// from the FILES, not the locks: one writer each.
///
/// This drives the interleave that proves it, in one process because the locks no longer share a
/// document: a targeted go-back reads dir custody (t0), a hook-fired quiet sweep converges config
/// custody in the window (t1), the go-back commits its dir custody from the t0 snapshot (t2), and
/// only then converges (t3). With both halves in ONE document, t2 writes back a snapshot taken
/// before t1 and silently drops the rows t1 committed — leaving a live entry with no record, which
/// the go-back's converge reads as a hand edit and refuses to touch: a permanent drift standoff
/// that no later sweep clears. With the halves in sibling files, t2 cannot reach the rows: the
/// entry stays provably topos's, current at v2, and is REPLACED with v1.
#[test]
fn a_go_back_racing_a_sweep_never_lands_in_a_drift_standoff() {
    let rig = Rig::new("standoff");
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
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    // The team moves to v2 and the sweep converges onto it.
    let mut served2 = delivered_mcp("s_linear", "linear", &v2);
    served2.generation = 2;
    plane.serves(vec![served2]);
    let mut entry2 = mcp_catalog_entry("s_linear", "linear", &v2);
    entry2.generation = 2;
    let dir2 = FakeDirectory {
        skills: vec![entry2],
        channels: Vec::new(),
    };
    sweep(&ctx, &plane, &dir2);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v2")
    );

    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let map_path = rig.layout().published(&sid).map;

    // t0 — the snapshot a targeted verb reads before it converges.
    let stale_map = crate::doc::read_map(&rig.fs, &map_path).unwrap().unwrap();

    // t1 — the quiet sweep lands in the window. The person had deleted the entry by hand, so this
    // run REWRITES it and commits fresh config custody.
    std::fs::write(&cursor, "{\n  \"mcpServers\": {}\n}\n").unwrap();
    sweep(&ctx, &plane, &dir2);
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v2"),
        "the interleaved sweep re-placed the entry"
    );

    // t2 — the targeted verb commits DIR custody from its pre-sweep snapshot. This is the write
    // that used to take the config rows with it.
    crate::doc::write_map(&rig.fs, &map_path, &stale_map).unwrap();
    assert!(
        !crate::config_custody::entries_of(&rig.fs, &rig.layout(), "s_linear").is_empty(),
        "a dir-custody commit must not disturb config custody — the two have different writers"
    );

    // t3 — the go-back converges. The entry is topos's own, current at v2, so it is REPLACED.
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
        "the restored document must reach the config, not stand off against it: {text}"
    );
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .expect("the go-back row");
    let cursor_state = row
        .harnesses
        .iter()
        .find(|h| h.agent == "cursor")
        .expect("a cursor state");
    assert_ne!(
        cursor_state.state,
        TargetOutcome::Drifted,
        "an entry topos itself wrote must never read as a hand edit: {cursor_state:?}"
    );
    assert!(cursor_state.state.wrote(), "{cursor_state:?}");
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
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n"
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
/// only the harnesses that already hold a custody entry here, so a harness the narrowing excluded
/// never gains one. Before the fix the targeted demand carried an EMPTY narrowing, read as ALL
/// harnesses: openclaw (detected, engaged, excluded by the row) gained an entry the sweep then had
/// to claw back.
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
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"~/.cursor/mcp.json\"] }}\n"
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

/// A plane that serves ONE bundle's `current` pointer (the targeted-accept read) over the fake
/// version store — what `topos update <mcp-name>` dials when the pointer has advanced.
struct ServesCurrent {
    inner: FakePlane,
    record: topos_types::WireCurrentRecord,
}
impl PlaneSource for ServesCurrent {
    fn get_current(
        &self,
        skill_id: &str,
        _known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        if skill_id == self.record.scope.skill_id {
            Ok(PointerFetch::Record(self.record.clone()))
        } else {
            Err(PlaneError::NotFound)
        }
    }
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        self.inner.fetch_version(skill_id, version_id)
    }
}

/// ITEM PAIR (targeted accept converges): `topos update <mcp-name>` must not report success while
/// every agent config still carries the previous document. The accept advances the store AND
/// converges this scope's configs before returning, threading the per-agent states onto the row —
/// the same converge the go-back runs. Before the fix the targeted path gave an mcp record an
/// empty placement plan and left the configs stale until the next sweep.
#[test]
fn a_targeted_accept_updates_the_configs_before_reporting_success() {
    let rig = Rig::new("accept-converge");
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
    let (plane, dir) = deliver_linear(&rig, &v1);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("https://mcp.example/v1"),
        "the sweep placed v1"
    );

    // The pointer advanced to v2; the person runs the targeted accept.
    let serves = ServesCurrent {
        inner: FakePlane::new().with_version("s_linear", &v2),
        record: topos_types::WireCurrentRecord {
            schema_version: topos_types::WIRE_SCHEMA_VERSION,
            scope: topos_types::PointerScope {
                workspace_id: WS.to_owned(),
                skill_id: "s_linear".to_owned(),
            },
            record: topos_types::CurrentRecord {
                version_id: topos_core::digest::to_hex(&v2.id),
                generation: 2,
            },
        },
    };
    let follow = ops::CacheFollow::load(&rig.fs, &rig.layout());
    let ctx2 = Ctx {
        progress: crate::progress::silent(),
        fs: &rig.fs,
        ids: &rig.ids,
        clock: &rig.clock,
        device_id: "d_test".into(),
        layout: rig.layout(),
        harness: &rig.harness,
        triggers: crate::ops::Triggers::active_only(&rig.harness),
        plane: &serves,
        follow: &follow,
        roots: Some(crate::ctx::AgentRoots {
            home: rig.home.0.clone(),
            cwd: Some(rig.work.0.clone()),
        }),
    };
    let out = ops::pull(
        &ctx2,
        ops::PullScope::One {
            name: "linear".into(),
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
            store: ops::StoreScope::Here,
        },
    )
    .expect("the accept applies");

    // The configs carry v2 BEFORE the verb returned — and the row reports the per-agent states.
    let text = std::fs::read_to_string(&cursor).unwrap();
    assert!(
        text.contains("https://mcp.example/v2") && !text.contains("https://mcp.example/v1"),
        "the accept converged the configs: {text}"
    );
    let row = &out.data.skills[0];
    assert_eq!(row.action, topos_types::results::PullAction::FastForwarded);
    let agents: BTreeSet<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor", "openclaw"].into(), "{row:?}");
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
    std::fs::remove_file(rig.layout().config_custody_path()).unwrap();

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
    // Person scope demands nothing; the PROJECT manifest carries a feed row — illegal in a
    // project file, so that scope is FROZEN. The run drives the MACHINE scope (driving the
    // frozen project would refuse the run whole now), which syncs nothing: the delivered skill
    // has NO placement map anywhere, and the report must not claim it held.
    rig.write_global("[bundles]\n");
    std::fs::write(
        rig.work.0.join("topos.toml"),
        format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"),
    )
    .unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Machine,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();

    let reported = plane.reported.lock().unwrap().clone();
    assert!(
        !reported.iter().any(|(id, ..)| id == "s_alpha"),
        "an empty-map skill placed nowhere must not be reported held: {reported:?}"
    );
}

/// A delivered, store-only mcp bundle's bare `diff` REFUSES, naming what `diff` acts on.
///
/// It used to answer an empty diff whose endpoint was the held current. That was honest about the
/// working tree (there is none) and dishonest about everything a person reads it for: "No changes"
/// over a bundle the verb never looked at, printed identically whether or not the entry standing in
/// their agent's config had been hand-edited. The refusal names the kind and points at the verb
/// that does answer. (It also still never trips the "placement map has no placement" corruption
/// error a store-only record would otherwise reach.)
#[test]
fn a_bare_diff_of_a_config_placed_bundle_refuses_naming_the_kind() {
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

    let err = ops::diff(
        &ctx,
        "linear",
        None,
        ops::DiffBudget::resolve(None, true),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .expect_err("a config-placed bundle has no file diff");
    let detail = err.detail();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{detail}");
    assert!(
        detail.contains("'linear' is an MCP server bundle"),
        "{detail}"
    );
    assert!(detail.contains("`diff` compares"), "{detail}");
    assert!(detail.contains("`topos list linear`"), "{detail}");
    assert!(
        !detail.contains("placement map"),
        "never the corruption error: {detail}"
    );
}

/// **An offline sweep never WIDENS a dest-narrowed bundle.** A row's `dest` freezes which config
/// files it reaches; a run that cannot dial still heals those files from the store, and the
/// narrowing must survive the round-trip through the offline path — otherwise one lost network
/// call quietly fans a server out to every MCP-capable agent on the machine, with the person's
/// stated destinations overruled. Held with a feed row present too: the cached-delivery arm the
/// feed falls back to must not pick this bundle up behind its own row's back.
#[test]
fn an_offline_sweep_keeps_a_dest_narrowed_bundle_narrow() {
    let rig = Rig::new("offline-narrow");
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
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\
         \"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let elsewhere = [
        rig.home.0.join(".openclaw/openclaw.json"),
        rig.home.0.join(".codex/config.toml"),
        rig.home.0.join(".hermes/config.yaml"),
    ];
    assert!(cursor.exists(), "the named file got the entry");
    for p in &elsewhere {
        assert!(!p.exists(), "a dest row reaches only what it names: {p:?}");
    }

    // The network dies, and the entry is lost locally: the sweep heals it from the store — into
    // the SAME one file.
    std::fs::remove_file(&cursor).unwrap();
    plane.serve_unreachable();
    sweep(&ctx, &plane, &dir);
    assert!(
        std::fs::read_to_string(&cursor).is_ok_and(|t| t.contains("https://mcp.example/a")),
        "healed offline"
    );
    for p in &elsewhere {
        assert!(!p.exists(), "offline widened the row into {p:?}");
    }
}

/// THE ADD DOOR CLOSES ITS OWN WINDOW. Kind classification for a WORKSPACE bundle reads the store's
/// durable marker — the row itself never spells a kind — so `add` must not leave a row standing
/// with no marker for a later verb to guess about. It does not: the reference add DELIVERS in the
/// same invocation, and the sync that lands the bytes lays the marker before `add` returns.
#[test]
fn a_workspace_mcp_add_leaves_the_marker_behind_before_it_returns() {
    let rig = Rig::new("add-marker");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let proj = Scratch::new("add-marker-co");
    std::fs::create_dir_all(proj.0.join(".git")).unwrap();
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
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
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));

    ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("{HOST}/{WS_NAME}/linear"),
        false,
        false,
        &Default::default(),
        None,
    )
    .unwrap();

    // The PROJECT store the add delivered into carries the kind, durably, right now.
    let layout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the add created the project store");
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let marker = std::fs::read_to_string(layout.published(&sid).kind).expect("kind.json");
    assert!(marker.contains("\"mcp\""), "{marker}");

    // Which is the fact every later arm resolves against: the classifier answers mcp with no
    // delivery cache and no custody consulted.
    let sctx = crate::ops::ctx_with_layout(&ctx, &layout);
    assert!(
        crate::bundle_kind::classify(&sctx, "s_linear", &[]).is_mcp(),
        "the marker is what a later `remove -a <agent>` resolves the dest vocabulary from"
    );
}

/// AND THE WINDOW CANNOT BE OPENED BY A DARK PLANE EITHER. The pair that closes it structurally:
/// a workspace row's kind comes from the CATALOG, and the catalog is what `add` needs before it
/// writes anything — so there is no ordering in which a row exists with no marker beside it.
///
/// With the DELIVERY dark the explicit row still syncs (it resolves against the catalog, not the
/// feed snapshot), so the marker lands anyway; with the catalog answering NO the add refuses and
/// writes no row at all. A never-swept mcp row with no marker is therefore unreachable, and no arm
/// can fall back to the skills vocabulary for one.
#[test]
fn an_mcp_row_never_stands_without_its_marker_even_when_the_feed_is_dark() {
    let rig = Rig::new("dark-feed-marker");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_linear", &v);
    // The FEED is unreachable for the whole invocation.
    plane.serve_unreachable();
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_linear", "linear", &v)],
        channels: Vec::new(),
    };
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("{HOST}/{WS_NAME}/linear"),
        true,
        true,
        &Default::default(),
        None,
    )
    .expect("an explicit row resolves against the catalog, not the feed");

    let manifest =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("linear"), "the row landed: {manifest}");
    let sid = crate::id::SkillId::parse("s_linear").unwrap();
    let marker = std::fs::read_to_string(rig.layout().published(&sid).kind)
        .expect("and the marker landed with it");
    assert!(marker.contains("\"mcp\""), "{marker}");

    // The other half: with no catalog answer there is no row either, so "row without marker" has
    // no way to come about.
    let empty = FakeDirectory {
        skills: Vec::new(),
        channels: Vec::new(),
    };
    ops::add_reference(
        &ctx,
        &connect(&plane, &empty),
        None,
        &format!("{HOST}/{WS_NAME}/absent"),
        true,
        true,
        &Default::default(),
        None,
    )
    .expect_err("no catalog answer, no row");
    let manifest =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!manifest.contains("absent"), "{manifest}");
}

/// A BARE NAME IS NOT AN IDENTITY. Two records in one store can answer to `linear` — here a
/// workspace MCP bundle and a local skill folder of the same name. Asking the store for "linear"
/// answers AMBIGUOUS, and a row that falls back from that ambiguity resolves its `-a` selector
/// against the SKILLS-folder vocabulary: the wrong dest table for a config-placed bundle. The row
/// is qualified, so the lookup must be too.
#[test]
fn a_qualified_row_resolves_its_own_record_when_the_name_is_shared() {
    let rig = Rig::new("shared-name");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);

    // Record ONE: a local skill folder that happens to be called `linear`.
    let local = rig.work.0.join("linear");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("SKILL.md"), b"# linear\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    crate::ops::add(&ctx, &local).expect("the local skill adopts");

    // Record TWO: the workspace's MCP bundle, same name, delivered by its qualified row.
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_mcp", &v);
    plane.serves(vec![delivered_mcp("s_mcp", "linear", &v)]);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_mcp", "linear", &v)],
        channels: Vec::new(),
    };
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/linear\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    assert!(
        rig.home.0.join(".cursor/mcp.json").exists(),
        "the mcp bundle landed in a config file"
    );

    // The premise: TWO records in this one store answer to the name, so a bare-name lookup
    // resolves to neither and only the row's workspace can pick between them.
    let named = std::fs::read_dir(rig.layout().skills_dir())
        .unwrap()
        .filter_map(|e| {
            let sid = crate::id::SkillId::parse(&e.ok()?.file_name().to_string_lossy()).ok()?;
            crate::doc::read_doc::<topos_types::persisted::Lock>(
                &rig.fs,
                &rig.layout().published(&sid).lock,
            )
            .ok()
            .flatten()
            .filter(|l| l.name == "linear")
        })
        .count();
    assert_eq!(
        named, 2,
        "the test is meaningless unless both records answer to the name"
    );

    // Narrowing the QUALIFIED mcp row by `-a cursor` must resolve cursor's MCP CONFIG FILE. It is
    // the row's only destination, so the subtraction takes the whole row — and the receipt speaks
    // in config files, which is only reachable through the mcp vocabulary.
    let outcome = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/linear")],
        None,
        true,
        &ops::Selection::new(&["cursor".to_owned()], &[]),
    )
    .expect("the qualified row resolves its own record");
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("applies under --yes");
    };
    let shown = format!("{:?}", data.uninstalled);
    assert!(
        shown.contains("mcp.json"),
        "the mcp vocabulary resolved cursor to its config file: {shown}"
    );
}

// =================================================================================================
// The failure tally's key: `(scope label, bundle identity)`.
//
// Two bundles of one name are two bundles. Keyed by DISPLAY NAME the tally counted them once, and
// the converge fold's row stand-down — which matched name + scope — took a healthy twin's receipt
// row down beside the failed one's. Both fixtures below give every bundle an id UNLIKE its name on
// purpose: a fixture whose id IS its name passes with the bug still in.
// =================================================================================================

/// A SECOND connected workspace, on its own server — what makes two same-named bundles two
/// bundles rather than one name said twice.
fn seed_ops_session(rig: &Rig) {
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

/// Each workspace answers from its OWN catalog and delivery. The single-lane [`connect`] would
/// hand both sessions one catalog holding two entries of one name, which is an ambiguous catalog
/// — not two workspaces.
fn connect_lanes<'a>(
    lanes: &'a [(&'a str, FakePlane, FakeDirectory)],
) -> impl Fn(&Session) -> ops::SessionTransports + 'a {
    move |s: &Session| {
        let (_, plane, dir) = lanes
            .iter()
            .find(|(ws, ..)| *ws == s.workspace_id)
            .unwrap_or_else(|| panic!("no lane for {}", s.workspace_id));
        ops::SessionTransports {
            plane: Box::new(plane.clone()),
            directory: Box::new(dir.clone()),
            contribute: Box::new(NoContribute),
            governance: Box::new(NoGovernance),
        }
    }
}

fn sweep_lanes(ctx: &Ctx<'_>, lanes: &[(&str, FakePlane, FakeDirectory)]) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect_lanes(lanes),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap()
}

/// TWO WORKSPACES, ONE NAME, TWO FAILURES. Both teams publish an MCP server called `linear`, and
/// this run can place neither: each document names an http endpoint the gate refuses. The tally
/// counts what actually failed — two bundles — because it is keyed by the bundle identity the
/// sweep already joins on. Under the display name the second failure disappeared into the first
/// and the summary was short by one on every same-named pair.
#[test]
fn two_same_named_bundles_from_two_workspaces_both_failing_count_as_two() {
    let rig = Rig::new("tally-two-ws");
    rig.seed_session();
    seed_ops_session(&rig);
    seed_harness_dirs(&rig.home.0);
    let eng = mk_version(&[(
        "server.json",
        server_json("http://eng.example/linear").as_bytes(),
    )]);
    let ops_v = mk_version(&[(
        "server.json",
        server_json("http://ops.example/linear").as_bytes(),
    )]);
    let lanes = [
        (
            WS,
            FakePlane::new().with_version("s_lin_eng", &eng),
            FakeDirectory {
                skills: vec![mcp_catalog_entry("s_lin_eng", "linear", &eng)],
                channels: Vec::new(),
            },
        ),
        (
            "w_ops",
            FakePlane::new().with_version("s_lin_ops", &ops_v),
            FakeDirectory {
                skills: vec![mcp_catalog_entry("s_lin_ops", "linear", &ops_v)],
                channels: Vec::new(),
            },
        ),
    ];
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n\
         \"beta.test/ops/linear\" = {{ {SAFE} }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep_lanes(&ctx, &lanes);

    // BOTH refusals are on the record…
    assert_eq!(
        out.warnings
            .iter()
            .filter(|w| w.contains("MCP_INSECURE_URL"))
            .count(),
        2,
        "each bundle's own refusal: {:?}",
        out.warnings
    );
    // …and the tally holds two entries, one per IDENTITY, in the one scope both rows stand in.
    let failed: Vec<(&str, &str)> = out
        .failed_bundles
        .iter()
        .map(|(label, id)| (label.as_str(), id.as_str()))
        .collect();
    assert_eq!(
        failed,
        vec![("person", "s_lin_eng"), ("person", "s_lin_ops")],
        "two bundles failed, so the summary counts two"
    );
    // Nothing was placed for either, and neither stood-down row is left claiming otherwise.
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "linear"),
        "a bundle nothing was placed for keeps no row: {:?}",
        out.data.skills
    );
    let cursor = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap_or_default();
    assert!(!cursor.contains("linear"), "{cursor}");
}

/// A FAILURE STANDS DOWN ITS OWN ROW AND NOBODY ELSE'S. One scope holds two bundles called
/// `linear`: the workspace's, which places cleanly, and a local folder whose `server.json` the
/// gate refuses. The stand-down finds the failed bundle's row through the identity index, so the
/// healthy twin keeps its own — a name + scope match took both rows down and left the run
/// wordless about a bundle it had just placed.
#[test]
fn a_failed_bundle_stands_down_its_own_row_and_the_healthy_twin_keeps_its_row() {
    let rig = Rig::new("tally-twin");
    rig.seed_session();
    seed_harness_dirs(&rig.home.0);
    // The healthy one: the workspace's `linear`, over https.
    let v = mk_version(&[(
        "server.json",
        server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new().with_version("s_lin_ws", &v);
    let dir = FakeDirectory {
        skills: vec![mcp_catalog_entry("s_lin_ws", "linear", &v)],
        channels: Vec::new(),
    };
    // The failing one: a LOCAL folder of the same name whose document names an http endpoint.
    let local = rig.home.0.join("linear");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("server.json"),
        server_json("http://local.example/linear"),
    )
    .unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ {SAFE} }}\n\
         \"{}\" = {{ kind = \"mcp\", {SAFE} }}\n",
        local.display()
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // ONE failure, filed under the LOCAL bundle's identity — never the name the two share.
    let failed: Vec<(&str, &str)> = out
        .failed_bundles
        .iter()
        .map(|(label, id)| (label.as_str(), id.as_str()))
        .collect();
    assert_eq!(
        failed,
        vec![("person", "local:linear")],
        "the refused bundle is the one that is counted: {:?}",
        out.warnings
    );
    // The healthy twin keeps its receipt row, with the agents its entries reached.
    let rows: Vec<_> = out
        .data
        .skills
        .iter()
        .filter(|s| s.skill == "linear")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "one row stands down, the other stands: {:?}",
        out.data.skills
    );
    assert_eq!(
        rows[0].workspace_id.as_deref(),
        Some(WS),
        "the surviving row is the workspace bundle's: {:?}",
        rows[0]
    );
    let agents: BTreeSet<&str> = rows[0].harnesses.iter().map(|h| h.agent.as_str()).collect();
    assert_eq!(agents, ["cursor", "openclaw"].into(), "{:?}", rows[0]);
    // …and the disk agrees: the healthy entry landed, the refused one never did.
    let cursor = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap();
    assert!(
        cursor.contains("topos-eng-linear") && cursor.contains("https://mcp.example/linear"),
        "{cursor}"
    );
    assert!(!cursor.contains("http://local.example"), "{cursor}");
}
