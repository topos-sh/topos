//! Test-only public facade (feature `test-fixtures`) — drive the **real** pull engine over the **real**
//! `ureq` transport from an EXTERNAL integration crate (the HERO loopback), without exposing the client's
//! `pub(crate)` internals.
//!
//! Everything here is a thin wrapper over already-built, in-crate machinery: it lays down a `~/.topos/` for
//! a *never-pulled, followed* skill exactly as a real `add` + enrollment would (so the initial
//! `sync.json`/`lock.json`/`map.json` are produced by the genuine `ops::add`, never hand-faked), builds the
//! production `UreqPlane` + `FileFollow` + a real `Ctx`, and runs `ops::pull`. The HERO supplies the loopback
//! `base_url` and the minted read token; it asserts on the returned
//! [`topos_types::results::PullData`] plus
//! the placement bytes and `sync.json` read back through the accessors here.
//!
//! **It enables no new dependency** — the whole surface is client-internal types + `std` + the public
//! `topos-types`/`topos-core` leaves the client already links. The `test-fixtures` feature stays OFF in any
//! production build (a `check-arch` guard asserts it adds no `plane-store`/`sqlx`/`tokio` edge).

use std::collections::HashMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest::to_hex;
use topos_harness::{
    ClaudeCode, DiscoveredPlacement, HarnessAdapter, Hermes, OpenClaw, PlacementTarget,
};
use topos_types::persisted::{PlacementMap, SyncState};
use topos_types::requests::WireSkillIndex;
use topos_types::results::{
    DiffData, ListData, ProposeData, PublishData, PullData, RevertData, ReviewData,
};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::enroll::{self, FollowEntry, FollowModeDoc, Follows, Instance, Membership, UserDoc};
use crate::fs_seam::RealFs;
use crate::ids::{IdSource, RealClock, RealIds};
use crate::plane::{
    CatalogSource, ContributeSource, DirectorySource, EnrollSource, FollowContext, FollowMode,
    FollowSource, GovernanceSource, InertFollow, InertPlane, PlaneSource,
};
use crate::plane_http::{FileFollow, UreqDeviceClient, UreqPlane};
use crate::sidecar::Layout;
use crate::{doc, ops, scan};

/// The device id local commits (a never-shared draft snapshot) are authored under. Fixed + controlled-ASCII;
/// the plane never sees it (only the client's own genesis carries it), so any stable value works.
const DEVICE_ID: &str = "d_hero";

/// Extract the applied `RevertData` from the two-phase [`ops::RevertOutcome`] — the e2e facades drive
/// `revert` with `confirm = true` (apply), so a describe / byte-level no-op is an unexpected result the
/// facade surfaces as an error string.
fn revert_applied(outcome: ops::RevertOutcome) -> Result<RevertData, String> {
    match outcome {
        ops::RevertOutcome::Applied(data) => Ok(data),
        ops::RevertOutcome::NoOp(_) => {
            Err("revert was a byte-level no-op (the --to bytes already are current)".to_owned())
        }
        ops::RevertOutcome::Describe { .. } => {
            Err("revert returned a describe — pass --yes to apply".to_owned())
        }
    }
}

/// How a followed skill adopts a new `current` — the public face of the engine's internal `FollowMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Follow {
    /// Auto-apply a verified strictly-higher `current` (a standing follow).
    Auto,
    /// One-tap accept each new `current` (`--manual`).
    ConfirmEach,
}

impl Follow {
    fn to_mode(self) -> FollowMode {
        match self {
            Follow::Auto => FollowMode::Auto,
            Follow::ConfirmEach => FollowMode::ConfirmEach,
        }
    }
}

/// What a [`PullHarness::run_pull`] targets — the public face of the engine's internal `PullScope`.
#[derive(Debug, Clone)]
pub enum Scope {
    /// The bare session-start sweep over every followed skill.
    AllFollowed,
    /// A targeted accept/resume of one skill (by name).
    Accept { name: String },
    /// Go back to a specific local version (by name + the 64-char lowercase-hex version id).
    GoBack {
        name: String,
        version_id_hex: String,
    },
}

/// One install a describe/apply names: the catalog identity, the consent digest, and WHY it arrives —
/// the public face of the engine's internal `DescribedInstall`.
#[derive(Debug, Clone)]
pub struct InstallView {
    pub skill_id: String,
    pub name: String,
    pub version_id: Option<String>,
    pub bundle_digest: Option<String>,
    /// The channels delivering it (`everyone` included when it delivers).
    pub via_channels: Vec<String>,
    /// Whether it arrives as a direct follow.
    pub via_direct: bool,
}

/// The two-phase DESCRIBE a bare address subscribe answers with — the public face of `FollowDescribe`:
/// who you are here, the workspace address block, what `--yes` would land, and the standing disclosures.
#[derive(Debug, Clone)]
pub struct FollowDescribeView {
    pub workspace_id: String,
    /// The workspace ADDRESS name (the slug).
    pub workspace_name: String,
    /// The full workspace address (the share link — server-built).
    pub address: String,
    /// The caller's role on the roster.
    pub role: String,
    /// Who invited the caller (present for an invited member).
    pub invited_by: Option<String>,
    /// Whether THIS invocation enrolled the device.
    pub enrolled_now: bool,
    /// The subscribe targets (`(kind, name)` — workspace / channel / skill).
    pub targets: Vec<(String, String)>,
    /// The installs `--yes` would land (pending first-receives included).
    pub installs: Vec<InstallView>,
    /// Channels the person is already placed into (an inviter's pre-placement; `everyone` excluded).
    pub preplaced_channels: Vec<String>,
    /// Following is person-scoped: every enrolled device receives the same set.
    pub all_devices_note: String,
    /// This device reports its applied versions to the workspace's fleet view.
    pub reporting_note: String,
}

/// The `--yes` APPLY report — the public face of `FollowApplied`: the subscription rows written and the
/// installs the reconcile actually landed.
#[derive(Debug, Clone)]
pub struct FollowAppliedView {
    pub workspace_id: String,
    /// The workspace ADDRESS name.
    pub workspace_name: String,
    pub enrolled_now: bool,
    /// The subscription rows this apply wrote (`(kind, name)` — channel joins / direct follows).
    pub subscribed: Vec<(String, String)>,
    /// The installs the reconcile landed (batch-accepted first receives + refreshed knowns).
    pub installed: Vec<InstallView>,
    /// The reconcile's isolated warnings.
    pub warnings: Vec<String>,
}

/// One followed skill's enrollment, as the harness holds it. The device credential is a secret
/// (redacted in `Debug`).
#[derive(Clone)]
struct FollowSpec {
    skill_id: String,
    workspace_id: String,
    credential: String,
    mode: Follow,
}

impl std::fmt::Debug for FollowSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FollowSpec")
            .field("skill_id", &self.skill_id)
            .field("workspace_id", &self.workspace_id)
            .field("credential", &"<redacted>")
            .field("mode", &self.mode)
            .finish()
    }
}

/// A self-cleaning temp directory (RAII — a failed test still tidies), mirroring the in-crate test rigs.
#[derive(Debug)]
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-hero-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A minimal harness stub — the engine reads the placement from `map.json`, never the adapter, so these
/// methods are never reached during a pull, and `add` of a plain dir does not recognize it (no currency).
#[derive(Debug)]
struct NoHarness;

impl HarnessAdapter for NoHarness {
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
            dir: PathBuf::from(skill_id),
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

/// An id source that mints exactly the id it is given — so an adopted skill's local id equals the plane's
/// skill id (the URL path segment + the read-token scope must match for the transport to resolve).
#[derive(Debug)]
struct FixedId(String);

impl IdSource for FixedId {
    fn new_skill_id(&self) -> String {
        self.0.clone()
    }
    fn new_op_id(&self) -> [u8; 16] {
        // This harness exercises no op_id-minting verb; a fixed value keeps the seam total.
        [0u8; 16]
    }
}

/// The HERO rig: a temp `~/.topos/` home + a temp work dir holding each adopted (placement) skill, the real
/// fs/clock/harness seams, and the followed-skill enrollment. Drives the genuine pull engine over a real
/// `UreqPlane` each [`run_pull`](Self::run_pull).
#[derive(Debug)]
pub struct PullHarness {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    /// `pull` never mints ids; the real source satisfies the `Ctx` field.
    ids: RealIds,
    clock: RealClock,
    harness: NoHarness,
    follows: Vec<FollowSpec>,
    /// skill_id -> the placement directory `map.json` records (where materialize writes).
    placements: HashMap<String, PathBuf>,
}

impl PullHarness {
    /// A fresh harness over unique temp dirs (cleaned on drop).
    #[must_use]
    pub fn new(tag: &str) -> Self {
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work: Scratch::new(&format!("{tag}-work")),
            fs: RealFs,
            ids: RealIds,
            clock: RealClock,
            harness: NoHarness,
            follows: Vec::new(),
            placements: HashMap::new(),
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }

    /// Adopt a local skill under the EXACT `skill_id` (so it equals the plane's skill id), producing the
    /// genuine never-pulled sidecar docs via `ops::add` (genesis at `(0,0)`, observed `(0,0)`), and
    /// enroll it as followed in `workspace_id` with the workspace `credential`. `files` is the LOCAL
    /// placeholder bundle — it may differ from the plane's genesis (a first pull then fast-forwards).
    ///
    /// `files` entries are `(bundle-relative path, is_executable, bytes)`.
    ///
    /// # Panics
    /// If the adopt fails (a test precondition error).
    pub fn adopt_followed(
        &mut self,
        skill_id: &str,
        workspace_id: &str,
        credential: &str,
        mode: Follow,
        files: &[(&str, bool, &[u8])],
    ) {
        // The work subdir IS the placement; its basename becomes the skill's name (no harness frontmatter).
        let dir = self.work.0.join(skill_id);
        write_tree(&dir, files);

        let fixed = FixedId(skill_id.to_owned());
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        let ctx = Ctx {
            fs: &self.fs,
            ids: &fixed,
            clock: &self.clock,
            device_id: DEVICE_ID.to_owned(),
            layout: self.layout(),
            harness: &self.harness,
            plane: &inert_plane,
            follow: &inert_follow,
            roots: None,
        };
        let added = ops::add(&ctx, &dir)
            .unwrap_or_else(|e| panic!("test_support: adopt of {skill_id} failed: {e}"));
        assert_eq!(
            added.skill_id, skill_id,
            "the fixed id source must mint the requested skill id"
        );

        // Record the placement EXACTLY as map.json holds it (canonicalized) — what materialize writes to.
        let map: PlacementMap =
            doc::read_map(&self.fs, &self.layout().published(&sid(skill_id)).map)
                .expect("read map.json")
                .expect("map.json exists after add");
        let placement = map
            .placements
            .first()
            .expect("a placement after add")
            .clone();
        self.placements
            .insert(skill_id.to_owned(), PathBuf::from(placement));

        self.follows.push(FollowSpec {
            skill_id: skill_id.to_owned(),
            workspace_id: workspace_id.to_owned(),
            credential: credential.to_owned(),
            mode,
        });
    }

    /// Run the REAL pull engine over a REAL `UreqPlane` at `base_url`. Builds the transport credential map +
    /// the follow seam from the enrolled skills and a fresh `Ctx`, then dispatches `scope`. The served
    /// `current` pointer is unsigned — nothing is verified against a key.
    ///
    /// # Panics
    /// If the pull errors (the bare sweep isolates per-skill failures, so this is a hard wiring fault) or a
    /// `GoBack` hex id is malformed.
    #[must_use]
    pub fn run_pull(&self, base_url: &str, scope: Scope) -> PullData {
        let skill_workspaces: HashMap<String, String> = self
            .follows
            .iter()
            .map(|s| (s.skill_id.clone(), s.workspace_id.clone()))
            .collect();
        // The ONE device credential (the last enrolled spec's — every spec of one rig carries it).
        let credential = self.follows.last().map(|s| s.credential.clone());
        let follow_entries: Vec<(String, FollowContext)> = self
            .follows
            .iter()
            .map(|s| {
                (
                    s.skill_id.clone(),
                    FollowContext {
                        workspace_id: s.workspace_id.clone(),
                        mode: s.mode.to_mode(),
                        review_required: false,
                        following: true,
                        agents: Vec::new(),
                        excluded_agents: Vec::new(),
                    },
                )
            })
            .collect();

        let plane = UreqPlane::new(base_url.to_owned(), credential, skill_workspaces);
        let follow = FileFollow::new(follow_entries);
        let ctx = Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id: DEVICE_ID.to_owned(),
            layout: self.layout(),
            harness: &self.harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };

        let internal = match scope {
            Scope::AllFollowed => ops::PullScope::AllFollowed,
            Scope::Accept { name } => ops::PullScope::One {
                name,
                mode: ops::TargetMode::AcceptPending,
                workspace: None,
            },
            Scope::GoBack {
                name,
                version_id_hex,
            } => {
                let hash = ops::parse_hex32(&version_id_hex)
                    .unwrap_or_else(|e| panic!("test_support: bad go-back version id: {e}"));
                ops::PullScope::One {
                    name,
                    mode: ops::TargetMode::GoBack(ops::VersionRef::Full(hash)),
                    workspace: None,
                }
            }
        };
        ops::pull(&ctx, internal)
            .unwrap_or_else(|e| panic!("test_support: pull failed: {e}"))
            .data
    }

