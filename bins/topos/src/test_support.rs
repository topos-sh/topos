//! Test-only public facade (feature `test-fixtures`) — drive the **real** pull engine over the **real**
//! `ureq` transport from an EXTERNAL integration crate (the HERO loopback), without exposing the client's
//! `pub(crate)` internals.
//!
//! Everything here is a thin wrapper over already-built, in-crate machinery: it lays down a `~/.topos/` for
//! a *never-pulled, followed* skill exactly as a real `add` + enrollment would (so the initial
//! `sync.json`/`lock.json`/`map.json` are produced by the genuine `ops::add`, never hand-faked), builds the
//! production `UreqPlane` + `FileFollow` + a real `Ctx`, and runs `ops::pull`. The HERO supplies the loopback
//! `base_url`, the pinned plane key, and the minted read token; it asserts on the returned
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

use topos_harness::{DiscoveredPlacement, HarnessAdapter, PlacementTarget};
use topos_types::persisted::{PlacementMap, SyncState};
use topos_types::results::PullData;
use topos_types::{CurrencyKind, HarnessId, TriggerReport, TriggerState};

use crate::ctx::Ctx;
use crate::fs_seam::RealFs;
use crate::ids::{IdSource, RealClock, RealIds};
use crate::plane::{FollowContext, FollowMode, InertFollow, InertPlane};
use crate::plane_http::{FileFollow, SkillCred, UreqPlane};
use crate::sidecar::Layout;
use crate::{doc, ops};

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

/// One followed skill's enrollment, as the harness holds it. The read token is a secret (redacted in
/// `Debug`, mirroring `SkillCred`/`FollowEntry`).
#[derive(Clone)]
struct FollowSpec {
    skill_id: String,
    workspace_id: String,
    read_token: String,
    mode: Follow,
}

impl std::fmt::Debug for FollowSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FollowSpec")
            .field("skill_id", &self.skill_id)
            .field("workspace_id", &self.workspace_id)
            .field("read_token", &"<redacted>")
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
    fn placement_for(&self, skill_id: &str, _d: Option<&DiscoveredPlacement>) -> PlacementTarget {
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
    /// enroll it as followed in `workspace_id` with `read_token`. `files` is the LOCAL placeholder bundle —
    /// it may differ from the plane's genesis (a first pull then fast-forwards onto the plane's bytes).
    ///
    /// `files` entries are `(bundle-relative path, is_executable, bytes)`.
    ///
    /// # Panics
    /// If the adopt fails (a test precondition error).
    pub fn adopt_followed(
        &mut self,
        skill_id: &str,
        workspace_id: &str,
        read_token: &str,
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
            plane_key: [0u8; 32],
            follow: &inert_follow,
        };
        let added = ops::add(&ctx, &dir)
            .unwrap_or_else(|e| panic!("test_support: adopt of {skill_id} failed: {e}"));
        assert_eq!(
            added.skill_id, skill_id,
            "the fixed id source must mint the requested skill id"
        );

        // Record the placement EXACTLY as map.json holds it (canonicalized) — what materialize writes to.
        let map: PlacementMap = doc::read_doc(&self.fs, &self.layout().published(skill_id).map)
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
            read_token: read_token.to_owned(),
            mode,
        });
    }

    /// Run the REAL pull engine over a REAL `UreqPlane` at `base_url`, with `plane_key` pinned as the
    /// signed-pointer trust root. Builds the transport credential map + the follow seam from the enrolled
    /// skills and a fresh `Ctx`, then dispatches `scope`.
    ///
    /// # Panics
    /// If the pull errors (the bare sweep isolates per-skill failures, so this is a hard wiring fault) or a
    /// `GoBack` hex id is malformed.
    #[must_use]
    pub fn run_pull(&self, base_url: &str, plane_key: [u8; 32], scope: Scope) -> PullData {
        let creds: HashMap<String, SkillCred> = self
            .follows
            .iter()
            .map(|s| {
                (
                    s.skill_id.clone(),
                    SkillCred::new(s.workspace_id.clone(), s.read_token.clone()),
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
            plane_key,
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
                    mode: ops::TargetMode::GoBack(hash),
                }
            }
        };
        ops::pull(&ctx, internal).unwrap_or_else(|e| panic!("test_support: pull failed: {e}"))
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
        doc::read_doc(&self.fs, &self.layout().published(skill_id).sync)
            .expect("read sync.json")
            .expect("sync.json exists for a followed skill")
    }
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
