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
use topos_types::bootstrap::{DeploymentMode, VerifiedDomainStatus};
use topos_types::persisted::{PlacementMap, SyncState};
use topos_types::requests::WireSkillIndex;
use topos_types::results::{
    DiffData, ListData, ProposeData, PublishData, PullData, RevertData, ReviewData,
};
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::device_signer::DeviceSigner;
use crate::enroll::{
    self, CredentialEntry, Credentials, FollowEntry, FollowModeDoc, Follows, Instance, Membership,
    UserDoc,
};
use crate::fs_seam::RealFs;
use crate::ids::{IdSource, RealClock, RealIds};
use crate::plane::{
    CatalogSource, ContributeSource, EnrollSource, FollowContext, FollowMode, FollowSource,
    GovernanceSource, InertFollow, InertPlane, PlaneSource,
};
use crate::plane_http::{FileFollow, SkillCred, UreqDeviceClient, UreqPlane};
use crate::sidecar::Layout;
use crate::{doc, ops, scan};

/// The device id local commits (a never-shared draft snapshot) are authored under. Fixed + controlled-ASCII;
/// the plane never sees it (only the client's own genesis carries it), so any stable value works.
const DEVICE_ID: &str = "d_hero";

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

/// One followed skill's enrollment, as the harness holds it. The workspace credential is a secret
/// (redacted in `Debug`, mirroring `SkillCred`).
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
        };
        let added = ops::add(&ctx, &dir)
            .unwrap_or_else(|e| panic!("test_support: adopt of {skill_id} failed: {e}"));
        assert_eq!(
            added.skill_id, skill_id,
            "the fixed id source must mint the requested skill id"
        );

        // Record the placement EXACTLY as map.json holds it (canonicalized) — what materialize writes to.
        let map: PlacementMap =
            doc::read_doc(&self.fs, &self.layout().published(&sid(skill_id)).map)
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
        let creds: HashMap<String, SkillCred> = self
            .follows
            .iter()
            .map(|s| {
                (
                    s.skill_id.clone(),
                    SkillCred::new(s.workspace_id.clone(), s.credential.clone()),
                )
            })
            .collect();
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
                    },
                )
            })
            .collect();

        let plane = UreqPlane::new(base_url.to_owned(), creds);
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
        };

        let internal = match scope {
            Scope::AllFollowed => ops::PullScope::AllFollowed,
            Scope::Accept { name } => ops::PullScope::One {
                name,
                mode: ops::TargetMode::AcceptPending,
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
/// OpenClaw adapter** the same way (a temp stand-in home; the promote registers the bootstrap-inject
/// surface in `openclaw.json` + writes the topos-owned plugin file) — and [`new_hermes`](Self::new_hermes)
/// wires the **real Hermes adapter** over a temp `$HERMES_HOME`, so an e2e can prove the whole
/// second-machine story against a genuine adapter.
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
    /// and the promote registers the bootstrap-inject surface in `openclaw.json` + writes the topos-owned
    /// inject plugin file.
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
            return f(&OpenClaw::new(home.0.clone(), &self.fs));
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
    /// EXACTLY as the production composition root builds them. Keeps the TYPED error — the public wrappers
    /// stringify it; [`resume_expect_denied`](Self::resume_expect_denied) renders the production envelope.
    fn run_follow(
        &self,
        plane: &dyn PlaneSource,
        follow: &dyn FollowSource,
        link: Option<String>,
        opts: ops::FollowOpts,
    ) -> Result<topos_types::results::FollowData, crate::error::ClientError> {
        let enroll_connect = |base_url: &str| -> Box<dyn EnrollSource> {
            Box::new(UreqDeviceClient::new(base_url.to_owned(), HashMap::new()))
        };
        let plane_connect =
            |base_url: &str, creds: HashMap<String, SkillCred>| -> Box<dyn PlaneSource> {
                Box::new(UreqPlane::new(base_url.to_owned(), creds))
            };
        let connectors = ops::FollowConnectors {
            enroll: &enroll_connect,
            plane: &plane_connect,
        };
        // Production's `Command::Follow` mints the host device id (writing `host.json`) before the op, so the
        // enrollment writer can record the device key into it; mirror that here.
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
            };
            ops::follow(&ctx, &connectors, link, opts).map(|o| o.data)
        })
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
        let creds = crate::enroll::skill_creds(&follows, &creds_doc(&self.fs, &self.layout()));
        let contexts = crate::enroll::follow_contexts(&follows);
        let plane = UreqPlane::new(base_url.to_owned(), creds);
        let follow = FileFollow::new(contexts);
        let opts = ops::FollowOpts {
            manual: false,
            workspace: None,
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

    /// The unix permission bits of the `0600` device-key seed file (`None` if absent).
    #[must_use]
    pub fn device_key_mode(&self) -> Option<u32> {
        std::fs::metadata(self.layout().device_key_path())
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

    /// The raw `openclaw.json` in the stand-in home (`None` when absent or not in openclaw mode) — the
    /// e2e asserts the exact bootstrap-inject registration against it.
    #[must_use]
    pub fn openclaw_config_json(&self) -> Option<String> {
        let home = self.openclaw.as_ref()?;
        std::fs::read_to_string(home.0.join("openclaw.json")).ok()
    }

    /// The topos-owned inject plugin file's text in the stand-in home (`None` when absent or not in
    /// openclaw mode) — the e2e asserts the honest first-`topos`-touch labeling against it.
    #[must_use]
    pub fn openclaw_plugin(&self) -> Option<String> {
        let home = self.openclaw.as_ref()?;
        std::fs::read_to_string(home.0.join("topos-currency.mjs")).ok()
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
            enroll::skill_creds(&follows, &creds_doc(&self.fs, &self.layout())),
        );
        let follow = FileFollow::new(enroll::follow_contexts(&follows));
        let internal = match scope {
            Scope::AllFollowed => ops::PullScope::AllFollowed,
            Scope::Accept { name } => ops::PullScope::One {
                name,
                mode: ops::TargetMode::AcceptPending,
            },
            Scope::GoBack {
                name,
                version_id_hex,
            } => ops::PullScope::One {
                name,
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

    /// Drive a DIRECT `publish <skill>@<digest>` over the REAL transports — including the
    /// workspace-standup branch: on an un-enrolled rig the publish starts the standup device flow against
    /// `standup_base_url` (the explicit loopback base — the compiled-in hosted default is never consulted)
    /// and returns [`PublishResult::Pending`]; re-invoking the SAME call resumes (poll → redeem → promote →
    /// the publish continues in that same invocation). On an enrolled rig this is the ordinary publish.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn publish(&self, standup_base_url: &str, approve: &str) -> Result<PublishResult, String> {
        self.publish_impl(standup_base_url, approve, None)
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
        self.publish_impl(standup_base_url, approve, Some(workspace))
    }

    fn publish_impl(
        &self,
        standup_base_url: &str,
        approve: &str,
        workspace: Option<&str>,
    ) -> Result<PublishResult, String> {
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .map_err(|e| e.to_string())?;
        // The write connectors present the workspace Bearer credential, re-read FRESH from disk (a standup
        // publish writes credentials.json mid-invocation, during promotion). Enrollment is unauthenticated.
        let contribute = |b: &str| -> Box<dyn ContributeSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                creds_map(&self.fs, &self.layout()),
            ))
        };
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                creds_map(&self.fs, &self.layout()),
            ))
        };
        let standup_enroll = |b: &str| -> Box<dyn EnrollSource> {
            Box::new(UreqDeviceClient::new(b.to_owned(), HashMap::new()))
        };
        let standup = ops::StandupConnectors {
            enroll: &standup_enroll,
            base_url: standup_base_url.to_owned(),
        };
        // `publish` never reads ctx.plane (the enrolled write transport is built per-base inside the op),
        // so THAT read seam stays inert; the OK receipt's pointer is scope-checked, not verified against a
        // key. The FOLLOW seam must be REAL, though: an enrolled publish infers a followed skill's OWN
        // workspace from its follow entry (the pointer scope — never an ambient guess), the only correct
        // op scope once this install follows skills across several workspaces. Absent `follows.json`
        // (the un-enrolled standup branch) yields an empty seam that branch never consults.
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
            };
            match ops::publish(
                &ctx,
                &contribute,
                &governance,
                &standup,
                None, // roots — the harness adopts the skill before publishing (no auto-add)
                approve,
                false,
                None,
                workspace,
            )
            .map_err(|e| e.to_string())?
            {
                ops::PublishOutcome::Published(d) => Ok(PublishResult::Published(d)),
                ops::PublishOutcome::Proposed(d) => Ok(PublishResult::Proposed(d)),
                ops::PublishOutcome::Pending { data, resume_argv } => {
                    Ok(PublishResult::Pending { data, resume_argv })
                }
            }
        })
    }

    /// Drive the real `invite` verb: this (owner) rig signs the governance Invite op and POSTs it,
    /// returning the minted `/i/` link. `skills` pre-offers the named skill ids to `email`.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string (a non-owner is DENIED).
    pub fn invite(&self, email: &str, skills: &[&str]) -> Result<String, String> {
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .map_err(|e| e.to_string())?;
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                creds_map(&self.fs, &self.layout()),
            ))
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
            };
            ops::invite(
                &ctx,
                &governance,
                vec![email.to_owned()],
                None,
                skills.iter().map(|s| (*s).to_owned()).collect(),
                None,
            )
            .map(|d| d.invite_link)
            .map_err(|e| e.to_string())
        })
    }

    /// Drive the real `invite` verb with an EXPLICIT `--workspace <id>` (the global flag). Otherwise
    /// identical to [`invite`](Self::invite): signs the governance Invite op and POSTs it, returning the
    /// `/i/` link. This is the AMBIENT-verb selector for an install that follows skills across several
    /// workspaces — `invite` with no `--workspace` fails locally with a `WorkspaceSelection` there.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string (a non-owner is DENIED; an unjoined `--workspace` id is
    /// a local `WorkspaceSelection` that never reaches the plane).
    pub fn invite_in_workspace(
        &self,
        email: &str,
        skills: &[&str],
        workspace: &str,
    ) -> Result<String, String> {
        let device_id = crate::identity::load_or_create_device_id(&self.fs, &self.layout())
            .map_err(|e| e.to_string())?;
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                creds_map(&self.fs, &self.layout()),
            ))
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
            };
            ops::invite(
                &ctx,
                &governance,
                vec![email.to_owned()],
                None,
                skills.iter().map(|s| (*s).to_owned()).collect(),
                Some(workspace),
            )
            .map(|d| d.invite_link)
            .map_err(|e| e.to_string())
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
    pub fn memberships(&self) -> Vec<(String, Option<String>)> {
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

    /// The device public key this rig's signer mints (load-or-generate is idempotent) — for the
    /// server-side same-device / different-device claim-replay witnesses.
    #[must_use]
    pub fn device_pubkey(&self) -> [u8; 32] {
        DeviceSigner::load_or_generate(&self.fs, &self.layout())
            .expect("load-or-generate device key")
            .public_key()
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

    /// POST a token to `/v1/admin-claim` over the REAL transport (this rig's device key) — the
    /// cross-species witness surface. `Ok` carries the redeemed workspace id.
    ///
    /// # Errors
    /// The transport's typed error rendered to a string (a non-claim / dead token is refused uniformly).
    pub fn admin_claim_attempt(&self, base_url: &str, token: &str) -> Result<String, String> {
        let signer =
            DeviceSigner::load_or_generate(&self.fs, &self.layout()).map_err(|e| e.to_string())?;
        let client = UreqDeviceClient::new(base_url.to_owned(), HashMap::new());
        EnrollSource::admin_claim(&client, token, signer.public_key(), "e2e")
            .map(|r| r.workspace_id)
            .map_err(|e| e.to_string())
    }

    /// POST a token as the `invite_token` of a `/v1/device/authorize` start over the REAL transport —
    /// the other cross-species direction. `Ok` carries the session's user code.
    ///
    /// # Errors
    /// The transport's typed error rendered to a string (a non-invite / dead token is refused uniformly).
    pub fn device_authorize_attempt(&self, base_url: &str, token: &str) -> Result<String, String> {
        let signer =
            DeviceSigner::load_or_generate(&self.fs, &self.layout()).map_err(|e| e.to_string())?;
        let client = UreqDeviceClient::new(base_url.to_owned(), HashMap::new());
        EnrollSource::device_authorize(&client, token, signer.public_key(), "e2e")
            .map(|a| a.user_code)
            .map_err(|e| e.to_string())
    }
}

/// The result of a [`ContributeHarness::publish`] / [`FollowHarness::publish`]: `current` moved (a direct
/// publish), a proposal opened (`--propose`), or the un-enrolled workspace-standup branch is PENDING a
/// human sign-in. The public face of the client's internal `PublishOutcome`.
#[derive(Debug, Clone)]
pub enum PublishResult {
    /// A direct publish moved `current`.
    Published(PublishData),
    /// `--propose` opened a proposal (NEEDS_REVIEW); `current` did NOT move.
    Proposed(ProposeData),
    /// The un-enrolled standup branch is waiting on a human sign-in: nothing was published, `data.pending`
    /// carries the sign-in envelope, and re-invoking `resume_argv` (the SAME publish command) resumes.
    Pending {
        data: PublishData,
        resume_argv: Vec<String>,
    },
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

    /// The device public key this client's signer mints (load-or-generate is idempotent) — so the e2e can
    /// register it on the plane (`seed_device`) before driving a write.
    #[must_use]
    pub fn device_pubkey(&self) -> [u8; 32] {
        DeviceSigner::load_or_generate(&self.fs, &self.layout())
            .expect("load-or-generate device key")
            .public_key()
    }

    /// The device key id the plane re-derives + selects to authenticate this client's write (the presented
    /// credential — the op kind rides the route, nothing is signed).
    #[must_use]
    pub fn device_key_id(&self) -> String {
        DeviceSigner::load_or_generate(&self.fs, &self.layout())
            .expect("load-or-generate device key")
            .device_key_id()
            .to_owned()
    }

    /// Enroll this client EXACTLY as `follow` would: write `instance.json` (the plane base), `user.json`
    /// (the workspace), `credentials.json` (the workspace Bearer credential), and `follows.json` (one
    /// followed skill — pure subscription state), then adopt the skill under its exact id with
    /// `placeholder_files`. A subsequent [`pull`](Self::pull) fast-forwards onto the plane's current.
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
                deployment_mode: DeploymentMode::Cloud,
                enrollment_method: "device_code".to_owned(),
            },
        )
        .expect("write instance.json");
        enroll::write_user(
            &self.fs,
            &layout,
            &UserDoc {
                schema_version: 1,
                email: None,
                principal: None,
                workspaces: vec![Membership {
                    workspace_id: workspace_id.to_owned(),
                    display_name: Some("Test".to_owned()),
                    roles: Vec::new(),
                    verified_domain: None,
                    verified_domain_status: VerifiedDomainStatus::Unverified,
                    invite_rooted: true,
                    enrolled_at: 1,
                }],
            },
        )
        .expect("write user.json");
        std::fs::create_dir_all(layout.identity_dir()).expect("create identity dir");
        doc::write_doc_private(
            &self.fs,
            &layout.credentials_path(),
            &Credentials {
                schema_version: 1,
                credentials: vec![CredentialEntry {
                    workspace_id: workspace_id.to_owned(),
                    credential: credential.to_owned(),
                }],
            },
        )
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
                enroll::skill_creds(&follows, &creds_doc(&self.fs, &self.layout())),
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
        };
        let contribute = |b: &str| -> Box<dyn ContributeSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                creds_map(&self.fs, &self.layout()),
            ))
        };
        let governance = |b: &str| -> Box<dyn GovernanceSource> {
            Box::new(UreqDeviceClient::new(
                b.to_owned(),
                creds_map(&self.fs, &self.layout()),
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
            // The harness is ALWAYS enrolled, so the standup branch never fires; the connector panics
            // if a regression ever routes an enrolled publish into it, and the base is explicit (the
            // compiled-in hosted default is never consulted from tests).
            let standup_enroll = |_b: &str| -> Box<dyn crate::plane::EnrollSource> {
                panic!("an enrolled publish must never build a standup transport")
            };
            let standup = ops::StandupConnectors {
                enroll: &standup_enroll,
                base_url: "http://127.0.0.1:0".to_owned(),
            };
            match ops::publish(
                ctx, contribute, governance, &standup, None, approve, propose, None, None,
            )
            .map_err(|e| e.to_string())?
            {
                ops::PublishOutcome::Published(d) => Ok(PublishResult::Published(d)),
                ops::PublishOutcome::Proposed(d) => Ok(PublishResult::Proposed(d)),
                ops::PublishOutcome::Pending { .. } => {
                    Err("unexpected standup-pending publish from an enrolled harness".to_owned())
                }
            }
        })
    }

    /// Drive `review --approve | --reject` on `<skill>@<hash>`.
    ///
    /// # Errors
    /// The verb's typed error rendered to a string.
    pub fn review(&self, target: &str, approve: bool) -> Result<ReviewData, String> {
        self.with_write_ctx(|ctx, contribute, _gov| {
            ops::review(ctx, contribute, target, approve, None).map_err(|e| e.to_string())
        })
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
        let client =
            UreqDeviceClient::new(base_url.to_owned(), creds_map(&self.fs, &self.layout()));
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
        let catalog =
            UreqDeviceClient::new(base_url.to_owned(), creds_map(&self.fs, &self.layout()));
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
                deployment_mode: DeploymentMode::Cloud,
                enrollment_method: "device_code".to_owned(),
            },
        )
        .expect("write instance.json");
        enroll::write_user(
            &self.fs,
            &layout,
            &UserDoc {
                schema_version: 1,
                email: None,
                principal: None,
                workspaces: vec![Membership {
                    workspace_id: workspace_id.to_owned(),
                    display_name: Some("Test".to_owned()),
                    roles: Vec::new(),
                    verified_domain: None,
                    verified_domain_status: VerifiedDomainStatus::Unverified,
                    invite_rooted: true,
                    enrolled_at: 1,
                }],
            },
        )
        .expect("write user.json");
        std::fs::create_dir_all(layout.identity_dir()).expect("create identity dir");
        doc::write_doc_private(
            &self.fs,
            &layout.credentials_path(),
            &Credentials {
                schema_version: 1,
                credentials: vec![CredentialEntry {
                    workspace_id: workspace_id.to_owned(),
                    credential: credential.to_owned(),
                }],
            },
        )
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

    /// The workspace credential map (`workspace_id → credential`) from `credentials.json`.
    fn ws_creds(&self) -> HashMap<String, String> {
        creds_map(&self.fs, &self.layout())
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
        let creds = creds_doc(&self.fs, &self.layout());
        let plane = UreqPlane::new(self.base_url(), enroll::skill_creds(&follows, &creds))
            .with_workspace_credentials(self.ws_creds());
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
        };
        let out = ops::pull_reconcile(&ctx, &plane)
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
        };
        ops::pull(
            &ctx,
            ops::PullScope::One {
                name: name.to_owned(),
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

/// Read a rig's `credentials.json` (empty if absent) — the per-workspace credentials `skill_creds` joins.
fn creds_doc(fs: &RealFs, layout: &Layout) -> Credentials {
    enroll::read_credentials(fs, layout)
        .ok()
        .flatten()
        .unwrap_or(Credentials {
            schema_version: 1,
            credentials: Vec::new(),
        })
}

/// The per-workspace credential map (`workspace_id → credential`) the write/catalog transports present.
fn creds_map(fs: &RealFs, layout: &Layout) -> HashMap<String, String> {
    creds_doc(fs, layout).into_map()
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