    /// The placement directory's files for `skill_id`: `(bundle-relative path, unix mode bits & 0o777, raw
    /// bytes)`, sorted by path — so the HERO can assert byte-exactness incl. the executable bit.
    ///
    /// # Panics
    /// If the skill was never adopted here.
    #[must_use]
    pub fn placement_files(&self, skill_id: &str) -> Vec<(String, u32, Vec<u8>)> {
        let dir = self
            .placements
            .get(skill_id)
            .unwrap_or_else(|| panic!("test_support: {skill_id} was never adopted"));
        snapshot_dir(dir)
    }

    /// The followed skill's `sync.json` (the durable floor/applied state), read back through the real doc
    /// protocol.
    ///
    /// # Panics
    /// If `sync.json` is missing or unreadable.
    #[must_use]
    pub fn sync_state(&self, skill_id: &str) -> SyncState {
        doc::read_doc(&self.fs, &self.layout().published(&sid(skill_id)).sync)
            .expect("read sync.json")
            .expect("sync.json exists for a followed skill")
    }
}

/// Parse a test skill id through the validated newtype (a rig id is always charset-clean).
fn sid(skill_id: &str) -> crate::id::SkillId {
    crate::id::SkillId::parse(skill_id).expect("test skill id is charset-clean")
}

/// A harness adapter whose placement is an ABSOLUTE directory under a test work root — so a followed skill's
/// first-receive baseline (which records `harness.placement_for(skill_id).dir` into `map.json`) materializes
/// where the e2e can read it back, never a process-relative path. Otherwise identical to [`NoHarness`].
#[derive(Debug)]
struct WorkHarness {
    work: PathBuf,
}

impl HarnessAdapter for WorkHarness {
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
            dir: self.work.join(skill_id),
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

/// The real-`follow` rig: a fresh temp `~/.topos/` (NO pre-adopted skill — `follow` enrolls from scratch) + a
/// temp work root where the first-received skill is placed. Drives the GENUINE `ops::follow` over the GENUINE
/// `ureq` enroll/read transports, so an EXTERNAL e2e crate can prove the whole loop (bootstrap → enroll →
/// redeem → first-receive placement) without reaching the client's `pub(crate)` internals.
///
/// Three adapter modes: the default [`new`](Self::new) wires the [`WorkHarness`] stub (placement under the
/// work root, no currency); [`new_claude`](Self::new_claude) wires the **real Claude Code adapter** over a
/// temp config home — placements land in `<config-home>/skills/<skill_id>` and the enrollment promote arms
/// the REAL `settings.json` session-start hook — [`new_openclaw`](Self::new_openclaw) wires the **real
/// OpenClaw adapter** the same way (a temp stand-in home; the promote registers the silent currency cron
/// through a file-persisted fake `openclaw` CLI — no real gateway exists in a suite) — and
/// [`new_hermes`](Self::new_hermes) wires the **real Hermes adapter** over a temp `$HERMES_HOME`, so an
/// e2e can prove the whole second-machine story against a genuine adapter.
/// The rig's fake `openclaw` CLI: a file-persisted cron store under the stand-in home
/// (`fake-cron.json`), simulating the probed healthy-gateway semantics — declaration-key-idempotent
/// `cron add`, `cron list --json`, id-only `cron rm` — so the composed suites drive the REAL
/// adapter's trigger path without a real openclaw install (which would need a live gateway).
#[derive(Debug)]
struct FakeOpenClawCli {
    store: std::path::PathBuf,
}

impl FakeOpenClawCli {
    fn jobs(&self) -> Vec<serde_json::Value> {
        std::fs::read(&self.store)
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .and_then(|v| v.get("jobs").and_then(|j| j.as_array().cloned()))
            .unwrap_or_default()
    }
    fn save(&self, jobs: &[serde_json::Value]) {
        let doc = serde_json::json!({ "jobs": jobs });
        let _ = std::fs::write(&self.store, doc.to_string());
    }
}

impl topos_harness::CommandRunner for FakeOpenClawCli {
    fn run(&self, _program: &str, args: &[&str]) -> std::io::Result<topos_harness::RunOutput> {
        let ok = |stdout: String| {
            Ok(topos_harness::RunOutput {
                success: true,
                stdout,
            })
        };
        match args {
            ["cron", "add", rest @ ..] => {
                let key = rest
                    .windows(2)
                    .find(|w| w[0] == "--declaration-key")
                    .map(|w| w[1].to_owned())
                    .unwrap_or_default();
                let mut jobs = self.jobs();
                if jobs
                    .iter()
                    .any(|j| j.get("declarationKey").and_then(|k| k.as_str()) == Some(&key))
                {
                    return ok("{\"created\":false,\"updated\":false}".to_owned());
                }
                let id = format!("job-{}", jobs.len() + 1);
                jobs.push(serde_json::json!({ "id": id, "declarationKey": key }));
                self.save(&jobs);
                ok("{\"created\":true}".to_owned())
            }
            ["cron", "list", "--json"] => {
                ok(serde_json::json!({ "jobs": self.jobs() }).to_string())
            }
            ["cron", "rm", id] => {
                let mut jobs = self.jobs();
                let before = jobs.len();
                jobs.retain(|j| j.get("id").and_then(|i| i.as_str()) != Some(*id));
                let removed = jobs.len() < before;
                self.save(&jobs);
                Ok(topos_harness::RunOutput {
                    success: removed,
                    stdout: format!("{{\"ok\":{removed},\"removed\":{removed}}}"),
                })
            }
            other => Ok(topos_harness::RunOutput {
                success: false,
                stdout: format!("unexpected argv: {other:?}"),
            }),
        }
    }
}

#[derive(Debug)]
pub struct FollowHarness {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: RealIds,
    clock: RealClock,
    harness: WorkHarness,
    /// `Some` = the real-Claude mode: the temp `$CLAUDE_CONFIG_DIR` the real adapter resolves against.
    claude: Option<Scratch>,
    /// `Some` = the real-OpenClaw mode: the temp stand-in `~/.openclaw` the real adapter resolves against.
    openclaw: Option<Scratch>,
    /// `Some` = the real-Hermes mode: a temp stand-in `$HERMES_HOME`. The adapter is constructed with
    /// `accept_hooks = false` — the honest no-acceptance-evidence form (the e2e never fabricates a live
    /// per-turn hook; the one-time approval is Hermes's own to grant).
    hermes: Option<Scratch>,
}

impl FollowHarness {
    /// A fresh rig over unique temp dirs (cleaned on drop).
    #[must_use]
    pub fn new(tag: &str) -> Self {
        let work = Scratch::new(&format!("{tag}-work"));
        let harness = WorkHarness {
            work: work.0.clone(),
        };
        Self {
            home: Scratch::new(&format!("{tag}-home")),
            work,
            fs: RealFs,
            ids: RealIds,
            clock: RealClock,
            harness,
            claude: None,
            openclaw: None,
            hermes: None,
        }
    }

    /// A fresh rig wired to the REAL Claude Code adapter over a temp config home (a stand-in
    /// `$CLAUDE_CONFIG_DIR` — the real `~/.claude` is never touched). Placement + the currency hook then
    /// go through the genuine adapter: skills land in `<config-home>/skills/<skill_id>` and the promote
    /// writes the real `settings.json` session-start entry.
    #[must_use]
    pub fn new_claude(tag: &str) -> Self {
        let mut rig = Self::new(tag);
        rig.claude = Some(Scratch::new(&format!("{tag}-claude")));
        rig
    }

    /// A fresh rig wired to the REAL OpenClaw adapter over a temp stand-in home (the real `~/.openclaw`
    /// is never touched — the home is injected, mirroring the adapter's own test isolation). Placement +
    /// the currency trigger then go through the genuine adapter: skills land in `<home>/skills/<skill_id>`
    /// and the promote registers the silent currency cron through the rig's file-persisted fake
    /// `openclaw` CLI (a healthy-gateway double — no suite spawns a real harness process).
    #[must_use]
    pub fn new_openclaw(tag: &str) -> Self {
        let mut rig = Self::new(tag);
        rig.openclaw = Some(Scratch::new(&format!("{tag}-openclaw")));
        rig
    }

    /// A fresh rig wired to the REAL Hermes adapter over a temp stand-in `$HERMES_HOME` (the real
    /// `~/.hermes` is never touched). Placement + the currency hook then go through the genuine adapter:
    /// skills land in `<hermes-home>/skills/general/<skill_id>` and the promote registers the real
    /// `config.yaml` per-turn `pre_llm_call` entry (reported honestly non-Active — no acceptance evidence
    /// exists in a fixture home).
    #[must_use]
    pub fn new_hermes(tag: &str) -> Self {
        let mut rig = Self::new(tag);
        rig.hermes = Some(Scratch::new(&format!("{tag}-hermes")));
        rig
    }

    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }

    /// Run `f` over the rig's adapter: the real `ClaudeCode` / `OpenClaw` / `Hermes` (a stack local
    /// borrowing the fs seam — the adapter borrows its `ConfigStore`, so it cannot be an owned field) or
    /// the `WorkHarness` stub.
    fn with_adapter<R>(&self, f: impl FnOnce(&dyn HarnessAdapter) -> R) -> R {
        if let Some(home) = &self.openclaw {
            let cli = FakeOpenClawCli {
                store: home.0.join("fake-cron.json"),
            };
            return f(&OpenClaw::new(home.0.clone(), &self.fs, &cli));
        }
        if let Some(home) = &self.hermes {
            return f(&Hermes::new(home.0.clone(), false, &self.fs));
        }
        match &self.claude {
            Some(home) => f(&ClaudeCode::new(home.0.clone(), &self.fs)),
            None => f(&self.harness),
        }
    }

    /// The `ops::follow` connector closures (a creds-free `ureq` enroll client + the read transport), built
    /// EXACTLY as the production composition root builds them. Returns the RAW [`ops::FollowOutcome`] (the
    /// classic wire payload, the two-phase describe, or the `--yes` apply) — the public wrappers project it:
    /// [`run_follow`](Self::run_follow) unwraps the classic `Data`, the address-flow methods surface the
    /// describe/apply. Keeps the TYPED error, which [`resume_expect_denied`](Self::resume_expect_denied)
    /// renders through the production envelope.
    fn run_follow_outcome(
        &self,
        plane: &dyn PlaneSource,
        follow: &dyn FollowSource,
        link: Option<String>,
        opts: ops::FollowOpts,
    ) -> Result<ops::FollowOutcome, crate::error::ClientError> {
        let enroll_connect = |base_url: &str| -> Box<dyn EnrollSource> {
            Box::new(UreqDeviceClient::new(base_url.to_owned(), None))
        };
        // The directory/reconcile connectors, built exactly as the composition root builds them
        // (fresh credential reads per build) — the classic e2e flows here never exercise them, but
        // the address-follow continuation does.
        let fs = &self.fs;
        let layout = self.layout();
        let directory_connect = move |base_url: &str| -> Box<dyn crate::plane::DirectorySource> {
            Box::new(UreqDeviceClient::new(
                base_url.to_owned(),
                device_credential(fs, &layout),
            ))
        };
        let layout2 = self.layout();
        let delivery_connect = move |base_url: &str| -> Box<dyn crate::plane::ReconcileTransport> {
            let follows = enroll::read_follows(fs, &layout2)
                .ok()
                .flatten()
                .unwrap_or(Follows {
                    schema_version: 1,
                    follows: Vec::new(),
                });
            Box::new(
                UreqPlane::new(
                    base_url.to_owned(),
                    device_credential(fs, &layout2),
                    enroll::skill_workspaces(&follows),
                )
                .with_workspaces(enrolled_workspaces(fs, &layout2)),
            )
        };
        let connectors = ops::FollowConnectors {
            enroll: &enroll_connect,
            directory: &directory_connect,
            delivery: &delivery_connect,
            web_origin: "https://topos.sh".to_owned(),
        };
        // Production's `Command::Follow` mints the host device id (writing `host.json`) before the op;
        // mirror that here.
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())?;
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane,
                follow,
                roots: None,
            };
            ops::follow(
                &ctx,
                &connectors,
                link.clone().into_iter().collect(),
                opts.clone(),
            )
        })
    }

    /// [`run_follow_outcome`](Self::run_follow_outcome) unwrapping the classic wire payload (the `Data`
    /// variant) — the claim door, the skill-path accept, and an address enrollment's PENDING call 1 all
    /// answer with it. A describe/apply outcome (the address subscribe's two phases) is a wiring error here
    /// — the caller should drive `resume_describe` / `resume_apply` / `follow_apply` for those.
    fn run_follow(
        &self,
        plane: &dyn PlaneSource,
        follow: &dyn FollowSource,
        link: Option<String>,
        opts: ops::FollowOpts,
    ) -> Result<topos_types::results::FollowData, crate::error::ClientError> {
        match self.run_follow_outcome(plane, follow, link, opts)? {
            ops::FollowOutcome::Data { data, .. } => Ok(data),
            other => Err(crate::error::ClientError::InvalidArgument(format!(
                "test_support::run_follow: expected the classic Data payload, got {other:?}"
            ))),
        }
    }

    /// Resume a pending ADDRESS enrollment (poll → redeem → promote → continue into the recorded follow
    /// intent) as the two-phase DESCRIBE (no `--yes`): the enrollment lands (`enrolled_now`), the
    /// subscription + bytes still await the `--yes` consent. Returns the describe the e2e asserts on.
    ///
    /// # Errors
    /// The follow op's typed error rendered to a string (a denied/expired redeem, a transport fault).
    pub fn resume_describe(&self) -> Result<FollowDescribeView, String> {
        match self
            .run_follow_outcome(&InertPlane, &InertFollow, None, follow_opts(false))
            .map_err(|e| e.to_string())?
        {
            ops::FollowOutcome::Described { describe, .. } => Ok(describe_view(&describe)),
            other => Err(format!("test_support: expected a describe, got {other:?}")),
        }
    }

    /// Resume a pending ADDRESS enrollment with `--yes`: promote, then APPLY in the same invocation — the
    /// subscription rows (channel join / direct follow; a workspace target needs none — membership itself
    /// entitles `everyone`) then the reconcile that lands the delivered set byte-exact. Returns the apply
    /// report the e2e asserts on.
    ///
    /// # Errors
    /// As [`resume_describe`](Self::resume_describe).
    pub fn resume_apply(&self) -> Result<FollowAppliedView, String> {
        match self
            .run_follow_outcome(&InertPlane, &InertFollow, None, follow_opts(true))
            .map_err(|e| e.to_string())?
        {
            ops::FollowOutcome::Applied(applied) => Ok(applied_view(&applied)),
            other => Err(format!("test_support: expected an apply, got {other:?}")),
        }
    }

    /// A fresh `follow <address>` (no `--yes`) on an ALREADY-enrolled install → the two-phase DESCRIBE
    /// (resolve the address against the enrolled universe, assemble what `--yes` would land).
    ///
    /// # Errors
    /// As [`resume_describe`](Self::resume_describe).
    pub fn follow_describe(&self, target: &str) -> Result<FollowDescribeView, String> {
        match self
            .run_follow_outcome(
                &InertPlane,
                &InertFollow,
                Some(target.to_owned()),
                follow_opts(false),
            )
            .map_err(|e| e.to_string())?
        {
            ops::FollowOutcome::Described { describe, .. } => Ok(describe_view(&describe)),
            other => Err(format!("test_support: expected a describe, got {other:?}")),
        }
    }

    /// A fresh `follow <address> --yes` on an ALREADY-enrolled install (the subscribe/apply path): resolve
    /// the address against the enrolled universe, write the subscription rows, and reconcile the delivered
    /// set this invocation. Returns the apply report.
    ///
    /// # Errors
    /// As [`resume_describe`](Self::resume_describe).
    pub fn follow_apply(&self, target: &str) -> Result<FollowAppliedView, String> {
        match self
            .run_follow_outcome(
                &InertPlane,
                &InertFollow,
                Some(target.to_owned()),
                follow_opts(true),
            )
            .map_err(|e| e.to_string())?
        {
            ops::FollowOutcome::Applied(applied) => Ok(applied_view(&applied)),
            ops::FollowOutcome::ReattachApplied(reattach) => Ok(reattach_applied_view(&reattach)),
            other => Err(format!("test_support: expected an apply, got {other:?}")),
        }
    }

    /// Call 1: `topos follow <link>` — fetch the bootstrap, guard one-plane, device-authorize, write the pending WAL.
    ///
    /// # Errors
    /// The follow op's typed error rendered to a string (a different-plane refusal / denied / transport failure).
    pub fn follow(&self, link: &str) -> Result<topos_types::results::FollowData, String> {
        self.follow_with(link, false)
    }

    /// Call 1 with an explicit adopt mode: `manual = true` is `follow <link> --manual` (confirm-each).
    ///
    /// # Errors
    /// As [`follow`](Self::follow).
    pub fn follow_with(
        &self,
        link: &str,
        manual: bool,
    ) -> Result<topos_types::results::FollowData, String> {
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        let opts = ops::FollowOpts {
            manual,
            workspace: None,
            yes: false,
            prefix_dirname: false,
            channels: Vec::new(),
            skills: Vec::new(),
            agents: Vec::new(),
        };
        self.run_follow(&inert_plane, &inert_follow, Some(link.to_owned()), opts)
            .map_err(|e| e.to_string())
    }

    /// Call 2: re-invoke `topos follow` — poll, redeem (the grant is the bearer credential; nothing is
    /// signed), promote.
    ///
    /// # Errors
    /// The follow op's typed error (pending/denied/expired/transport).
    pub fn resume(&self) -> Result<topos_types::results::FollowData, String> {
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        let opts = ops::FollowOpts {
            manual: false,
            workspace: None,
            yes: false,
            prefix_dirname: false,
            channels: Vec::new(),
            skills: Vec::new(),
            agents: Vec::new(),
        };
        self.run_follow(&inert_plane, &inert_follow, None, opts)
            .map_err(|e| e.to_string())
    }

    /// A re-invoked `topos follow` where the redeem is EXPECTED to be refused — returns the denial exactly as
    /// the production error envelope surfaces it (wire code + next-action codes + the redacted message),
    /// so the e2e asserts the ask-an-owner `REQUEST_ACCESS` guidance.
    ///
    /// # Panics
    /// If the resume unexpectedly succeeds.
    #[must_use]
    pub fn resume_expect_denied(&self) -> DeniedSurface {
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        let opts = ops::FollowOpts {
            manual: false,
            workspace: None,
            yes: false,
            prefix_dirname: false,
            channels: Vec::new(),
            skills: Vec::new(),
            agents: Vec::new(),
        };
        match self.run_follow(&inert_plane, &inert_follow, None, opts) {
            Ok(_) => panic!("test_support: expected the resume to be denied"),
            Err(e) => {
                let envelope = crate::render::err_envelope("follow", &e);
                DeniedSurface {
                    code: envelope
                        .error
                        .as_ref()
                        .map(|w| w.code.clone())
                        .unwrap_or_default(),
                    message: crate::render::safe_message(&e),
                    next_action_codes: envelope
                        .next_actions
                        .iter()
                        .map(|a| a.code.as_str().to_owned())
                        .collect(),
                }
            }
        }
    }

    /// `topos follow <skill>[@<hash>]` — place the first-received bytes through the REAL read transport
    /// (wired from the minted creds the resume wrote into `follows.json`). The new surface takes ONE
    /// positional skill; the facade keeps its slice signature and drives the single target the e2e passes.
    ///
    /// # Errors
    /// The follow op's typed error.
    pub fn approve(
        &self,
        base_url: &str,
        targets: &[String],
    ) -> Result<topos_types::results::FollowData, String> {
        // Wire ctx.plane from the minted follow-state (the skill path places through ctx.plane).
        let follows = crate::enroll::read_follows(&self.fs, &self.layout())
            .expect("read follows.json")
            .expect("follows.json exists after resume");
        let contexts = crate::enroll::follow_contexts(&follows);
        let plane = UreqPlane::new(
            base_url.to_owned(),
            device_credential(&self.fs, &self.layout()),
            crate::enroll::skill_workspaces(&follows),
        );
        let follow = FileFollow::new(contexts);
        let opts = ops::FollowOpts {
            manual: false,
            workspace: None,
            yes: false,
            prefix_dirname: false,
            channels: Vec::new(),
            skills: Vec::new(),
            agents: Vec::new(),
        };
        let target = targets.first().cloned();
        self.run_follow(&plane, &follow, target, opts)
            .map_err(|e| e.to_string())
    }

    /// Whether `instance.json` exists (enrollment writes it). No trust root is stored — the `current`
    /// pointer is unsigned.
    #[must_use]
    pub fn instance_written(&self) -> bool {
        matches!(
            crate::enroll::read_instance(&self.fs, &self.layout()),
            Ok(Some(_))
        )
    }

    /// The number of followed skills in `follows.json` (0 if absent).
    #[must_use]
    pub fn follows_count(&self) -> usize {
        crate::enroll::read_follows(&self.fs, &self.layout())
            .ok()
            .flatten()
            .map_or(0, |f| f.follows.len())
    }

    /// The unix permission bits of the `0600` device-credential doc (`None` if absent).
    #[must_use]
    pub fn credentials_mode(&self) -> Option<u32> {
        std::fs::metadata(self.layout().credentials_path())
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    }

    /// Whether the pending-enrollment WAL is present (it must be after call 1, absent after a completed resume).
    #[must_use]
    pub fn wal_exists(&self) -> bool {
        self.layout().enrollment_path().exists()
    }

    /// Whether enrollment is complete enough that the production `load_enrollment` would light up: `instance.json`
    /// present AND `follows.json` names at least one followed skill.
    #[must_use]
    pub fn enrolled(&self) -> bool {
        let layout = self.layout();
        let Ok(Some(_)) = crate::enroll::read_instance(&self.fs, &layout) else {
            return false;
        };
        crate::enroll::read_follows(&self.fs, &layout)
            .ok()
            .flatten()
            .is_some_and(|f| f.follows.iter().any(|e| e.following))
    }

    /// The placed bundle for `skill_id`: `(relative path, mode & 0o777, bytes)`, sorted — for a byte-exact assert.
    #[must_use]
    pub fn placement_files(&self, skill_id: &str) -> Vec<(String, u32, Vec<u8>)> {
        snapshot_dir(&self.placement_dir(skill_id))
    }

    /// Where this rig's adapter places `skill_id`: the real harness layout (`<home>/skills/<id>`) in
    /// claude / openclaw mode, the real Hermes no-discovery default (`<hermes-home>/skills/general/<id>`)
    /// in hermes mode, else the work root.
    fn placement_dir(&self, skill_id: &str) -> PathBuf {
        if let Some(home) = &self.openclaw {
            return home.0.join("skills").join(skill_id);
        }
        if let Some(home) = &self.hermes {
            return home.0.join("skills").join("general").join(skill_id);
        }
        match &self.claude {
            Some(home) => home.0.join("skills").join(skill_id),
            None => self.work.0.join(skill_id),
        }
    }

    /// Overwrite the placement with `files` — a local draft ahead of `current` (work != base), for the
    /// never-clobber / diverged assertions.
    pub fn edit_placement(&self, skill_id: &str, files: &[(&str, bool, &[u8])]) {
        write_tree(&self.placement_dir(skill_id), files);
    }

    /// The adapter's currency [`TriggerReport`] — a fresh, IDEMPOTENT re-probe of the same currency
    /// install the enroll `promote` armed (the adapter's `install_currency_trigger` is idempotent: it
    /// detects the already-installed hook and re-reports it, writing no duplicate). The two-phase ADDRESS
    /// enroll does not surface the classic `FollowData.currency` on its describe/apply, so an e2e reads the
    /// disclosure through this probe after enrolling.
    #[must_use]
    pub fn currency_report(&self) -> topos_types::TriggerReport {
        self.with_adapter(|h| h.install_currency_trigger())
    }

    /// Re-record one followed skill's adoption MODE in `follows.json` (the exact doc shape a real
    /// `follow --manual` records). The BRIDGE for an address-enrolled confirm-each drafter: the
    /// enrollment WAL carries the human's `--manual` intent, but the reconcile's first-receive install
    /// currently records `Auto` (threading the WAL mode into the install is the client's later work) —
    /// so the e2e re-records the declared intent here, and the REAL subsequent sweeps then exercise the
    /// engine's genuine confirm-each never-clobber contract.
    ///
    /// # Panics
    /// If the skill has no follow entry (a test-precondition error).
    pub fn set_follow_mode(&self, skill_id: &str, mode: Follow) {
        let follows = enroll::read_follows(&self.fs, &self.layout())
            .expect("read follows.json")
            .expect("follows.json exists after enrollment");
        let mut entry = follows
            .follows
            .into_iter()
            .find(|e| e.skill_id == skill_id)
            .unwrap_or_else(|| panic!("test_support: {skill_id} has no follow entry"));
        entry.mode = match mode {
            Follow::Auto => FollowModeDoc::Auto,
            Follow::ConfirmEach => FollowModeDoc::ConfirmEach,
        };
        enroll::write_follows_merged(&self.fs, &self.layout(), std::slice::from_ref(&entry))
            .expect("write follows.json");
    }

    /// Record a follows.json entry for an AUTHORED (locally adopted + published) skill — the doc-level
    /// shape `follow` writes — so the follow-scoped verbs (`revert`'s strict resolve, the workspace
    /// inference) treat the author's own skill as followed in `workspace_id`. The real flow gets this
    /// entry from the reconcile's install; an authoring rig that never re-received its own publish
    /// records it here.
    pub fn follow_locally(&self, skill_id: &str, workspace_id: &str) {
        enroll::write_follows_merged(
            &self.fs,
            &self.layout(),
            &[FollowEntry {
                skill_id: skill_id.to_owned(),
                workspace_id: workspace_id.to_owned(),
                mode: FollowModeDoc::Auto,
                review_required: false,
                following: true,
                excluded_here: false,
                agents: Vec::new(),
                excluded_agents: Vec::new(),
            }],
        )
        .expect("write follows.json");
    }

    // ── the enrolled VERB drivers (the reshaped surface an external e2e drives post-enroll) ────────
    //
    // Each builds the SAME connectors the production composition root builds (credentialed
    // `UreqDeviceClient` / `UreqPlane` per base URL, credentials re-read fresh per build) and runs the
    // REAL op. Ops whose payload types are client-internal (`unfollow`, the auth group) return the
    // `--json`-equivalent `serde_json::Value`; ops with public `topos-types` payloads return them typed.

    /// This rig's REGISTERED device id (the non-secret handle a self-revoke names; `None` before an
    /// enrollment granted) — for row-level witnesses on per-device tables (exclusions, fleet state).
    #[must_use]
    pub fn device_id(&self) -> Option<String> {
        enroll::read_credentials(&self.fs, &self.layout())
            .ok()
            .flatten()
            .map(|c| c.device_id)
    }

    /// The enrolled plane base (`instance.json` — present after a completed enroll/login).
    fn instance_base(&self) -> String {
        enroll::read_instance(&self.fs, &self.layout())
            .expect("read instance.json")
            .expect("instance.json exists after enrollment")
            .base_url
    }

    /// The wall clock in epoch millis (what the production composition root stamps).
    fn wall_ms() -> i64 {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("the wall clock is past the epoch")
                .as_millis(),
        )
        .expect("epoch millis fit i64")
    }

    /// A credentialed directory-connector closure (the read seam most verbs resolve through).
    fn dir_connect(&self) -> impl Fn(&str) -> Box<dyn DirectorySource> + '_ {
        move |b: &str| -> Box<dyn DirectorySource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        }
    }

    /// A credentialed contribute-connector closure (the write seam).
    fn contribute_connect(&self) -> impl Fn(&str) -> Box<dyn ContributeSource> + '_ {
        move |b: &str| -> Box<dyn ContributeSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        }
    }

    /// The reconcile transport (delivery + report + the per-skill read lane, one object) built from
    /// this rig's on-disk enrollment — fresh reads per call, so a verb that just wrote docs is seen.
    fn reconcile_transport(&self) -> (UreqPlane, FileFollow) {
        let follows = enroll::read_follows(&self.fs, &self.layout())
            .expect("read follows.json")
            .unwrap_or(Follows {
                schema_version: 1,
                follows: Vec::new(),
            });
        let plane = UreqPlane::new(
            self.instance_base(),
            device_credential(&self.fs, &self.layout()),
            enroll::skill_workspaces(&follows),
        )
        .with_workspaces(enrolled_workspaces(&self.fs, &self.layout()));
        let follow = FileFollow::new(enroll::follow_contexts(&follows));
        (plane, follow)
    }

    /// Run one enrolled op over a fresh `Ctx` whose plane/follow seams are INERT (the directory /
    /// contribute / delivery transports are built per-base inside each op).
    fn with_inert_ctx<T>(
        &self,
        op: impl FnOnce(&Ctx<'_>) -> Result<T, crate::error::ClientError>,
    ) -> Result<T, crate::error::ClientError> {
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())?;
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane: &inert_plane,
                follow: &inert_follow,
                roots: None,
            };
            op(&ctx)
        })
    }

    /// Run one enrolled op over a fresh `Ctx` with the REAL read transport + the on-disk follow seam
    /// wired — what the follow-scoped verbs need (`review` resolves the followed skill through
    /// `ctx.follow` and fetches proposal bytes through `ctx.plane`; `revert` reads the fresh current).
    fn with_enrolled_ctx<T>(
        &self,
        op: impl FnOnce(&Ctx<'_>) -> Result<T, crate::error::ClientError>,
    ) -> Result<T, crate::error::ClientError> {
        let (plane, follow) = self.reconcile_transport();
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())?;
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane: &plane,
                follow: &follow,
                roots: None,
            };
            op(&ctx)
        })
    }

    /// Run the REAL delivery-driven reconcile (the bare enrolled `update` sweep: one delivery call per
    /// enrolled workspace, converge, report) over this rig's own enrollment docs. `ack_notices` selects
    /// the interactive posture (narrate THEN ack) vs the quiet hook's fetch-without-ack.
    ///
    /// # Panics
    /// If the reconcile errors (per-skill/per-workspace faults are isolated into warnings).
    #[must_use]
    pub fn reconcile(&self, ack_notices: bool) -> (PullData, Vec<String>) {
        let (plane, follow) = self.reconcile_transport();
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .expect("load-or-create device id");
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane: &plane,
                follow: &follow,
                roots: None,
            };
            let opts = ops::ReconcileOpts {
                ack_notices,
                ..ops::ReconcileOpts::default()
            };
            let out = ops::pull_reconcile_with(&ctx, &plane, &opts)
                .unwrap_or_else(|e| panic!("test_support: reconcile failed: {e}"));
            (out.data, out.warnings)
        })
    }

    /// The `update --quiet` HOOK posture: run the bare reconcile (fetch WITHOUT acking) and return the
    /// one-liner warnings the quiet hook would print. `Ok(lines)` ⇔ the hook exits 0 (an auth/transport
    /// failure is soft — a warning line, never a failed session start); `Err` ⇔ a genuinely local
    /// failure that exits nonzero.
    ///
    /// # Errors
    /// The reconcile's typed error rendered to a string, ONLY when it is not hook-soft.
    pub fn quiet_update(&self) -> Result<Vec<String>, String> {
        let (plane, follow) = self.reconcile_transport();
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .map_err(|e| e.to_string())?;
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane: &plane,
                follow: &follow,
                roots: None,
            };
            match ops::pull_reconcile_with(&ctx, &plane, &ops::ReconcileOpts::default()) {
                Ok(out) => Ok(ops::quiet_hook_lines(
                    &self.fs,
                    &self.layout(),
                    Self::wall_ms(),
                    &out,
                )),
                Err(e) if ops::quiet_soft_failure(&e) => Ok(vec![format!("topos: {e}")]),
                Err(e) => Err(e.to_string()),
            }
        })
    }

    /// Backdate one workspace's `state/sync_status.json` freshness entry — the staleness clock the
    /// quiet hook's "last synced <age> ago" warning reads. Writes through the real doc protocol.
    pub fn backdate_sync_status(&self, ws: &str, last_delivery_at_ms: i64, window_ms: u64) {
        crate::sync_status::record(
            &self.fs,
            &self.layout(),
            &[(
                ws.to_owned(),
                crate::sync_status::WorkspaceSync {
                    last_delivery_at: Some(last_delivery_at_ms),
                    last_report_at: None,
                    staleness_window_ms: window_ms,
                    delivered: std::collections::BTreeMap::new(),
                },
            )],
        )
        .expect("write sync_status.json");
    }

    /// Drive `unfollow <target> --yes` (the person-scoped detach) and return the applied report as its
    /// `--json` value (`items` with kind/name/stops, `bytes_kept`).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn unfollow_apply(&self, target: &str) -> Result<serde_json::Value, String> {
        let directory = self.dir_connect();
        let fs = &self.fs;
        let layout = self.layout();
        let delivery = move |b: &str| -> Box<dyn crate::plane::ReconcileTransport> {
            let follows = enroll::read_follows(fs, &layout)
                .ok()
                .flatten()
                .unwrap_or(Follows {
                    schema_version: 1,
                    follows: Vec::new(),
                });
            Box::new(
                UreqPlane::new(
                    b.to_owned(),
                    device_credential(fs, &layout),
                    enroll::skill_workspaces(&follows),
                )
                .with_workspaces(enrolled_workspaces(fs, &layout)),
            )
        };
        let connectors = ops::UnfollowConnectors {
            directory: &directory,
            delivery: &delivery,
        };
        self.with_inert_ctx(|ctx| {
            match ops::unfollow(ctx, &connectors, &[target.to_owned()], &[], &[], true)? {
                ops::UnfollowOutcome::Applied(a) => {
                    Ok(serde_json::to_value(&a).expect("serialize UnfollowApplied"))
                }
                ops::UnfollowOutcome::Described { .. } => {
                    Err(crate::error::ClientError::InvalidArgument(
                        "test_support: expected an unfollow apply, got a describe".into(),
                    ))
                }
            }
        })
        .map_err(|e| e.to_string())
    }

    /// Drive `remove <target> --yes` (per-device exclusion for a followed skill) and return the typed
    /// [`topos_types::results::RemoveData`].
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn remove_apply(&self, target: &str) -> Result<topos_types::results::RemoveData, String> {
        let directory = self.dir_connect();
        let connectors = ops::RemoveConnectors {
            directory: &directory,
        };
        self.with_inert_ctx(|ctx| {
            match ops::remove(ctx, &connectors, &[target.to_owned()], &[], None, true)? {
                ops::RemoveOutcome::Applied(d) => Ok(d),
                ops::RemoveOutcome::Described { .. } | ops::RemoveOutcome::AgentScope(_) => {
                    Err(crate::error::ClientError::InvalidArgument(
                        "test_support: expected a remove apply, got a describe".into(),
                    ))
                }
            }
        })
        .map_err(|e| e.to_string())
    }

    /// Drive `follow --skill <s>... --yes` (the kind-forced selector batch — resolve ALL-OR-NONE, then
    /// subscribe + reconcile). One unresolvable name refuses the WHOLE invocation with nothing applied.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string (the uniform not-found on any bad name).
    pub fn follow_apply_skills(&self, skills: &[&str]) -> Result<FollowAppliedView, String> {
        let opts = ops::FollowOpts {
            manual: false,
            workspace: None,
            yes: true,
            prefix_dirname: false,
            channels: Vec::new(),
            skills: skills.iter().map(|s| (*s).to_owned()).collect(),
            agents: Vec::new(),
        };
        match self
            .run_follow_outcome(&InertPlane, &InertFollow, None, opts)
            .map_err(|e| e.to_string())?
        {
            ops::FollowOutcome::Applied(applied) => Ok(applied_view(&applied)),
            // A device-excluded skill routes to the RE-ATTACH arm; its apply is the same
            // "subscription re-affirmed + bytes landed" fact, projected into the one view.
            ops::FollowOutcome::ReattachApplied(reattach) => Ok(reattach_applied_view(&reattach)),
            other => Err(format!("test_support: expected an apply, got {other:?}")),
        }
    }

    /// Drive `protect <target> [<level>]` (two-phase: `yes = false` describes — audience included —
    /// `yes = true` applies). Bare level = tighten to the kind's protected default; `"open"` loosens.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string (a role refusal NAMES the required role).
    pub fn protect(
        &self,
        target: &str,
        level: Option<&str>,
        yes: bool,
    ) -> Result<topos_types::results::ProtectData, String> {
        let directory = self.dir_connect();
        let connectors = ops::ProtectConnectors {
            directory: &directory,
        };
        self.with_inert_ctx(
            |ctx| match ops::protect(ctx, &connectors, target, level, None, yes)? {
                ops::ProtectOutcome::Described { data, .. }
                | ops::ProtectOutcome::Applied(data) => Ok(data),
            },
        )
        .map_err(|e| e.to_string())
    }

    /// The ROUTE-BACKED skill log (`GET …/skills/{skill}/log` over the real credentialed transport) —
    /// versions with author/message + purge tombstones, proposal events, and the archived-name facts
    /// (`base_name` vs `name`). This is the same wire read the `log` verb merges into its events; the
    /// verb's own connectors type is module-private to the client, so the e2e drives the transport.
    ///
    /// # Errors
    /// The transport's typed error rendered to a string.
    pub fn skill_log_wire(
        &self,
        workspace_id: &str,
        skill_id: &str,
    ) -> Result<topos_types::requests::WireSkillLog, String> {
        let client = UreqDeviceClient::new(
            self.instance_base(),
            device_credential(&self.fs, &self.layout()),
        );
        DirectorySource::skill_log(&client, workspace_id, skill_id).map_err(|e| e.to_string())
    }

    /// Drive the bare `review` — the INBOX/OUTBOX across every enrolled workspace, author-message first.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn review_inbox(&self) -> Result<topos_types::results::ReviewIndexData, String> {
        let directory = self.dir_connect();
        let contribute = self.contribute_connect();
        let connectors = ops::ReviewConnectors {
            directory: &directory,
            contribute: &contribute,
        };
        self.with_inert_ctx(
            |ctx| match ops::review_dispatch(ctx, &connectors, None, None, None)? {
                ops::ReviewOutcome::Inbox(data) => Ok(data),
                other => Err(crate::error::ClientError::InvalidArgument(format!(
                    "test_support: expected the review inbox, got {other:?}"
                ))),
            },
        )
        .map_err(|e| e.to_string())
    }

    /// Drive `review <skill>@<hash> --approve`. A stale base (current moved since the proposal) is the
    /// typed CONFLICT — surfaced as `Err("CONFLICT: …")` through the production error envelope.
    ///
    /// # Errors
    /// `"<WIRE_CODE>: <redacted message>"`.
    pub fn review_approve(&self, target: &str) -> Result<ReviewData, String> {
        self.review_verdict(target, ops::ReviewVerdict::Approve)
    }

    /// Drive `review <skill>@<hash> --reject -m <reason>` (the reason is REQUIRED and rides into the
    /// author's verdict notice).
    ///
    /// # Errors
    /// As [`review_approve`](Self::review_approve).
    pub fn review_reject(&self, target: &str, reason: &str) -> Result<ReviewData, String> {
        self.review_verdict(
            target,
            ops::ReviewVerdict::Reject {
                reason: Some(reason.to_owned()),
            },
        )
    }

    fn review_verdict(
        &self,
        target: &str,
        verdict: ops::ReviewVerdict,
    ) -> Result<ReviewData, String> {
        let directory = self.dir_connect();
        let contribute = self.contribute_connect();
        let connectors = ops::ReviewConnectors {
            directory: &directory,
            contribute: &contribute,
        };
        self.with_enrolled_ctx(|ctx| {
            match ops::review_dispatch(ctx, &connectors, Some(target), Some(verdict), None)? {
                ops::ReviewOutcome::Applied(data) => Ok(data),
                other => Err(crate::error::ClientError::InvalidArgument(format!(
                    "test_support: expected an applied verdict, got {other:?}"
                ))),
            }
        })
        .map_err(|e| {
            let envelope = crate::render::err_envelope("review", &e);
            let code = envelope
                .error
                .as_ref()
                .map(|w| w.code.clone())
                .unwrap_or_default();
            format!("{code}: {}", crate::render::safe_message(&e))
        })
    }

    /// Drive `revert <skill> --to <good>` from this (enrolled) rig.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string (a purged `--to` target is a typed refusal).
    pub fn revert(
        &self,
        skill: &str,
        to: &str,
        confirm: bool,
    ) -> Result<topos_types::results::RevertData, String> {
        let contribute = self.contribute_connect();
        self.with_enrolled_ctx(|ctx| ops::revert(ctx, &contribute, skill, to, confirm, None))
            .map_err(|e| e.to_string())
            .and_then(revert_applied)
    }

    /// Drive `revert` and expose WHICH two-phase arm answered — the e2e probe for the
    /// describe / byte-level-no-op / applied split (the [`revert`](Self::revert) facade expects an
    /// apply and treats the other arms as errors).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn revert_probe(&self, skill: &str, to: &str, yes: bool) -> Result<RevertProbe, String> {
        let contribute = self.contribute_connect();
        self.with_enrolled_ctx(|ctx| ops::revert(ctx, &contribute, skill, to, yes, None))
            .map_err(|e| e.to_string())
            .map(|outcome| match outcome {
                ops::RevertOutcome::Applied(data) => RevertProbe::Applied(data),
                ops::RevertOutcome::NoOp(_) => RevertProbe::NoOp,
                ops::RevertOutcome::Describe { .. } => RevertProbe::Described,
            })
    }

    /// The op-WAL ids currently pending under this rig's home — empty means no in-flight (or
    /// wedged) contribute op. The e2e wedge regressions assert on this after a refused verdict.
    #[must_use]
    pub fn pending_ops(&self) -> Vec<String> {
        let dir = self.layout().ops_dir();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        let mut ids: Vec<String> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|n| n.ends_with(".json"))
            .collect();
        ids.sort();
        ids
    }

    /// Drive `invite <emails>... [--channel <c>]... --yes` and return the FULL applied invitation:
    /// `(address, invited, mailed)` — `mailed` is the server's honest can-deliver flag (false on a
    /// plane with no SMTP relay; the inviter pastes the address by hand).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn invite_full(
        &self,
        emails: &[&str],
        channels: &[&str],
    ) -> Result<(String, Vec<String>, bool), String> {
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let directory = self.dir_connect();
        let connectors = ops::InviteConnectors {
            governance: &governance,
            directory: &directory,
        };
        self.with_inert_ctx(|ctx| {
            match ops::invite(
                ctx,
                &connectors,
                emails.iter().map(|e| (*e).to_owned()).collect(),
                channels.iter().map(|c| (*c).to_owned()).collect(),
                None,
                true,
            )? {
                ops::InviteOutcome::Applied(d) => Ok((d.address, d.invited, d.mailed)),
                other => Err(crate::error::ClientError::InvalidArgument(format!(
                    "test_support: expected an invite apply, got {other:?}"
                ))),
            }
        })
        .map_err(|e| e.to_string())
    }

    /// Drive `auth login [server]` — call 1 answers `{"pending": {user_code, server}}` (the device flow
    /// started; a WAL holds the session), a re-invoke after the identity leg answers `{"done": …}` (the
    /// `POST /v1/login` redeem: one re-minted credential per confirmed seat, the docs written).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn auth_login(&self, server: Option<&str>) -> Result<serde_json::Value, String> {
        let enroll_c = |b: &str| -> Box<dyn EnrollSource> {
            Box::new(UreqDeviceClient::new(b.to_owned(), None))
        };
        let directory = self.dir_connect();
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let connectors = ops::AuthConnectors {
            enroll: &enroll_c,
            directory: &directory,
            governance: &governance,
            web_origin: server
                .map(str::to_owned)
                .unwrap_or_else(|| "https://topos.sh".to_owned()),
        };
        self.with_inert_ctx(|ctx| match ops::login(ctx, &connectors, server, None)? {
            ops::AuthLoginOutcome::Pending(p) => Ok(serde_json::json!({
                "pending": { "user_code": p.user_code, "server": p.server },
            })),
            ops::AuthLoginOutcome::Done(d) => Ok(serde_json::json!({
                "done": serde_json::to_value(&d).expect("serialize AuthLoginData"),
            })),
        })
        .map_err(|e| e.to_string())
    }

    /// Drive `auth logout --yes` (the apply: best-effort self device-revoke per workspace + delete
    /// `credentials.json`; skills/follows/drafts stay). Returns the applied report as its `--json` value.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn auth_logout(&self) -> Result<serde_json::Value, String> {
        let enroll_c = |b: &str| -> Box<dyn EnrollSource> {
            Box::new(UreqDeviceClient::new(b.to_owned(), None))
        };
        let directory = self.dir_connect();
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let connectors = ops::AuthConnectors {
            enroll: &enroll_c,
            directory: &directory,
            governance: &governance,
            web_origin: "https://topos.sh".to_owned(),
        };
        self.with_inert_ctx(|ctx| match ops::logout(ctx, &connectors, true)? {
            ops::AuthLogoutOutcome::Applied(d) => {
                Ok(serde_json::to_value(&d).expect("serialize AuthLogoutData"))
            }
            ops::AuthLogoutOutcome::Described { .. } => {
                Err(crate::error::ClientError::InvalidArgument(
                    "test_support: expected a logout apply, got a describe".into(),
                ))
            }
        })
        .map_err(|e| e.to_string())
    }

    /// Drive the side-effect-free `auth status` and return it as its `--json` value (whoami, the
    /// per-workspace credential probe verdicts, hook health, the reporting posture).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn auth_status(&self) -> Result<serde_json::Value, String> {
        let enroll_c = |b: &str| -> Box<dyn EnrollSource> {
            Box::new(UreqDeviceClient::new(b.to_owned(), None))
        };
        let directory = self.dir_connect();
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let connectors = ops::AuthConnectors {
            enroll: &enroll_c,
            directory: &directory,
            governance: &governance,
            web_origin: "https://topos.sh".to_owned(),
        };
        self.with_inert_ctx(|ctx| {
            ops::status(ctx, &connectors)
                .map(|d| serde_json::to_value(&d).expect("serialize AuthStatusData"))
        })
        .map_err(|e| e.to_string())
    }

    /// The raw `settings.json` in the claude config home (`None` when absent or not in claude mode) — the
    /// e2e asserts the exact installed hook command against it.
    #[must_use]
    pub fn settings_json(&self) -> Option<String> {
        let home = self.claude.as_ref()?;
        std::fs::read_to_string(home.0.join("settings.json")).ok()
    }

    /// The openclaw-mode stand-in home path (`None` when not in openclaw mode) — the e2e derives the
    /// expected registration entry from it.
    #[must_use]
    pub fn openclaw_home(&self) -> Option<PathBuf> {
        self.openclaw.as_ref().map(|h| h.0.clone())
    }

    /// The fake `openclaw` CLI's persisted cron store in the stand-in home (`None` when absent or
    /// not in openclaw mode) — the e2e asserts the silent currency job's registration (its
    /// declaration key) against it.
    #[must_use]
    pub fn openclaw_cron_state(&self) -> Option<String> {
        let home = self.openclaw.as_ref()?;
        std::fs::read_to_string(home.0.join("fake-cron.json")).ok()
    }

    /// The raw `config.yaml` in the hermes home (`None` when absent or not in hermes mode) — the e2e
    /// asserts the exact registered `pre_llm_call` entry line against it.
    #[must_use]
    pub fn hermes_config(&self) -> Option<String> {
        let home = self.hermes.as_ref()?;
        std::fs::read_to_string(home.0.join("config.yaml")).ok()
    }

    /// The followed skill's `sync.json` (the durable floor/applied state).
    ///
    /// # Panics
    /// If `sync.json` is missing or unreadable.
    #[must_use]
    pub fn sync_state(&self, skill_id: &str) -> SyncState {
        doc::read_doc(&self.fs, &self.layout().published(&sid(skill_id)).sync)
            .expect("read sync.json")
            .expect("sync.json exists for a followed skill")
    }

    /// Run the REAL pull engine over the enrollment THIS rig's own `follow` wrote — the transports and the
    /// base URL all come from `instance.json`/`follows.json`, exactly as the production `load_enrollment`
    /// wires them (the session-start hook's `topos pull` runs this path).
    ///
    /// # Panics
    /// If the enrollment docs are missing/corrupt or the pull errors (a hard wiring fault).
    #[must_use]
    pub fn pull(&self, scope: Scope) -> PullData {
        let instance = enroll::read_instance(&self.fs, &self.layout())
            .expect("read instance.json")
            .expect("instance.json exists after resume");
        let follows = enroll::read_follows(&self.fs, &self.layout())
            .expect("read follows.json")
            .expect("follows.json exists after resume");
        let plane = UreqPlane::new(
            instance.base_url,
            device_credential(&self.fs, &self.layout()),
            enroll::skill_workspaces(&follows),
        );
        let follow = FileFollow::new(enroll::follow_contexts(&follows));
        let internal = match scope {
            Scope::AllFollowed => ops::PullScope::AllFollowed,
            Scope::Accept { name } => ops::PullScope::One {
                name,
                mode: ops::TargetMode::AcceptPending,
                workspace: None,
            },
            Scope::GoBack {
                name,
                version_id_hex,
            } => ops::PullScope::One {
                name,
                workspace: None,
                mode: ops::TargetMode::GoBack(ops::VersionRef::Full(
                    ops::parse_hex32(&version_id_hex).expect("go-back id is 32-byte hex"),
                )),
            },
        };
        // Production's `Command::Pull` arm loads (or mints) the device id — a draft snapshot authors under it.
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .expect("load-or-create device id");
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id,
                layout: self.layout(),
                harness,
                plane: &plane,
                follow: &follow,
                roots: None,
            };
            ops::pull(&ctx, internal)
                .unwrap_or_else(|e| panic!("test_support: pull failed: {e}"))
                .data
        })
    }

    // ── the workspace-standup drivers (the chain e2e's surface) ────────────────────────────────────

    /// Adopt a local skill under the EXACT `skill_id` into this rig's work root via the genuine
    /// `ops::add` (the same never-pulled sidecar docs a real adopt writes) — the draft a subsequent
    /// [`publish`](Self::publish) ships. `files` entries are `(bundle-relative path, is_executable, bytes)`.
    ///
    /// # Panics
    /// If the adopt fails (a test-precondition error).
    pub fn adopt(&self, skill_id: &str, files: &[(&str, bool, &[u8])]) {
        let dir = self.work.0.join(skill_id);
        write_tree(&dir, files);
        let fixed = FixedId(skill_id.to_owned());
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .expect("load-or-create device id");
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &fixed,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane: &inert_plane,
                follow: &inert_follow,
                roots: None,
            };
            let added = ops::add(&ctx, &dir)
                .unwrap_or_else(|e| panic!("test_support: adopt of {skill_id} failed: {e}"));
            assert_eq!(
                added.skill_id, skill_id,
                "the fixed id source must mint the requested skill id"
            );
        });
    }

    /// Write an UNadopted skill bundle at `<work>/<name>/` and return its path — the raw directory a
    /// `publish <dir>` AUTO-ADOPTS before shipping. Unlike [`adopt`](Self::adopt) it does not track the
    /// skill (that is the publish's auto-add pre-step's job).
    #[must_use]
    pub fn write_skill_dir(&self, name: &str, files: &[(&str, bool, &[u8])]) -> std::path::PathBuf {
        let dir = self.work.0.join(name);
        write_tree(&dir, files);
        dir
    }

    /// The adopted draft's bundle digest (lowercase hex) — the `<digest>` a publish's `<skill>@<digest>`
    /// pin carries. Scans the SAME work-root placement [`adopt`](Self::adopt) tracked.
    ///
    /// # Panics
    /// If the placement cannot be scanned.
    #[must_use]
    pub fn draft_digest(&self, skill_id: &str) -> String {
        let scanned = scan::scan(&self.work.0.join(skill_id)).expect("scan the adopted draft");
        to_hex(&scanned.bundle_digest)
    }

    /// Drive a DIRECT `publish <skill>@<digest>` over the REAL transports. An un-enrolled rig is
    /// refused typed ("not enrolled — run `topos follow <workspace-address>` first"); the
    /// `_standup_base_url` parameter is retained for call-site compatibility and IGNORED (the
    /// workspace-creating publish is retired — workspaces are born server-side).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn publish(&self, standup_base_url: &str, approve: &str) -> Result<PublishResult, String> {
        self.publish_impl(standup_base_url, approve, false, None, None, None)
    }

    /// [`publish`](Self::publish) with a `-m <message>` — the author's commit message (it becomes the
    /// candidate's recorded message: the review inbox leads with it, `log` carries it, and a verdict
    /// notice names the change by it).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn publish_message(
        &self,
        standup_base_url: &str,
        approve: &str,
        message: &str,
    ) -> Result<PublishResult, String> {
        self.publish_impl(standup_base_url, approve, false, None, None, Some(message))
    }

    /// [`publish`](Self::publish) with an EXPLICIT `--workspace <id>` (the global flag) — disambiguates a
    /// skill NAME shared across workspaces (the resolve filter), while a FOLLOWED skill still signs in its
    /// OWN workspace (the pointer scope). The multi-workspace e2e's same-name disambiguation drives this.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string (an ambiguous name, an unjoined `--workspace`, …).
    pub fn publish_in_workspace(
        &self,
        standup_base_url: &str,
        approve: &str,
        workspace: &str,
    ) -> Result<PublishResult, String> {
        self.publish_impl(
            standup_base_url,
            approve,
            false,
            None,
            Some(workspace),
            None,
        )
    }

    /// [`publish`](Self::publish) with a `--to <channel>` placement — the channel-targeted genesis the
    /// placement e2e drives (a `--to` placement REPLACES the `everyone` default for a brand-new skill).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn publish_to(
        &self,
        approve: &str,
        channel: &str,
        message: &str,
    ) -> Result<PublishResult, String> {
        self.publish_impl("", approve, false, Some(channel), None, Some(message))
    }

    /// [`publish`](Self::publish) with `--propose` — the VOLUNTARY proposal (a reviewer+ author's
    /// direct publish would land; this opens review anyway). The four-eyes e2e drives it.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn propose_message(&self, approve: &str, message: &str) -> Result<PublishResult, String> {
        self.publish_impl("", approve, true, None, None, Some(message))
    }

    fn publish_impl(
        &self,
        standup_base_url: &str,
        approve: &str,
        propose: bool,
        channel: Option<&str>,
        workspace: Option<&str>,
        message: Option<&str>,
    ) -> Result<PublishResult, String> {
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .map_err(|e| e.to_string())?;
        let _ = standup_base_url;
        // The write connector presents the ONE device credential, re-read FRESH from disk.
        let contribute = |b: &str| -> Box<dyn ContributeSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        // `publish` never reads ctx.plane (the enrolled write transport is built per-base inside the op),
        // so THAT read seam stays inert; the OK receipt's pointer is scope-checked, not verified against a
        // key. The FOLLOW seam must be REAL, though: an enrolled publish infers a followed skill's OWN
        // workspace from its follow entry (the pointer scope — never an ambient guess), the only correct
        // op scope once this install follows skills across several workspaces.
        let inert_plane = InertPlane;
        let follows = crate::enroll::read_follows(&self.fs, &self.layout())
            .ok()
            .flatten();
        let follow = FileFollow::new(
            follows
                .as_ref()
                .map(crate::enroll::follow_contexts)
                .unwrap_or_default(),
        );
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane: &inert_plane,
                follow: &follow,
                roots: None,
            };
            match ops::publish(
                &ctx,
                &contribute,
                None, // roots — the harness adopts the skill before publishing (no auto-add)
                approve,
                propose,
                channel,
                workspace,
                message,
            )
            .map_err(|e| e.to_string())?
            {
                ops::PublishOutcome::Published(d) => Ok(PublishResult::Published(d)),
                ops::PublishOutcome::Proposed(d) => Ok(PublishResult::Proposed(d)),
            }
        })
    }

    /// Drive the real `invite <email> --yes` verb: this (owner) rig POSTs the invitation (a member-lane
    /// roster write under the workspace Bearer credential) and returns the workspace ADDRESS the invitee
    /// joins at (the invite carries no link — the roster is the lock). `skills` is accepted for call-site
    /// compatibility but IGNORED: the reshaped invite pre-places CHANNELS, and the seeded genesis rides the
    /// structural `everyone`, so an invitee needs no explicit placement.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string (a policy-DENIED surfaces as "not authorized").
    pub fn invite(&self, email: &str, skills: &[&str]) -> Result<String, String> {
        self.invite_impl(email, skills, None)
    }

    /// [`invite`](Self::invite) with an EXPLICIT `--workspace <id>` (the ambient-verb selector for an
    /// install that follows skills across several workspaces).
    ///
    /// # Errors
    /// As [`invite`](Self::invite); an unjoined `--workspace` id is a local `WorkspaceSelection` that never
    /// reaches the plane.
    pub fn invite_in_workspace(
        &self,
        email: &str,
        skills: &[&str],
        workspace: &str,
    ) -> Result<String, String> {
        self.invite_impl(email, skills, Some(workspace))
    }

    fn invite_impl(
        &self,
        email: &str,
        skills: &[&str],
        workspace: Option<&str>,
    ) -> Result<String, String> {
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .map_err(|e| e.to_string())?;
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let directory = |b: &str| -> Box<dyn DirectorySource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let connectors = ops::InviteConnectors {
            governance: &governance,
            directory: &directory,
        };
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane: &inert_plane,
                follow: &inert_follow,
                roots: None,
            };
            let _ = skills;
            match ops::invite(
                &ctx,
                &connectors,
                vec![email.to_owned()],
                Vec::new(),
                workspace,
                true,
            )
            .map_err(|e| e.to_string())?
            {
                ops::InviteOutcome::Applied(data) => Ok(data.address),
                other => Err(format!(
                    "test_support: expected an invite apply, got {other:?}"
                )),
            }
        })
    }

    /// Drive the real bare `list` (`list --json`, no skill filter), returning the typed [`ListData`]. A
    /// bare list is plane-independent (only a narrowed `list <skill>` fetches proposals), so the read seams
    /// stay inert; every per-entry `workspace_id` + the followed bucket come from the on-disk
    /// `follows.json` / `user.json` the follows wrote — so an e2e asserts each skill's workspace provenance.
    ///
    /// # Panics
    /// If the list errors (a hard wiring fault).
    #[must_use]
    pub fn list(&self) -> ListData {
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .expect("load-or-create device id");
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        self.with_adapter(|harness| {
            let ctx = Ctx {
                fs: &self.fs,
                ids: &self.ids,
                clock: &self.clock,
                device_id: device_id.clone(),
                layout: self.layout(),
                harness,
                plane: &inert_plane,
                follow: &inert_follow,
                roots: None,
            };
            // `None` discovery roots = tracked-only: this helper must not scan the runner's real home dirs.
            ops::list(&ctx, None, false, None, None)
                .expect("test_support: list failed")
                .data
        })
    }

    /// The workspace memberships `user.json` holds, as `(workspace_id, display_name)` in stored order — so
    /// an e2e asserts a second same-plane follow ADDED a membership rather than overwriting the first.
    #[must_use]
    pub fn memberships(&self) -> Vec<(String, String)> {
        enroll::read_user(&self.fs, &self.layout())
            .ok()
            .flatten()
            .map(|u| {
                u.workspaces
                    .into_iter()
                    .map(|m| (m.workspace_id, m.display_name))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The follow-state `follows.json` holds, as `(skill_id, workspace_id, following)` in stored order — so
    /// an e2e asserts each followed skill is tagged with its OWN workspace and a second follow never drops
    /// the first.
    #[must_use]
    pub fn follows(&self) -> Vec<(String, String, bool)> {
        enroll::read_follows(&self.fs, &self.layout())
            .ok()
            .flatten()
            .map(|f| {
                f.follows
                    .into_iter()
                    .map(|e| (e.skill_id, e.workspace_id, e.following))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The principal the enrollment seated this device as (from `user.json`; `None` before promote).
    #[must_use]
    pub fn user_principal(&self) -> Option<String> {
        enroll::read_user(&self.fs, &self.layout())
            .ok()
            .flatten()
            .and_then(|u| u.principal)
    }

    /// The first enrolled workspace id (from `user.json`; `None` before promote). One membership in the
    /// single-workspace flows the fixtures drive.
    #[must_use]
    pub fn user_workspace(&self) -> Option<String> {
        enroll::read_user(&self.fs, &self.layout())
            .ok()
            .flatten()
            .and_then(|u| u.workspaces.into_iter().next())
            .map(|m| m.workspace_id)
    }
}

/// The result of a [`ContributeHarness::publish`] / [`FollowHarness::publish`]: `current` moved (a direct
/// publish), or a proposal opened (`--propose`, or the protection gate's downgrade). The public face of
/// the client's internal `PublishOutcome`.
#[derive(Debug, Clone)]
pub enum PublishResult {
    /// A direct publish moved `current`.
    Published(PublishData),
    /// `--propose` opened a proposal (NEEDS_REVIEW); `current` did NOT move.
    Proposed(ProposeData),
}

/// Which two-phase `revert` arm answered — the public face of the client's internal `RevertOutcome`
/// for the e2e describe/no-op probes.
#[derive(Debug)]
pub enum RevertProbe {
    /// `--yes` landed the forward move.
    Applied(topos_types::results::RevertData),
    /// The `--to` bytes already ARE current — nothing minted, nothing POSTed.
    NoOp,
    /// The bare describe — nothing written.
    Described,
}

/// A follow step's DENIAL, surfaced exactly as the production `--json` envelope would carry it: the wire
/// error code and the next-action codes come from the real render mapping, the message from the redacted
/// safe surface — so an external e2e asserts the REQUEST_ACCESS guidance without reaching `pub(crate)`.
#[derive(Debug, Clone)]
pub struct DeniedSurface {
    /// The wire error code (`DENIED` for a refused redeem).
    pub code: String,
    /// The redacted user-facing message (the ask-an-owner guidance for a denied enrollment redeem).
    pub message: String,
    /// The machine-actionable next-action codes (`REQUEST_ACCESS` for a denied redeem).
    pub next_action_codes: Vec<String>,
}

/// The contribute rig: an ENROLLED publisher/reviewer home (instance/user/follows + a device key) with one
/// adopted, followed skill, over a real `~/.topos/` + work dir. Drives the GENUINE write verbs
/// (`publish`/`review`/`revert`/`diff`) over the GENUINE `ureq` transports against a loopback plane, so an
/// external e2e crate proves the contribute loop without reaching the client's `pub(crate)` internals.
#[derive(Debug)]
pub struct ContributeHarness {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: RealIds,
    clock: RealClock,
    harness: WorkHarness,
    skill_id: String,
    workspace_id: String,
    credential: String,
}

impl ContributeHarness {
    /// A fresh rig over unique temp dirs (cleaned on drop).
    #[must_use]
    pub fn new(tag: &str) -> Self {
        let work = Scratch::new(&format!("{tag}-cwork"));
        let harness = WorkHarness {
            work: work.0.clone(),
        };
        Self {
            home: Scratch::new(&format!("{tag}-chome")),
            work,
            fs: RealFs,
            ids: RealIds,
            clock: RealClock,
            harness,
            skill_id: String::new(),
            workspace_id: String::new(),
            credential: String::new(),
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }

    /// The REGISTERED device id this rig enrolled under (from `credentials.json`).
    ///
    /// # Panics
    /// If [`enroll`](Self::enroll) has not run yet.
    #[must_use]
    pub fn device_id(&self) -> String {
        enroll::read_credentials(&self.fs, &self.layout())
            .expect("read credentials.json")
            .expect("credentials.json exists after enroll")
            .device_id
    }

    /// Enroll this client EXACTLY as a granted `follow <address>` would: write `instance.json` (the
    /// plane base), `user.json` (the workspace membership), `credentials.json` (the ONE device Bearer
    /// credential + the registered device id), and `follows.json` (one followed skill — pure
    /// subscription state), then adopt the skill under its exact id with `placeholder_files`. A
    /// subsequent [`pull`](Self::pull) fast-forwards onto the plane's current.
    ///
    /// # Panics
    /// If a write or the adopt fails (a test-precondition error).
    pub fn enroll(
        &mut self,
        base_url: &str,
        workspace_id: &str,
        skill_id: &str,
        credential: &str,
        review_required: bool,
        placeholder_files: &[(&str, bool, &[u8])],
    ) {
        let layout = self.layout();
        enroll::write_instance(
            &self.fs,
            &layout,
            &Instance {
                schema_version: 1,
                base_url: base_url.to_owned(),
            },
        )
        .expect("write instance.json");
        enroll::write_user(
            &self.fs,
            &layout,
            &UserDoc {
                schema_version: 1,
                principal: None,
                workspaces: vec![Membership {
                    workspace_id: workspace_id.to_owned(),
                    name: "test".to_owned(),
                    display_name: "Test".to_owned(),
                    enrolled_at: 1,
                }],
            },
        )
        .expect("write user.json");
        enroll::write_credentials(&self.fs, &layout, credential, "dev_e2e")
            .expect("write credentials.json");
        doc::write_doc_private(
            &self.fs,
            &layout.follows_path(),
            &Follows {
                schema_version: 1,
                follows: vec![FollowEntry {
                    skill_id: skill_id.to_owned(),
                    workspace_id: workspace_id.to_owned(),
                    mode: FollowModeDoc::Auto,
                    review_required,
                    following: true,
                    excluded_here: false,
                    agents: Vec::new(),
                    excluded_agents: Vec::new(),
                }],
            },
        )
        .expect("write follows.json");

        // Adopt the skill under its exact id (so the local id == the plane skill id), via the genuine add.
        let dir = self.work.0.join(skill_id);
        write_tree(&dir, placeholder_files);
        let fixed = FixedId(skill_id.to_owned());
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &layout)
            .expect("load-or-create device id");
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        let ctx = Ctx {
            fs: &self.fs,
            ids: &fixed,
            clock: &self.clock,
            device_id,
            layout: layout.clone(),
            harness: &self.harness,
            plane: &inert_plane,
            follow: &inert_follow,
            roots: None,
        };
        let added =
            ops::add(&ctx, &dir).unwrap_or_else(|e| panic!("contribute: adopt {skill_id}: {e}"));
        assert_eq!(
            added.skill_id, skill_id,
            "the fixed id source mints the requested id"
        );

        self.skill_id = skill_id.to_owned();
        self.workspace_id = workspace_id.to_owned();
        self.credential = credential.to_owned();
    }

    /// The placement directory this client's skill is tracked at (where edits become a draft).
    fn placement(&self) -> PathBuf {
        self.work.0.join(&self.skill_id)
    }

    /// The real `UreqPlane` read transport + `FileFollow`, built from the enrolled follow-state.
    fn read_transport(&self) -> (UreqPlane, FileFollow) {
        let follows = enroll::read_follows(&self.fs, &self.layout())
            .expect("read follows.json")
            .expect("follows.json exists after enroll");
        (
            UreqPlane::new(
                self.base_url(),
                device_credential(&self.fs, &self.layout()),
                enroll::skill_workspaces(&follows),
            ),
            FileFollow::new(enroll::follow_contexts(&follows)),
        )
    }

    fn base_url(&self) -> String {
        enroll::read_instance(&self.fs, &self.layout())
            .expect("read instance.json")
            .expect("instance.json exists after enroll")
            .base_url
    }

    /// Run the real pull engine (reach the plane's current). Panics on a hard wiring fault.
    #[must_use]
    pub fn pull(&self) -> PullData {
        let (plane, follow) = self.read_transport();
        let device_id =
            crate::identity::load_or_create_device_id(&self.fs, &self.layout()).expect("device id");
        let ctx = Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id,
            layout: self.layout(),
            harness: &self.harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        ops::pull(&ctx, ops::PullScope::AllFollowed)
            .unwrap_or_else(|e| panic!("contribute: pull: {e}"))
            .data
    }

    /// Overwrite the placement with `files` (a fresh draft ahead of `current`).
    pub fn edit_placement(&self, files: &[(&str, bool, &[u8])]) {
        write_tree(&self.placement(), files);
    }

    /// The current placement bundle's digest (lowercase hex) — the `<digest>` an outward verb's `@<digest>` pin
    /// must carry.
    #[must_use]
    pub fn draft_digest(&self) -> String {
        let scanned = scan::scan(&self.placement()).expect("scan the placement");
        to_hex(&scanned.bundle_digest)
    }

    /// Build the device-signed-write `Ctx` + connectors and run `op` (a closure over the wired transports).
    fn with_write_ctx<T>(
        &self,
        op: impl FnOnce(
            &Ctx<'_>,
            &dyn Fn(&str) -> Box<dyn ContributeSource>,
            &dyn Fn(&str) -> Box<dyn GovernanceSource>,
        ) -> Result<T, String>,
    ) -> Result<T, String> {
        let (plane, follow) = self.read_transport();
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .map_err(|e| e.to_string())?;
        let ctx = Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id,
            layout: self.layout(),
            harness: &self.harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        let contribute = |b: &str| -> Box<dyn ContributeSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        op(&ctx, &contribute, &governance)
    }

    /// Drive `publish` (or `--propose`).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn publish(&self, propose: bool, approve: &str) -> Result<PublishResult, String> {
        self.with_write_ctx(|ctx, contribute, governance| {
            let _ = governance;
            match ops::publish(ctx, contribute, None, approve, propose, None, None, None)
                .map_err(|e| e.to_string())?
            {
                ops::PublishOutcome::Published(d) => Ok(PublishResult::Published(d)),
                ops::PublishOutcome::Proposed(d) => Ok(PublishResult::Proposed(d)),
            }
        })
    }

    /// Drive `review <skill>@<hash> --approve | --reject` (the verdict is the consent). The reshaped verb
    /// dispatches through `review_dispatch` over the directory (inbox/describe) + contribute (the write)
    /// seams; a target + verdict applies directly, which is what this drives.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn review(&self, target: &str, approve: bool) -> Result<ReviewData, String> {
        let (plane, follow) = self.read_transport();
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .map_err(|e| e.to_string())?;
        let ctx = Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id,
            layout: self.layout(),
            harness: &self.harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        let contribute = |b: &str| -> Box<dyn ContributeSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let directory = |b: &str| -> Box<dyn DirectorySource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                device_credential(&self.fs, &self.layout()),
            ))
        };
        let connectors = ops::ReviewConnectors {
            directory: &directory,
            contribute: &contribute,
        };
        let verdict = if approve {
            ops::ReviewVerdict::Approve
        } else {
            ops::ReviewVerdict::Reject {
                reason: Some("test_support: rejected".to_owned()),
            }
        };
        match ops::review_dispatch(&ctx, &connectors, Some(target), Some(verdict), None)
            .map_err(|e| e.to_string())?
        {
            ops::ReviewOutcome::Applied(data) => Ok(data),
            other => Err(format!(
                "test_support: expected an applied review verdict, got {other:?}"
            )),
        }
    }

    /// Drive `revert <skill> --to <good>`. The `approve` arg is the legacy `<skill>@<hash>` token the e2e
    /// still passes; the new surface takes only the skill, so the facade parses the name from it (`--to` is
    /// now the sole good-version source).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn revert(&self, to: &str, approve: &str, confirm: bool) -> Result<RevertData, String> {
        let skill = approve.split_once('@').map(|(s, _)| s).unwrap_or(approve);
        self.with_write_ctx(|ctx, contribute, _gov| {
            ops::revert(ctx, contribute, skill, to, confirm, None).map_err(|e| e.to_string())
        })
        .and_then(revert_applied)
    }

    /// Drive `diff <skill> [<ref>]` (a plane ref fetches + re-verifies).
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn diff(&self, r#ref: Option<&str>) -> Result<DiffData, String> {
        let skill = self.skill_id.clone();
        self.with_write_ctx(|ctx, _c, _g| ops::diff(ctx, &skill, r#ref).map_err(|e| e.to_string()))
    }

    /// Run `pull` and return the `proposals_awaiting` count (the plane proposals route summed over the
    /// followed skills).
    #[must_use]
    pub fn proposals_awaiting(&self) -> u32 {
        self.pull().proposals_awaiting
    }

    /// Run `list <skill>` (narrowed), returning the entry's `pending_proposals` (each `<skill>@<hash>`, from
    /// the plane proposals route).
    #[must_use]
    pub fn list_pending_proposals(&self) -> Vec<String> {
        let (plane, follow) = self.read_transport();
        let device_id =
            crate::identity::load_or_create_device_id(&self.fs, &self.layout()).expect("device id");
        let ctx = Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id,
            layout: self.layout(),
            harness: &self.harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        let data = ops::list(&ctx, Some(&self.skill_id), false, None, None)
            .expect("list")
            .data;
        data.tracked
            .into_iter()
            .flat_map(|e| e.pending_proposals)
            .collect()
    }

    /// This skill's `sync.json` (the floor/applied state).
    #[must_use]
    pub fn sync_state(&self) -> SyncState {
        doc::read_doc(
            &self.fs,
            &self.layout().published(&sid(&self.skill_id)).sync,
        )
        .expect("read sync.json")
        .expect("sync.json exists")
    }

    /// The placement bundle: `(relative path, mode & 0o777, bytes)`, sorted — for a byte-exact assert.
    #[must_use]
    pub fn placement_files(&self) -> Vec<(String, u32, Vec<u8>)> {
        snapshot_dir(&self.placement())
    }

    /// The RAW device-credential catalog round-trip (`list --remote`'s transport leg): build the REAL
    /// [`UreqDeviceClient`] at `base_url` holding this rig's per-workspace credential map (from
    /// `credentials.json`), and `fetch_catalog` over loopback HTTP presenting the workspace's Bearer
    /// credential — proving the presented-credential path → the plane's registry lookup → the
    /// confirmed-member gate → the `WireSkillIndex` body (returned verbatim). A **404** — not a member /
    /// revoked credential / no such workspace — maps to an EMPTY index (the transport's degradation
    /// contract), so the caller drives the negative case through this same method.
    ///
    /// # Errors
    /// The transport's typed error rendered to a string (a connect-level / non-200 / missing-credential fault).
    pub fn fetch_catalog(
        &self,
        base_url: &str,
        workspace_id: &str,
    ) -> Result<WireSkillIndex, String> {
        let client = UreqDeviceClient::new(
            base_url.to_owned(),
            device_credential(&self.fs, &self.layout()),
        );
        CatalogSource::fetch_catalog(&client, workspace_id).map_err(|e| format!("{e:?}"))
    }

    /// Drive the real `list --remote` MERGE over the REAL catalog transport: build a [`ops::RemoteScope`]
    /// over a live [`UreqDeviceClient`] holding this rig's per-workspace credential map and run `ops::list`,
    /// so the returned `remote_available` is the plane's credentialed catalog annotated with THIS install's
    /// on-disk follow-state (`follows.json` following flags + the tracked lock versions). `memberships` are
    /// the `(workspace_id, label)` catalog targets; `only` is the `--workspace` filter. Returns
    /// `(ListData, warnings)` — the per-workspace catalog faults ride the warnings, outside the pinned data.
    ///
    /// # Panics
    /// If the list errors (a hard wiring fault).
    #[must_use]
    pub fn list_remote(
        &self,
        base_url: &str,
        memberships: Vec<(String, String)>,
        only: Option<String>,
    ) -> (ListData, Vec<String>) {
        let catalog = UreqDeviceClient::new(
            base_url.to_owned(),
            device_credential(&self.fs, &self.layout()),
        );
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .expect("load-or-create device id");
        let inert_plane = InertPlane;
        let inert_follow = InertFollow;
        let ctx = Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id,
            layout: self.layout(),
            harness: &self.harness,
            plane: &inert_plane,
            follow: &inert_follow,
            roots: None,
        };
        let scope = ops::RemoteScope {
            catalog: &catalog,
            memberships,
            only,
        };
        let outcome = ops::list(&ctx, None, false, None, Some(scope))
            .expect("test_support: list --remote failed");
        (outcome.data, outcome.warnings)
    }
}

/// The RECONCILE rig (Leg G): an ENROLLED member home (instance/user/credentials + a possibly-empty
/// `follows.json`) over a real `~/.topos/` + a work root where delivered skills land through the REAL
/// `WorkHarness` adapter. Drives the GENUINE delivery-driven reconcile
/// ([`ops::pull_reconcile`]) over the REAL `ureq` [`crate::plane::DeliverySource`] against a loopback
/// plane — so an external e2e proves the whole "one delivery call answers what to have, the engine
/// converges" loop (new-arrival OFFER, update, withdraw, freeze, access-gone) without reaching the
/// client's `pub(crate)` internals. Mirrors [`FollowHarness`]/[`ContributeHarness`] conventions.
#[derive(Debug)]
pub struct ReconcileHarness {
    home: Scratch,
    work: Scratch,
    fs: RealFs,
    ids: RealIds,
    clock: RealClock,
    harness: WorkHarness,
}

impl ReconcileHarness {
    /// A fresh rig over unique temp dirs (cleaned on drop).
    #[must_use]
    pub fn new(tag: &str) -> Self {
        let work = Scratch::new(&format!("{tag}-rwork"));
        let harness = WorkHarness {
            work: work.0.clone(),
        };
        Self {
            home: Scratch::new(&format!("{tag}-rhome")),
            work,
            fs: RealFs,
            ids: RealIds,
            clock: RealClock,
            harness,
        }
    }

    fn layout(&self) -> Layout {
        Layout::new(&self.home.0)
    }

    /// Enroll a member EXACTLY as `follow` would leave it, but with ZERO followed skills: write
    /// `instance.json` (the plane base), `user.json` (the workspace membership), and
    /// `credentials.json` (the workspace Bearer credential); the `follows.json` is EMPTY so the first
    /// reconcile installs whatever the delivery delivers as a fresh arrival. Additional workspaces are
    /// added with [`add_workspace`](Self::add_workspace).
    pub fn enroll_member(&self, base_url: &str, workspace_id: &str, credential: &str) {
        let layout = self.layout();
        enroll::write_instance(
            &self.fs,
            &layout,
            &Instance {
                schema_version: 1,
                base_url: base_url.to_owned(),
            },
        )
        .expect("write instance.json");
        enroll::write_user(
            &self.fs,
            &layout,
            &UserDoc {
                schema_version: 1,
                principal: None,
                workspaces: vec![Membership {
                    workspace_id: workspace_id.to_owned(),
                    name: "test".to_owned(),
                    display_name: "Test".to_owned(),
                    enrolled_at: 1,
                }],
            },
        )
        .expect("write user.json");
        enroll::write_credentials(&self.fs, &layout, credential, "dev_e2e")
            .expect("write credentials.json");
        doc::write_doc_private(
            &self.fs,
            &layout.follows_path(),
            &Follows {
                schema_version: 1,
                follows: Vec::new(),
            },
        )
        .expect("write empty follows.json");
    }

    fn base_url(&self) -> String {
        enroll::read_instance(&self.fs, &self.layout())
            .expect("read instance.json")
            .expect("instance.json exists after enroll")
            .base_url
    }

    /// The wired transport (per-skill read creds from the on-disk follow-state × credentials) + the
    /// on-disk follow seam — rebuilt each call so a reconcile that just wrote `follows.json` is seen.
    fn transport(&self) -> (UreqPlane, FileFollow) {
        let follows = enroll::read_follows(&self.fs, &self.layout())
            .expect("read follows.json")
            .unwrap_or(Follows {
                schema_version: 1,
                follows: Vec::new(),
            });
        let plane = UreqPlane::new(
            self.base_url(),
            device_credential(&self.fs, &self.layout()),
            enroll::skill_workspaces(&follows),
        )
        .with_workspaces(enrolled_workspaces(&self.fs, &self.layout()));
        let follow = FileFollow::new(enroll::follow_contexts(&follows));
        (plane, follow)
    }

    /// Run the REAL delivery-driven reconcile over the loopback plane (one `GET …/delivery` per
    /// enrolled workspace + the `PUT …/report` fleet write). Returns `(PullData, warnings)` — the
    /// per-workspace faults (`ACCESS_GONE`, `PLANE_UNAVAILABLE`) ride the warnings.
    ///
    /// # Panics
    /// If the reconcile errors (a hard wiring fault — per-skill/per-workspace faults are isolated).
    #[must_use]
    pub fn reconcile(&self) -> (PullData, Vec<String>) {
        let (plane, follow) = self.transport();
        let device_id =
            crate::identity::load_or_create_device_id(&self.fs, &self.layout()).expect("device id");
        let ctx = Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id,
            layout: self.layout(),
            harness: &self.harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        let out = ops::pull_reconcile_with(&ctx, &plane, &ops::ReconcileOpts::default())
            .unwrap_or_else(|e| panic!("test_support: reconcile failed: {e}"));
        (out.data, out.warnings)
    }

    /// Accept an OFFERED first-receive (or land a pending update) for one skill by its catalog NAME —
    /// the explicit `topos pull <name>` consent that lands the bytes a reconcile only offered.
    ///
    /// # Panics
    /// If the targeted pull errors.
    #[must_use]
    pub fn accept(&self, name: &str) -> PullData {
        let (plane, follow) = self.transport();
        let device_id =
            crate::identity::load_or_create_device_id(&self.fs, &self.layout()).expect("device id");
        let ctx = Ctx {
            fs: &self.fs,
            ids: &self.ids,
            clock: &self.clock,
            device_id,
            layout: self.layout(),
            harness: &self.harness,
            plane: &plane,
            follow: &follow,
            roots: None,
        };
        ops::pull(
            &ctx,
            ops::PullScope::One {
                name: name.to_owned(),
                workspace: None,
                mode: ops::TargetMode::AcceptPending,
            },
        )
        .unwrap_or_else(|e| panic!("test_support: accept failed: {e}"))
        .data
    }

    /// Where the `WorkHarness` places `skill_id` (`<work>/<skill_id>`) — the agent dir a reconcile
    /// installs into and withdraws from.
    fn placement_dir(&self, skill_id: &str) -> PathBuf {
        self.work.0.join(skill_id)
    }

    /// The placed bundle for `skill_id`: `(relative path, mode & 0o777, bytes)`, sorted — a byte-exact
    /// witness of what landed.
    #[must_use]
    pub fn placement_files(&self, skill_id: &str) -> Vec<(String, u32, Vec<u8>)> {
        snapshot_dir(&self.placement_dir(skill_id))
    }

    /// Whether the agent placement dir for `skill_id` exists (present after an accept; GONE after an
    /// upstream withdrawal, INTACT after a person-level detach / freeze).
    #[must_use]
    pub fn placement_exists(&self, skill_id: &str) -> bool {
        self.placement_dir(skill_id).exists()
    }

    /// The followed skill's `sync.json` (the floor/applied state), or `None` before its baseline exists.
    #[must_use]
    pub fn sync_state(&self, skill_id: &str) -> Option<SyncState> {
        doc::read_doc(&self.fs, &self.layout().published(&sid(skill_id)).sync)
            .expect("read sync.json")
    }

    /// The follow-state `follows.json` holds, as `(skill_id, workspace_id, following)` in stored order.
    #[must_use]
    pub fn follows(&self) -> Vec<(String, String, bool)> {
        enroll::read_follows(&self.fs, &self.layout())
            .ok()
            .flatten()
            .map(|f| {
                f.follows
                    .into_iter()
                    .map(|e| (e.skill_id, e.workspace_id, e.following))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The number of versions (commits) the sidecar store for `skill_id` holds — a fetched version plus
    /// any snapshotted draft. The withdrawal-with-a-draft scenario asserts the draft commit was retained
    /// (the store keeps the bytes even as the agent dir is cleaned).
    #[must_use]
    pub fn store_version_count(&self, skill_id: &str) -> usize {
        let store_dir = self.layout().published(&sid(skill_id)).store;
        topos_gitstore::Store::open(&store_dir)
            .expect("open sidecar store")
            .list_versions()
            .expect("list sidecar versions")
            .len()
    }

    /// Simulate the LOCAL half of the `remove` verb (the verb itself lands later): flip `following = false` in `follows.json`
    /// and delete the agent placement dir — so the next reconcile sees an already-frozen entry (the
    /// server exclusion is driven separately via `Authority::exclude_device`).
    pub fn simulate_local_remove(&self, skill_id: &str) {
        enroll::set_following(&self.fs, &self.layout(), skill_id, false)
            .expect("set following false");
        let dir = self.placement_dir(skill_id);
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("remove placement dir");
        }
    }

    /// Resume following a locally-frozen skill (the LOCAL half of `follow <skill>`): flip
    /// `following = true` so the next reconcile re-attaches it (paired with the server-side
    /// `Authority::follow_skill` that lifts the device exclusion).
    pub fn resume_local_following(&self, skill_id: &str) {
        enroll::set_following(&self.fs, &self.layout(), skill_id, true)
            .expect("set following true");
    }

    /// Overwrite the placement with `files` — a local draft ahead of `current`, so a subsequent
    /// upstream withdrawal snapshots it into the sidecar store.
    pub fn edit_placement(&self, skill_id: &str, files: &[(&str, bool, &[u8])]) {
        write_tree(&self.placement_dir(skill_id), files);
    }
}

/// `follow`'s flags for the address-flow methods: the classic auto adoption, no workspace filter, no
/// prefixing, no kind-forced selectors — just the `--yes` toggle the describe/apply split turns on.
fn follow_opts(yes: bool) -> ops::FollowOpts {
    ops::FollowOpts {
        manual: false,
        workspace: None,
        yes,
        prefix_dirname: false,
        channels: Vec::new(),
        skills: Vec::new(),
        agents: Vec::new(),
    }
}

/// Project the engine's `FollowDescribe` into the public [`FollowDescribeView`] the external e2e reads.
fn describe_view(d: &crate::ops::FollowDescribe) -> FollowDescribeView {
    FollowDescribeView {
        workspace_id: d.workspace.workspace_id.clone(),
        workspace_name: d.workspace.name.clone(),
        address: d.workspace.address.clone(),
        role: d.role.clone(),
        invited_by: d.invited_by.clone(),
        enrolled_now: d.enrolled_now,
        targets: d
            .targets
            .iter()
            .map(|t| (t.kind.clone(), t.name.clone()))
            .collect(),
        installs: d
            .installs
            .iter()
            .map(|i| InstallView {
                skill_id: i.skill_id.clone(),
                name: i.name.clone(),
                version_id: i.version_id.clone(),
                bundle_digest: i.bundle_digest.clone(),
                via_channels: i.via_channels.clone(),
                via_direct: i.via_direct,
            })
            .collect(),
        preplaced_channels: d.preplaced_channels.clone(),
        all_devices_note: d.all_devices_note.clone(),
        reporting_note: d.reporting_note.clone(),
    }
}

/// Project a re-attach apply into the public [`FollowAppliedView`]: the one re-affirmed direct
/// follow is the subscription row, and the reinstalled current is the single install (a re-attach
/// is never a first-receive, so there is no via-channel attribution to carry).
fn reattach_applied_view(r: &crate::ops::Reattach) -> FollowAppliedView {
    FollowAppliedView {
        workspace_id: r.workspace_id.clone(),
        workspace_name: r.workspace_name.clone(),
        enrolled_now: false,
        subscribed: vec![("skill".to_owned(), r.name.clone())],
        installed: if r.installed {
            vec![InstallView {
                skill_id: r.skill_id.clone(),
                name: r.name.clone(),
                version_id: r.version_id.clone(),
                bundle_digest: r.bundle_digest.clone(),
                via_channels: Vec::new(),
                via_direct: true,
            }]
        } else {
            Vec::new()
        },
        warnings: r.warnings.clone(),
    }
}

/// Project the engine's `FollowApplied` into the public [`FollowAppliedView`].
fn applied_view(a: &crate::ops::FollowApplied) -> FollowAppliedView {
    FollowAppliedView {
        workspace_id: a.workspace_id.clone(),
        workspace_name: a.workspace_name.clone(),
        enrolled_now: a.enrolled_now,
        subscribed: a
            .subscribed
            .iter()
            .map(|t| (t.kind.clone(), t.name.clone()))
            .collect(),
        installed: a
            .installed
            .iter()
            .map(|i| InstallView {
                skill_id: i.skill_id.clone(),
                name: i.name.clone(),
                version_id: i.version_id.clone(),
                bundle_digest: i.bundle_digest.clone(),
                via_channels: i.via_channels.clone(),
                via_direct: i.via_direct,
            })
            .collect(),
        warnings: a.warnings.clone(),
    }
}

/// Read a rig's ONE device Bearer credential (`None` = signed out) — what every credentialed
/// transport presents.
fn device_credential(fs: &RealFs, layout: &Layout) -> Option<String> {
    enroll::read_credentials(fs, layout)
        .ok()
        .flatten()
        .map(|c| c.credential)
}

/// The enrolled workspace ids from `user.json` — the delivery lane's fan-out set.
fn enrolled_workspaces(fs: &RealFs, layout: &Layout) -> Vec<String> {
    enroll::read_user(fs, layout)
        .ok()
        .flatten()
        .map(|u| u.workspaces.into_iter().map(|m| m.workspace_id).collect())
        .unwrap_or_default()
}

/// Write `files` (`(path, is_executable, bytes)`) into a fresh `dir`, honoring the executable bit — the
/// local placeholder bundle an adopt tracks in place.
fn write_tree(dir: &Path, files: &[(&str, bool, &[u8])]) {
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).expect("create placement dir");
    for (rel, exec, bytes) in files {
        let dest = dir.join(rel);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).expect("create parent dir");
        }
        std::fs::write(&dest, bytes).expect("write placement file");
        let mode = if *exec { 0o755 } else { 0o644 };
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(mode))
            .expect("set placement file mode");
    }
}

/// Snapshot every file under `dir`: `(relative path, mode & 0o777, bytes)`, sorted by path.
fn snapshot_dir(dir: &Path) -> Vec<(String, u32, Vec<u8>)> {
    let mut out = Vec::new();
    fn walk(base: &Path, dir: &Path, out: &mut Vec<(String, u32, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .expect("a child path is under base")
                    .to_string_lossy()
                    .into_owned();
                let mode = std::fs::metadata(&path)
                    .expect("stat placement file")
                    .permissions()
                    .mode()
                    & 0o777;
                let bytes = std::fs::read(&path).expect("read placement file");
                out.push((rel, mode, bytes));
            }
        }
    }
    walk(dir, dir, &mut out);
    out.sort();
    out
}
