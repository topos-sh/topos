//! The RECONCILE — what `update` runs. Two UNBLENDED scopes, each converged on its own recipe:
//!
//! - the **person scope** — the global manifest (`~/.topos/topos.toml`) when the file exists, else
//!   the implicit recipe (one feed row per connected workspace). A file is COMPLETE: only its rows
//!   deliver, and a workspace's feed flows iff a feed row says so. Bytes land in the home harness
//!   dirs, state in `~/.topos/`.
//! - the **project scope** — the NEAREST `topos.toml` walking up from the working directory, taken
//!   WHOLE (no ancestor merging). Bytes land inside the checkout (each dir self-ignoring), state in
//!   the project's own store.
//!
//! There is no cross-scope shadowing or subtraction: each scope's delivered set is the union of its
//! rows' resolutions (or its feeds), minus its `"off"` switches, deduped by bundle identity — so the
//! same bundle demanded at both scopes lands twice, with two state trees and no interaction. Within
//! a scope the order is explicit THINGS, then SETS (a channel, a repo), then the feeds: an explicit
//! row's version and fields beat any set's delivery of the same identity, and a set delivers its
//! curator's current truth.
//!
//! Delivery is silent — the login was the acceptance — and every honest line here is phrased from
//! LOCAL knowledge (which file asked; which login is missing), never from a server confirmation: the
//! plane answers uniformly, so a miss can never be read as an existence claim. A manifest the grammar
//! refuses FREEZES its whole scope (no delivery, no cleaning): the failure mode of a typo must be
//! keeping bytes, never dropping them.
//!
//! The sweep also maintains the OFFLINE DELIVERY CACHE (`state/sync_status.json`): per workspace what
//! the plane last served (with the attribution each row carries and the caller's declines), which
//! `status`/`list` read without a network call and [`CacheFollow`] is built over.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use topos_core::digest::to_hex;
use topos_gitstore::Store;
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::persisted::{Lock, PlacementMap, SwapCapability, SyncState};
use topos_types::requests::{WireChannelIndex, WireSkillIndex, WireSkillIndexEntry};
use topos_types::results::{PullAction, PullData, PullSkill, WorkspaceSyncReport};
use topos_types::{CurrentRecord, PointerScope, WIRE_SCHEMA_VERSION, WireCurrentRecord};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::id::SkillId;
use crate::manifest::keys::KeyShape;
use crate::manifest::scopes::{self, PlanRow, ResolvedScope, ScopePlan};
use crate::plane::{
    DeliverySkill, DeliverySnapshot, DirectorySource, FollowContext, FollowMode, FollowSource,
    LinkStatus, PlaneError, PlaneSource, ReconcileTransport,
};
use crate::sessions::{self, SESSION_ACTIVE, SESSION_ENDED, SESSION_PENDING, Session};
use crate::sync_status::{self, DeliveredSkill, WorkspaceSync};
use crate::{doc, placement, sidecar};

use super::pull::{PullOutcome, StaleReason, UnreachableWorkspace};
use super::sync_engine::{self, Invocation};

/// The per-session transports the reconcile drives: the byte/delivery lane (one `UreqPlane` under
/// the session's credential) and the directory lane (catalog + channel reads).
pub(crate) struct SessionTransports {
    pub plane: Box<dyn ReconcileTransport>,
    pub directory: Box<dyn DirectorySource>,
    /// The contribute-write lane (publish / propose / revert / review) under the same credential.
    pub contribute: Box<dyn crate::plane::ContributeSource>,
    /// The governance lane (invitations; the session self-revoke). Consumed by the invite fold.
    pub governance: Box<dyn crate::plane::GovernanceSource>,
}

/// Builds the transports for ONE session (per-workspace credentials — the session model).
pub(crate) type SessionConnect<'a> = dyn Fn(&Session) -> SessionTransports + 'a;

/// Which scope(s) ONE `update` reconciles — the scope rule made explicit. Every verb acts on where
/// you stand, so a hand-run update converges the scope the invocation is standing in and leaves the
/// other one's stores, placements and agent dirs alone; only the background sweep covers both,
/// because silent delivery has to reach everything the machine holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum UpdateScope {
    /// Where the invocation stands: the project scope when a manifest covers the cwd, else the
    /// machine.
    #[default]
    Here,
    /// The machine scope only (`-g`), even from inside a project.
    Machine,
    /// Both scopes — the hook sweep's mode (`--quiet`): silent delivery covers everything, always.
    Both,
}

/// How a reconcile behaves.
#[derive(Default)]
pub(crate) struct ManifestUpdateOpts {
    /// Targeted names/references (`topos update <name>…`); empty = the full sweep. A target
    /// narrows WITHIN the driven scope(s) — a name only the other scope demands is unmatched.
    pub targets: Vec<String>,
    /// Ack the delivered notices (the interactive / `--json` update); the quiet hook fetches
    /// WITHOUT acking, so nothing is marked read that no one narrated.
    pub ack_notices: bool,
    /// `--rebuild`: absorb every edited copy into its store, drop the recorded placement dirs, and
    /// let the ordinary sweep re-project them from the store. The absorb-then-drop ORDER is the
    /// whole guarantee — a rebuild must never be a way to lose an edit. Gated per scope, like
    /// everything else this run drives.
    pub rebuild: bool,
    /// Which scope(s) to DRIVE (see [`UpdateScope`]). Both plans are still read either way — the
    /// selector decides which ones converge, clean, and disclose.
    pub scope: UpdateScope,
}

/// The scope(s) this run resolved to drive, and how the receipt names them. Two booleans rather
/// than the request enum: the arms below gate on what is actually being converged, so a
/// "not driven" scope can never be read as "nothing demanded" by a cleaner.
struct Driven {
    project: bool,
    person: bool,
}

impl Driven {
    /// Resolve the request against what covers the cwd. A project manifest the grammar REFUSED
    /// still counts as covering it: the parse failure freezes that scope loudly, and silently
    /// updating the machine instead would answer a typo by converging the wrong tree.
    fn resolve(scope: UpdateScope, project_here: bool) -> Self {
        match scope {
            UpdateScope::Here => Driven {
                project: project_here,
                person: !project_here,
            },
            UpdateScope::Machine => Driven {
                project: false,
                person: true,
            },
            UpdateScope::Both => Driven {
                project: true,
                person: true,
            },
        }
    }

    /// The receipt's scope name: `"project <dir>"`, `"machine"`, or `"both"`.
    fn label(&self, project_dir: Option<&Path>) -> String {
        match (self.project, self.person) {
            (true, true) => "both".to_owned(),
            (true, false) => project_dir.map_or_else(
                || "project".to_owned(),
                |d| format!("project {}", d.display()),
            ),
            _ => "machine".to_owned(),
        }
    }
}

/// One session's runtime state for this sweep.
struct SessionRun {
    session: Session,
    transports: SessionTransports,
    /// The delivery answer (`None` = no fresh delivery this run, whatever the fault — the feed is
    /// cache-fed and the engine converges from the local store).
    snapshot: Option<DeliverySnapshot>,
    /// Lazily fetched catalog (row resolution). `Some(None)` = fetch failed this run. `Rc`, so the
    /// per-row reads share ONE fetch instead of deep-cloning the index each time.
    skills_index: std::cell::RefCell<Option<Option<Rc<WireSkillIndex>>>>,
    channels_index: std::cell::RefCell<Option<Option<Rc<WireChannelIndex>>>>,
}

/// The offline-degraded run: no snapshot, so the feed is fed from the local delivery cache and the
/// converge still runs. Every no-fresh-delivery arm builds exactly this.
fn offline_run(s: &Session, transports: SessionTransports) -> SessionRun {
    SessionRun {
        session: s.clone(),
        transports,
        snapshot: None,
        skills_index: std::cell::RefCell::new(None),
        channels_index: std::cell::RefCell::new(None),
    }
}

/// The quiet hook's staleness signal for a session that got no fresh delivery. ONE place pairs the
/// id with the name: the cache lookup is keyed by the id, the line says the name, and a swap between
/// them silently kills the warning (a missed lookup reads as "not stale").
fn stale_signal(s: &Session, reason: StaleReason) -> UnreachableWorkspace {
    UnreachableWorkspace {
        workspace_id: s.workspace_id.clone(),
        workspace_name: s.workspace_name.clone(),
        reason,
    }
}

impl SessionRun {
    /// The catalog, fetched once per run (a failure caches as `None` — one warning, not N).
    fn catalog(&self, warnings: &mut Vec<String>) -> Option<Rc<WireSkillIndex>> {
        let mut slot = self.skills_index.borrow_mut();
        if slot.is_none() {
            let fetched = match self
                .transports
                .directory
                .skills_index(&self.session.workspace_id)
            {
                Ok(ix) => Some(Rc::new(ix)),
                Err(e) => {
                    warnings.push(format!(
                        "CATALOG_UNAVAILABLE {}: {}",
                        self.session.workspace_name,
                        crate::render::safe_message(&e)
                    ));
                    None
                }
            };
            *slot = Some(fetched);
        }
        slot.as_ref().and_then(Clone::clone)
    }

    fn channels(&self, warnings: &mut Vec<String>) -> Option<Rc<WireChannelIndex>> {
        let mut slot = self.channels_index.borrow_mut();
        if slot.is_none() {
            let fetched = match self
                .transports
                .directory
                .channels_index(&self.session.workspace_id)
            {
                Ok(ix) => Some(Rc::new(ix)),
                Err(e) => {
                    warnings.push(format!(
                        "CHANNELS_UNAVAILABLE {}: {}",
                        self.session.workspace_name,
                        crate::render::safe_message(&e)
                    ));
                    None
                }
            };
            *slot = Some(fetched);
        }
        slot.as_ref().and_then(Clone::clone)
    }
}

/// A follow seam materialized from the CURRENT deliveries + the offline cache — what the engine's
/// person-scope plan reads for workspace provenance. The delivered set IS the standing state; demand
/// lives in the manifests.
pub(crate) struct CacheFollow {
    entries: Vec<(String, FollowContext)>,
}

impl CacheFollow {
    /// Build from the offline delivery cache (`state/sync_status.json`) — the not-dialing form
    /// every verb outside the reconcile uses.
    pub(crate) fn load(fs: &dyn crate::fs_seam::FsOps, layout: &crate::sidecar::Layout) -> Self {
        let status = sync_status::read(fs, layout).unwrap_or_default();
        let mut entries = Vec::new();
        for (ws, entry) in &status.workspaces {
            for (skill_id, ds) in &entry.delivered {
                if ds.withdrawn {
                    continue;
                }
                entries.push((
                    skill_id.clone(),
                    FollowContext {
                        workspace_id: ws.clone(),
                        mode: FollowMode::Auto,
                        review_required: ds.review_required,
                        following: true,
                        agents: Vec::new(),
                        excluded_agents: Vec::new(),
                    },
                ));
            }
        }
        Self { entries }
    }

    fn upsert(&mut self, skill_id: &str, follow: FollowContext) {
        self.entries.retain(|(id, _)| id != skill_id);
        self.entries.push((skill_id.to_owned(), follow));
    }
}

impl FollowSource for CacheFollow {
    fn followed(&self) -> Vec<(String, FollowContext)> {
        self.entries.clone()
    }
}

/// The SESSION-ROUTED plane — the app ctx's `PlaneSource` when the installation runs on sessions:
/// each per-skill read routes to the session lane of the workspace the skill belongs to (the offline
/// delivery cache supplies the map; `bind_skill` teaches new pairs mid-run). A skill no session
/// covers answers "not served", exactly like the inert source.
pub(crate) struct SessionRoutedPlane {
    lanes: Vec<(String, Box<dyn ReconcileTransport>)>,
    skill_ws: std::cell::RefCell<HashMap<String, String>>,
}

impl SessionRoutedPlane {
    /// Build from the live sessions + the offline delivery cache.
    pub(crate) fn load(
        fs: &dyn crate::fs_seam::FsOps,
        layout: &crate::sidecar::Layout,
        connect: &SessionConnect<'_>,
    ) -> Self {
        let mut lanes = Vec::new();
        if let Ok(all) = sessions::read_sessions(fs, layout) {
            for s in &all.sessions {
                if s.status == SESSION_ENDED {
                    continue;
                }
                lanes.push((s.workspace_id.clone(), connect(s).plane));
            }
        }
        let mut skill_ws = HashMap::new();
        if let Ok(status) = sync_status::read(fs, layout) {
            for (ws, entry) in &status.workspaces {
                for skill_id in entry.delivered.keys() {
                    skill_ws.insert(skill_id.clone(), ws.clone());
                }
            }
        }
        Self {
            lanes,
            skill_ws: std::cell::RefCell::new(skill_ws),
        }
    }

    fn lane_of(&self, skill_id: &str) -> Option<&dyn PlaneSource> {
        let ws = self.skill_ws.borrow().get(skill_id).cloned()?;
        self.lanes.iter().find(|(w, _)| *w == ws).map(|(_, t)| {
            let p: &dyn PlaneSource = &**t;
            p
        })
    }
}

impl PlaneSource for SessionRoutedPlane {
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<crate::plane::KnownCurrent>,
    ) -> Result<crate::plane::PointerFetch, PlaneError> {
        match self.lane_of(skill_id) {
            Some(lane) => {
                lane.bind_skill(&self.skill_ws.borrow()[skill_id], skill_id);
                lane.get_current(skill_id, known)
            }
            None => Err(PlaneError::NotFound),
        }
    }
    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<crate::plane::FetchedVersion, PlaneError> {
        match self.lane_of(skill_id) {
            Some(lane) => {
                lane.bind_skill(&self.skill_ws.borrow()[skill_id], skill_id);
                lane.fetch_version(skill_id, version_id)
            }
            None => Err(PlaneError::NotFound),
        }
    }
    fn list_open_proposals(&self, skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        match self.lane_of(skill_id) {
            Some(lane) => {
                lane.bind_skill(&self.skill_ws.borrow()[skill_id], skill_id);
                lane.list_open_proposals(skill_id)
            }
            None => Ok(Vec::new()),
        }
    }
    fn bind_skill(&self, workspace_id: &str, skill_id: &str) {
        self.skill_ws
            .borrow_mut()
            .insert(skill_id.to_owned(), workspace_id.to_owned());
        if let Some((_, lane)) = self.lanes.iter().find(|(w, _)| w == workspace_id) {
            PlaneSource::bind_skill(&**lane, workspace_id, skill_id);
        }
    }
}

// =================================================================================================
// The sweep's accumulating state.
// =================================================================================================

/// Everything one sweep accumulates across its scopes.
#[derive(Default)]
struct Sweep {
    rows: Vec<PullSkill>,
    /// FAILURES only — the isolated per-skill faults the receipt counts and the renderer calls
    /// failed. A line that describes something that WORKED belongs in `disclosures`, or a clean
    /// run reports itself as broken.
    warnings: Vec<String>,
    /// Successful facts worth stating — the settled-draft fan-out, a cross-scope version split.
    /// They ride the same `--json` `warnings` array (one stable machine channel) but are never
    /// counted as failures.
    disclosures: Vec<String>,
    /// `(scope label, bundle identity)` already reconciled — the ONE dedupe key. Scopes are
    /// unblended, so the same identity may appear once per scope.
    synced: HashSet<(String, String)>,
    /// Bundle ids an EXPLICIT row delivered (any scope) — what the declined-override line reads.
    explicit: HashSet<String>,
    /// `(workspace id, skill id, cache row)` per delivery this sweep recorded through a manifest row.
    delivered: Vec<(String, String, DeliveredSkill)>,
    /// `(workspace id, channel)` expansions that FAILED this run — their members freeze, and their
    /// cache rows must survive the sweep that could not see the member list.
    failed_channels: HashSet<(String, String)>,
    /// Feed ADDRESSES whose empty serve is already disclosed. A feed may be adopted by more than
    /// one recipe; the fact is the workspace's, not a scope's, so it is said once per run.
    empty_feeds: HashSet<String>,
    /// Project dirs owning a failed expansion — the cleaner freezes everything under them.
    unexpanded: Vec<PathBuf>,
    /// Per scope label, every NAME its rows MENTION — delivered or not. A name a row still names is
    /// never cleaned, so a transient failure can not read as a drop.
    mentioned: BTreeMap<String, HashSet<String>>,
}

impl Sweep {
    /// Claim an identity for a scope. `false` = something already reconciled it there.
    fn claim(&mut self, label: &str, identity: &str) -> bool {
        self.synced.insert((label.to_owned(), identity.to_owned()))
    }

    fn claimed(&self, label: &str, identity: &str) -> bool {
        self.synced
            .contains(&(label.to_owned(), identity.to_owned()))
    }

    /// The identities this scope reconciled.
    fn synced_in(&self, label: &str) -> HashSet<&str> {
        self.synced
            .iter()
            .filter(|(l, _)| l == label)
            .map(|(_, id)| id.as_str())
            .collect()
    }

    fn mention(&mut self, label: &str, name: &str) {
        self.mentioned
            .entry(label.to_owned())
            .or_default()
            .insert(name.to_owned());
    }

    /// Every name ANY scope of this sweep mentions.
    fn mentioned_anywhere(&self) -> HashSet<&str> {
        self.mentioned
            .values()
            .flat_map(|s| s.iter().map(String::as_str))
            .collect()
    }

    fn push(&mut self, row: PullSkill) {
        self.rows.push(row);
    }

    /// Record one failed set expansion: its members are unknowable this run, so nothing under the
    /// owning project dir may be treated as undemanded. `workspace_id` is the SESSION's opaque id
    /// (two servers may both hold an `acme` — the name alone would freeze the wrong one); `None`
    /// when no session resolves the reference at all (nothing dials for it, so no cache row is
    /// written either — only the dir freeze matters there).
    fn note_set_failure(&mut self, workspace_id: Option<&str>, set: &str, scope: &ResolvedScope) {
        if let Some(ws) = workspace_id {
            self.failed_channels.insert((ws.to_owned(), set.to_owned()));
        }
        if let ResolvedScope::Project { dir } = scope {
            self.unexpanded.push(dir.clone());
        }
    }
}

/// The shared seams every arm reads — grouped so the per-arm signatures stay legible.
struct Env<'a> {
    ctx: &'a Ctx<'a>,
    runs: &'a [SessionRun],
    follow: &'a CacheFollow,
    /// The forge lane. `None` on the background sweep: git rows move only on an explicit update.
    git: Option<&'a dyn crate::git_source::GitTarballSource>,
    prior: &'a sync_status::SyncStatus,
    /// One tarball per repo per sweep (a repo named by both a set and a skill row fetches once).
    repos: std::cell::RefCell<Vec<(String, Rc<Vec<u8>>)>>,
}

/// One scope under reconciliation: its recipe, where its bytes go, and what receipts call it.
struct ScopeCtx<'a> {
    scope: ResolvedScope,
    label: String,
    plan: &'a ScopePlan,
}

/// What `update <target>…` narrowed the sweep to (empty = everything).
struct Targets {
    wanted: Vec<String>,
    matched: HashSet<String>,
}

impl Targets {
    fn new(raw: &[String]) -> Self {
        Self {
            wanted: raw.to_vec(),
            matched: HashSet::new(),
        }
    }

    /// Whether the sweep covers something spelled any of `candidates` — always true for the bare
    /// full sweep. Records the hit, so an unmatched target can be named at the end.
    fn hit(&mut self, candidates: &[&str]) -> bool {
        if self.wanted.is_empty() {
            return true;
        }
        let mut found = false;
        for c in candidates {
            if self.wanted.iter().any(|w| w == c) {
                self.matched.insert((*c).to_owned());
                found = true;
            }
        }
        found
    }

    /// The first target nothing in the sweep answered.
    fn unmatched(&self) -> Option<&str> {
        self.wanted
            .iter()
            .find(|w| !self.matched.contains(*w))
            .map(String::as_str)
    }
}

// =================================================================================================
// The entry point.
// =================================================================================================

/// The reconcile (see the module doc). Returns the [`PullOutcome`] shape the hook and the `update`
/// finishers consume — `access_gone` carries the sessions that answered the uniform 404 (ended
/// server-side), `unreachable` the sessions that got no fresh delivery for any other reason — the
/// server unreached, the exchange unsuccessful, or the answer unreadable — each tagged with which,
/// since only the first is the network's doing.
///
/// # Errors
/// A session-file read failure, or an unmatched `update <target>` (the typed refusal names the fix).
pub(crate) fn manifest_update(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: Option<&dyn crate::git_source::GitTarballSource>,
    opts: &ManifestUpdateOpts,
) -> Result<PullOutcome, ClientError> {
    let mut sweep = Sweep::default();
    let mut access_gone: Vec<String> = Vec::new();
    let mut unreachable: Vec<UnreachableWorkspace> = Vec::new();
    let mut notices = Vec::new();
    let mut proposals_awaiting: u32 = 0;

    let prior_sync = match sync_status::read(ctx.fs, &ctx.layout) {
        Ok(s) => s,
        Err(e) => {
            sweep
                .warnings
                .push(format!("SYNC_STATUS_UNREADABLE: {}", e.detail()));
            sync_status::SyncStatus::default()
        }
    };

    // ---- 1. Dial each live session's delivery. ----
    let all_sessions = sessions::read_sessions(ctx.fs, &ctx.layout)?;
    let mut runs: Vec<SessionRun> = Vec::new();
    for s in &all_sessions.sessions {
        if s.status == SESSION_ENDED {
            continue; // the one typed line printed when it flipped; login is the way back
        }
        let transports = connect(s);
        match transports.plane.fetch_delivery(&s.workspace_id) {
            Ok(snap) if snap.link_status == LinkStatus::Pending => {
                // No data flows over a pending session — skip QUIETLY (a `status`-visible fact;
                // delivery starts automatically after an owner approves).
                let _ = sessions::set_session_status(
                    ctx.fs,
                    &ctx.layout,
                    &s.host,
                    &s.workspace_id,
                    SESSION_PENDING,
                );
                continue;
            }
            Ok(snap) => {
                // A delivering session self-heals a locally-recorded pending wait.
                let _ = sessions::set_session_status(
                    ctx.fs,
                    &ctx.layout,
                    &s.host,
                    &s.workspace_id,
                    SESSION_ACTIVE,
                );
                proposals_awaiting = proposals_awaiting
                    .saturating_add(u32::try_from(snap.proposals_awaiting).unwrap_or(u32::MAX));
                runs.push(SessionRun {
                    session: s.clone(),
                    transports,
                    snapshot: Some(snap),
                    skills_index: std::cell::RefCell::new(None),
                    channels_index: std::cell::RefCell::new(None),
                });
            }
            Err(PlaneError::NotFound) => {
                // The whole session answered the uniform 404: revoked, the seat removed, or the
                // workspace gone — indistinguishable by design. Mark it ended locally so the line
                // prints once; every copy stays in place (bytes are yours; `login` re-connects).
                sweep.warnings.push(format!(
                    "SESSION_ENDED {}: this session no longer has access (ended, removed, or \
                     gone); its skills stay in place — reconnect with `topos login {}/{}`",
                    s.workspace_name, s.host, s.workspace_name
                ));
                access_gone.push(s.workspace_name.clone());
                let _ = sessions::set_session_status(
                    ctx.fs,
                    &ctx.layout,
                    &s.host,
                    &s.workspace_id,
                    SESSION_ENDED,
                );
            }
            // The three no-fresh-delivery faults degrade IDENTICALLY: the feed falls back to the
            // OFFLINE CACHE below, so a dead network keeps the local converge working (and the hook
            // never wedges a session start); dropping the scope's demand would let the cleaner treat
            // its items as undemanded. They differ ONLY in what the person is told — the staleness
            // nudge is true of all three, but blaming the network for a 500 or for garbled bytes
            // would send someone the wrong way.
            Err(PlaneError::Unreachable(m)) => {
                sweep
                    .warnings
                    .push(format!("PLANE_UNAVAILABLE {}: {m}", s.workspace_name));
                unreachable.push(stale_signal(s, StaleReason::Unreachable));
                runs.push(offline_run(s, transports));
            }
            Err(PlaneError::Unavailable(m)) => {
                sweep
                    .warnings
                    .push(format!("PLANE_UNAVAILABLE {}: {m}", s.workspace_name));
                unreachable.push(stale_signal(s, StaleReason::Unavailable));
                runs.push(offline_run(s, transports));
            }
            Err(PlaneError::Malformed(m)) => {
                sweep
                    .warnings
                    .push(format!("WIRE_INVALID {}: {m}", s.workspace_name));
                unreachable.push(stale_signal(s, StaleReason::Malformed));
                runs.push(offline_run(s, transports));
            }
        }
    }

    // ---- 2. Build the two scope plans. A file the grammar refuses freezes its scope WHOLE. ----
    let connected: Vec<(String, String)> = runs
        .iter()
        .map(|r| (r.session.host.clone(), r.session.workspace_name.clone()))
        .collect();
    let person: Option<ScopePlan> = match scopes::person_plan(ctx.fs, &ctx.layout, &connected) {
        Ok(p) => Some(p),
        Err(e) => {
            sweep
                .warnings
                .push(format!("MANIFEST_INVALID {}", e.detail()));
            None
        }
    };
    let cwd = ctx.roots.as_ref().and_then(|r| r.cwd.clone());
    let home = ctx.roots.as_ref().map(|r| r.home.clone());
    let mut project: Option<(PathBuf, ScopePlan)> = None;
    let mut project_frozen = false;
    if let Some(cwd) = &cwd {
        match scopes::nearest_project_plan(ctx.fs, cwd, home.as_deref()) {
            Ok(found) => project = found,
            Err(e) => {
                sweep
                    .warnings
                    .push(format!("MANIFEST_INVALID {}", e.detail()));
                project_frozen = true;
            }
        }
    }
    // Which scope(s) this run DRIVES. Both plans above are read either way — the read is what
    // makes the freeze warnings and the cross-scope bookkeeping honest; the selector below decides
    // only which of them converges, cleans, and discloses. A frozen project file still COVERS the
    // cwd, so `Here` stays on it (its dir comes from the walk, since the failed parse produced no
    // plan to carry one).
    let project_dir: Option<PathBuf> = match (&project, project_frozen) {
        (Some((dir, _)), _) => Some(dir.clone()),
        (None, true) => cwd
            .as_deref()
            .and_then(|c| scopes::nearest_manifest_dir(ctx.fs, c, home.as_deref())),
        (None, false) => None,
    };
    let driven = Driven::resolve(opts.scope, project_dir.is_some());
    let scope_label = driven.label(project_dir.as_deref());

    // Every manifest dir up the chain — NOT a resolution input (nearest wins whole); the store
    // surfaces (lazy recovery, the pre-1.0 handover) still visit each one.
    let manifest_dirs: Vec<PathBuf> = match &cwd {
        Some(cwd) => scopes::manifest_dirs_up(ctx.fs, cwd, home.as_deref()),
        None => Vec::new(),
    };

    // Recovery is LAZY for project stores, mirroring manifest discovery: a store is swept exactly
    // when a run visits its project. The home store's own recovery already ran at command start.
    let now_millis = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    for pd in &manifest_dirs {
        if let Some(playout) = sidecar::existing_project_store(ctx.fs, pd)
            && let Err(e) = sidecar::recover(ctx.fs, &playout, now_millis, &mut sweep.warnings)
        {
            sweep.warnings.push(format!(
                "STORE_RECOVERY_FAILED {}: {}",
                pd.display(),
                e.detail()
            ));
        }
    }
    // Pre-1.0 handover: HOME-store map rows that point into the ACTIVE project are the OLD
    // blended model's leftovers — dropped (bytes stay in place) only once the project store has
    // verifiably adopted the skill. ONLY the nearest, parsed manifest's dir participates: a
    // parse failure froze the scope (a typo must keep state, never retire it), and an ancestor a
    // nearer file shadows gets no project pass this run, so nothing may retire toward it.
    let handover_dirs: Vec<PathBuf> = if project_frozen {
        Vec::new()
    } else {
        project.iter().map(|(dir, _)| dir.clone()).collect()
    };
    handover_legacy_project_rows(ctx, &handover_dirs, &mut sweep.warnings);

    // The follow seam for this run: current deliveries first, the cache behind them.
    let mut follow = CacheFollow::load(ctx.fs, &ctx.layout);
    for run in &runs {
        if let Some(snap) = &run.snapshot {
            for ds in &snap.skills {
                follow.upsert(
                    &ds.skill_id,
                    FollowContext {
                        workspace_id: run.session.workspace_id.clone(),
                        mode: FollowMode::Auto,
                        review_required: ds.review_required,
                        following: true,
                        agents: Vec::new(),
                        excluded_agents: Vec::new(),
                    },
                );
            }
        }
    }

    let env = Env {
        ctx,
        runs: &runs,
        follow: &follow,
        git,
        prior: &prior_sync,
        repos: std::cell::RefCell::new(Vec::new()),
    };

    // ---- 3. `--rebuild`, BEFORE the fan-out: absorb, then drop, then let the sweep re-project.
    // A rebuild rebuilds exactly what this run converges: re-projecting a store no scope drives
    // would drop placement dirs nothing is about to write back.
    if opts.rebuild {
        if driven.person {
            rebuild_store(ctx, &ctx.layout, &mut sweep.warnings);
        }
        if driven.project
            && let Some((dir, _)) = &project
            && let Some(playout) = sidecar::existing_project_store(ctx.fs, dir)
        {
            let pctx = super::pull::ctx_with_layout(ctx, &playout);
            rebuild_store(&pctx, &playout, &mut sweep.warnings);
        }
    }

    // ---- 4. Reconcile the driven scope(s), unblended. ----
    let mut targets = Targets::new(&opts.targets);
    if driven.project
        && let Some((dir, plan)) = &project
    {
        let sc = ScopeCtx {
            scope: ResolvedScope::Project { dir: dir.clone() },
            label: ResolvedScope::Project { dir: dir.clone() }.label(),
            plan,
        };
        reconcile_scope(&env, &sc, &mut targets, &mut sweep);
    }
    if driven.person
        && let Some(plan) = &person
    {
        let sc = ScopeCtx {
            scope: ResolvedScope::Person,
            label: ResolvedScope::Person.label(),
            plan,
        };
        reconcile_scope(&env, &sc, &mut targets, &mut sweep);
    }
    if let Some(t) = targets.unmatched() {
        // The refusal names the scope actually SEARCHED. Under the scope rule a name can be
        // perfectly real one scope over, and "not in any manifest" would send someone to re-add
        // what they already have — so each arm says where it looked and offers the other scope's
        // spelling as a way out rather than a claim about where the name lives.
        return Err(ClientError::InvalidArgument(
            match (driven.project, driven.person, &project_dir) {
                (true, false, Some(dir)) => format!(
                    "'{t}' is not demanded by {}/{} — `topos list` shows what this folder \
                     resolves to; `topos add {t}` records it here, and `topos update -g {t}` \
                     updates your machine-wide set instead",
                    dir.display(),
                    crate::manifest::MANIFEST_FILE
                ),
                (false, true, _) => format!(
                    "'{t}' is not in your machine-wide set — neither your own {} nor a connected \
                     feed demands it; `topos list` shows the resolved set; `topos add -g {t}` \
                     records new demand",
                    crate::manifest::MANIFEST_FILE
                ),
                _ => format!(
                    "'{t}' is not in any manifest covering this directory or your connected \
                     feeds — `topos list` shows the resolved set; `topos add` records new demand"
                ),
            },
        ));
    }

    // ---- 5. Disclosures the table cannot carry per row. The person plan's own note (a global
    // file withholding a feed) belongs to a run that drove that scope; a project-scoped run has
    // no business narrating what the machine set is or is not adopting. ----
    disclose(&env, person.as_ref().filter(|_| driven.person), &mut sweep);

    // ---- 6. Clean what nothing demands any more (the targeted form never cleans). ----
    if opts.targets.is_empty() {
        clean_undemanded(
            &env,
            &driven,
            person.as_ref(),
            project.as_ref(),
            project_frozen,
            &mut sweep,
        );
    }

    // ---- 7. Report applied state + refresh the delivery cache, per session. ----
    let mentioned = sweep
        .mentioned_anywhere()
        .into_iter()
        .map(str::to_owned)
        .collect::<HashSet<String>>();
    // The applied report is COMPLETE-state per session: what this installation holds for the
    // workspace, wherever it holds it — the home store AND every visited project store, THIS
    // run's chain unioned with the machine-local visited-stores index (an update run from one
    // checkout must still report another checkout's holdings) — over the feed's deliveries AND
    // the manifest-row deliveries (a declined-but-locally-added bundle included, which is what
    // makes the server's declined-but-applied disclosure real).
    let visited_stores: Vec<sidecar::Layout> =
        crate::visited_stores::recall_and_record(ctx, &manifest_dirs);
    // What the MACHINE still holds bytes for, across all those stores — the prior cache's ids
    // filtered through it keep reporting until their placements really go (the natural drop).
    let held = held_skill_ids(ctx, &visited_stores);
    let mut sync_updates: Vec<(String, WorkspaceSync)> = Vec::new();
    for run in &runs {
        let Some(snap) = &run.snapshot else {
            continue; // unreachable this run: the prior cache entry stands
        };
        let mut reported: HashSet<String> =
            snap.skills.iter().map(|s| s.skill_id.clone()).collect();
        reported.extend(
            sweep
                .delivered
                .iter()
                .filter(|(ws, _, _)| *ws == run.session.workspace_id)
                .map(|(_, id, _)| id.clone()),
        );
        // This workspace's PRIOR deliveries that the machine still holds — another checkout's
        // project rows above all — stay in the complete-state report until their bytes go.
        if let Some(prior) = prior_sync.workspaces.get(&run.session.workspace_id) {
            reported.extend(
                prior
                    .delivered
                    .keys()
                    .filter(|id| held.contains(*id))
                    .cloned(),
            );
        }
        let delivered_ids: HashSet<&str> = reported.iter().map(String::as_str).collect();
        let mut report_ok = false;
        match super::pull::applied_snapshot(ctx, &delivered_ids, &visited_stores) {
            Ok(snapshot) => {
                // The wire carries ONE row per (session, bundle); a bundle held at different
                // versions in different stores says so here, where the pick was made — a local
                // fact, disclosed whether or not the report reaches the plane.
                sweep.disclosures.extend(snapshot.splits.iter().cloned());
                match run
                    .transports
                    .plane
                    .report_applied(&run.session.workspace_id, &snapshot.applied)
                {
                    Ok(()) => report_ok = true,
                    Err(e) => {
                        let m = match e {
                            PlaneError::NotFound => "access gone".to_owned(),
                            PlaneError::Unreachable(m)
                            | PlaneError::Unavailable(m)
                            | PlaneError::Malformed(m) => m,
                        };
                        sweep
                            .warnings
                            .push(format!("REPORT_FAILED {}: {m}", run.session.workspace_name));
                    }
                }
            }
            Err(e) => sweep.warnings.push(format!(
                "REPORT_FAILED {}: {}",
                run.session.workspace_name,
                e.detail()
            )),
        }
        let mut delivered_cache: BTreeMap<String, DeliveredSkill> = BTreeMap::new();
        for ds in &snap.skills {
            delivered_cache.insert(
                ds.skill_id.clone(),
                DeliveredSkill {
                    name: ds.name.clone(),
                    review_required: ds.review_required,
                    served_version: to_hex(&ds.version_id),
                    withdrawn: false,
                    via_channels: ds.via_channels.clone(),
                    via_manifest: false,
                    assigned_by: ds.assigned_by.clone(),
                    picked: ds.picked,
                },
            );
        }
        // Manifest-ROW deliveries this run ride the same cache, marked `via_manifest` — the feed's
        // own row wins a collision (its provenance is broader).
        for (ws, skill_id, ds) in &sweep.delivered {
            if *ws == run.session.workspace_id {
                delivered_cache
                    .entry(skill_id.clone())
                    .or_insert_with(|| ds.clone());
            }
        }
        // A channel that FAILED to expand this run froze its members' placements, a name the
        // recipes still MENTION was not re-recorded either, and a bundle the machine still HOLDS
        // (another checkout's project row — its manifest is not on this run's chain at all): keep
        // all three kinds' prior cache rows — the provenance must survive the same sweep the
        // bytes do, or a LATER drop of the row would find nothing for the cleaner to discover.
        if let Some(prior) = prior_sync.workspaces.get(&run.session.workspace_id) {
            for (skill_id, ds) in &prior.delivered {
                if ds.via_manifest
                    && !ds.withdrawn
                    && !delivered_cache.contains_key(skill_id)
                    && (mentioned.contains(&ds.name)
                        || held.contains(skill_id)
                        || ds.via_channels.iter().any(|c| {
                            sweep
                                .failed_channels
                                .contains(&(run.session.workspace_id.clone(), c.clone()))
                        }))
                {
                    delivered_cache.insert(skill_id.clone(), ds.clone());
                }
            }
        }
        let declined: BTreeMap<String, String> = snap
            .declined
            .iter()
            .map(|(id, name)| (id.clone(), name.clone()))
            .collect();
        sync_updates.push((
            run.session.workspace_id.clone(),
            WorkspaceSync {
                host: Some(run.session.host.clone()),
                workspace_name: Some(run.session.workspace_name.clone()),
                last_delivery_at: Some(now_millis),
                last_report_at: if report_ok {
                    Some(now_millis)
                } else {
                    prior_sync
                        .workspaces
                        .get(&run.session.workspace_id)
                        .and_then(|e| e.last_report_at)
                },
                staleness_window_ms: snap.staleness_window_ms,
                delivered: delivered_cache,
                declined,
            },
        ));
        // Notices, LAST per workspace (the ack marks read-state only after the reconcile ran).
        if !snap.notices.is_empty() {
            if opts.ack_notices {
                let ids: Vec<String> = snap.notices.iter().map(|n| n.id.clone()).collect();
                if let Err(e) = run
                    .transports
                    .plane
                    .ack_notices(&run.session.workspace_id, &ids)
                {
                    let m = match e {
                        PlaneError::NotFound => "access gone".to_owned(),
                        PlaneError::Unreachable(m)
                        | PlaneError::Unavailable(m)
                        | PlaneError::Malformed(m) => m,
                    };
                    sweep
                        .warnings
                        .push(format!("ACK_FAILED {}: {m}", run.session.workspace_name));
                }
            }
            notices.extend(snap.notices.iter().cloned());
        }
    }
    if let Err(e) = sync_status::record(ctx.fs, &ctx.layout, &sync_updates) {
        sweep
            .warnings
            .push(format!("SYNC_STATUS_WRITE_FAILED: {}", e.detail()));
    }
    let sync = sync_updates
        .into_iter()
        .map(|(workspace_id, e)| WorkspaceSyncReport {
            workspace_id,
            last_delivery_at: e.last_delivery_at,
            last_report_at: e.last_report_at,
            staleness_window_ms: e.staleness_window_ms,
        })
        .collect();

    Ok(PullOutcome {
        data: PullData {
            skills: sweep.rows,
            proposals_awaiting,
            notices,
            sync,
            scope: Some(scope_label),
        },
        warnings: sweep.warnings,
        disclosures: sweep.disclosures,
        access_gone,
        unreachable,
    })
}

// =================================================================================================
// The per-scope fan-out.
// =================================================================================================

/// Reconcile ONE scope's whole recipe, in the order that decides collisions: explicit THINGS first
/// (their version and fields are the last word on their identity), then SETS, then — person scope
/// only — the feeds.
fn reconcile_scope<'a>(env: &Env<'a>, sc: &ScopeCtx<'_>, targets: &mut Targets, sweep: &mut Sweep) {
    for row in &sc.plan.things {
        reconcile_thing(env, sc, row, targets, sweep);
    }
    for row in &sc.plan.sets {
        reconcile_set(env, sc, row, targets, sweep);
    }
    if matches!(sc.scope, ResolvedScope::Person) {
        for (host, workspace) in &sc.plan.feeds {
            reconcile_feed(env, sc, host, workspace, targets, sweep);
        }
    }
}

/// One explicit THING row: a workspace bundle, one repo skill, or a local folder.
fn reconcile_thing<'a>(
    env: &Env<'a>,
    sc: &ScopeCtx<'_>,
    row: &PlanRow,
    targets: &mut Targets,
    sweep: &mut Sweep,
) {
    let display = row.display_name();
    if !targets.hit(&[
        display.as_str(),
        row.shape.leaf_name(),
        row.reference.as_str(),
    ]) {
        return;
    }
    sweep.mention(&sc.label, &display);
    match &row.shape {
        KeyShape::WorkspaceBundle {
            host,
            workspace,
            bundle,
        } => {
            let Some(run) = find_run(env.runs, Some(host), workspace) else {
                sweep
                    .warnings
                    .push(not_connected_line(&row.reference, host, workspace));
                return;
            };
            let Some(catalog) = run.catalog(&mut sweep.warnings) else {
                return;
            };
            let Some(entry) = catalog.skills.iter().find(|e| &e.name == bundle) else {
                sweep.warnings.push(format!(
                    "NOT_AVAILABLE {}: \"{}\" — not in {}'s catalog, or not visible with your \
                     current access",
                    sc.label, row.reference, run.session.workspace_name
                ));
                return;
            };
            let target = CatalogTarget::from_entry(entry);
            let override_dir = row_override(sc, row, entry.kind.as_str(), env, sweep);
            sweep.explicit.insert(target.skill_id.clone());
            // A manifest-row delivery records its provenance in the cache too (marked
            // `via_manifest`), so the offline surfaces know which workspace governs the name.
            sweep.delivered.push((
                run.session.workspace_id.clone(),
                target.skill_id.clone(),
                DeliveredSkill {
                    name: target.name.clone(),
                    review_required: false,
                    served_version: target.version_id.clone(),
                    withdrawn: false,
                    via_channels: Vec::new(),
                    via_manifest: true,
                    assigned_by: None,
                    picked: false,
                },
            ));
            let st = SyncTarget {
                target,
                pin: row.pin(),
                display: display.clone(),
                override_dir,
                step: None,
            };
            sync_workspace_skill(env, sc, run, &st, sweep);
        }
        KeyShape::RepoSkill {
            host,
            owner,
            repo,
            skill,
        } => reconcile_repo_skill(env, sc, row, host, owner, repo, skill, sweep),
        KeyShape::LocalPath { raw } => {
            let dir = local_dir(env.ctx, sc, raw);
            if env.ctx.fs.exists(&dir) {
                // A path row whose dir is a placement of an already-GOVERNED bundle is a landed
                // publish's PENDING transfer (the local rewrite half failed) — converge it here,
                // idempotently, disclosed.
                if !converge_pending_governance(env, &dir, sweep) {
                    sweep.push(plain_row(&display, PullAction::UpToDate, None, &sc.label));
                }
            } else {
                sweep.warnings.push(format!(
                    "PATH_MISSING {}: \"{raw}\" — the folder is gone; `topos remove {raw}` drops \
                     the row",
                    sc.label
                ));
            }
        }
        // A feed or channel never lands in `things`; a repo set never does either.
        _ => {}
    }
}

/// One SET row: a workspace channel, or a whole repo.
fn reconcile_set<'a>(
    env: &Env<'a>,
    sc: &ScopeCtx<'_>,
    row: &PlanRow,
    targets: &mut Targets,
    sweep: &mut Sweep,
) {
    match &row.shape {
        KeyShape::Channel {
            host,
            workspace,
            channel,
        } => {
            let set_selected = targets.hit(&[
                row.reference.as_str(),
                channel.as_str(),
                row.display_name().as_str(),
            ]);
            let Some(run) = find_run(env.runs, Some(host), workspace) else {
                sweep.note_set_failure(None, channel, &sc.scope);
                sweep
                    .warnings
                    .push(not_connected_line(&row.reference, host, workspace));
                return;
            };
            let Some(index) = run.channels(&mut sweep.warnings) else {
                sweep.note_set_failure(Some(&run.session.workspace_id), channel, &sc.scope);
                return;
            };
            let Some(ch) = index.channels.iter().find(|c| &c.name == channel) else {
                sweep.note_set_failure(Some(&run.session.workspace_id), channel, &sc.scope);
                sweep.warnings.push(format!(
                    "NOT_AVAILABLE {}: \"{}\" — no such channel in {}, or not visible with your \
                     current access",
                    sc.label, row.reference, run.session.workspace_name
                ));
                return;
            };
            let Some(catalog) = run.catalog(&mut sweep.warnings) else {
                sweep.note_set_failure(Some(&run.session.workspace_id), channel, &sc.scope);
                return;
            };
            let members: Vec<String> = ch.skills.iter().map(|s| s.skill_id.clone()).collect();
            // The batch this channel converges — its members the catalog still serves, minus the
            // ones an explicit row of the SAME scope owns (its version and fields win, and the set
            // adds nothing), minus what `--target` narrowing skips and what an earlier source in
            // this sweep already claimed. Every filter runs HERE, in visit order, so the activity
            // line counts exactly what this run will sync — never "(6 of 20)" over nineteen skips.
            // (`targets.hit`/`mention`/`claimed` mutate sweep bookkeeping; hoisting them keeps each
            // evaluated once per member, in the same order the loop below visits.)
            // `claimed` reads what EARLIER sources synced; a duplicate id WITHIN this channel's own
            // index passes it every time (nothing syncs until the loop below), so the batch tracks
            // its own picks too — a duplicate row is bookkept (hit/mention) but never counted.
            let mut picked: HashSet<&str> = HashSet::new();
            let batch: Vec<&WireSkillIndexEntry> = members
                .iter()
                .filter_map(|member| {
                    let entry = catalog.skills.iter().find(|e| &e.skill_id == member)?;
                    // Archived / no current members simply are not in the catalog — nothing to
                    // deliver, and nothing to count.
                    if sc.plan.explicit_claims(host, workspace, &entry.name) {
                        return None;
                    }
                    if !set_selected && !targets.hit(&[entry.name.as_str()]) {
                        return None;
                    }
                    sweep.mention(&sc.label, &entry.name);
                    if sweep.claimed(&sc.label, &entry.skill_id) {
                        return None;
                    }
                    // An unparseable id can never open a phase (the sync guard refuses it first),
                    // so it must not count either — same warning, raised where it is excluded.
                    if SkillId::parse(&entry.skill_id).is_err() {
                        sweep
                            .warnings
                            .push(format!("BAD_ID {}: served an invalid skill id", entry.name));
                        return None;
                    }
                    picked.insert(entry.skill_id.as_str()).then_some(entry)
                })
                .collect();
            let total = batch.len();
            for (position, entry) in batch.into_iter().enumerate() {
                let target = CatalogTarget::from_entry(entry);
                // Book the provenance BEFORE the sync (a per-item failure does not unmake the fact
                // that this channel provides the name).
                sweep.delivered.push((
                    run.session.workspace_id.clone(),
                    target.skill_id.clone(),
                    DeliveredSkill {
                        name: target.name.clone(),
                        review_required: false,
                        served_version: target.version_id.clone(),
                        withdrawn: false,
                        via_channels: vec![channel.clone()],
                        via_manifest: true,
                        assigned_by: None,
                        picked: false,
                    },
                ));
                let display = target.name.clone();
                let override_dir = row_override(sc, row, entry.kind.as_str(), env, sweep);
                let st = SyncTarget {
                    target,
                    pin: None,
                    display,
                    override_dir,
                    step: Some(Step {
                        index: position + 1,
                        total,
                    }),
                };
                sync_workspace_skill(env, sc, run, &st, sweep);
            }
        }
        KeyShape::RepoSet { host, owner, repo } => {
            reconcile_repo_set(env, sc, row, host, owner, repo, targets, sweep);
        }
        _ => {}
    }
}

/// One workspace FEED (person scope only): everything the workspace currently gives this person,
/// minus what an `"off"` switch withholds and what an explicit row already claimed.
fn reconcile_feed<'a>(
    env: &Env<'a>,
    sc: &ScopeCtx<'_>,
    host: &str,
    workspace: &str,
    targets: &mut Targets,
    sweep: &mut Sweep,
) {
    let address = format!("{host}/{workspace}");
    let feed_selected = targets.hit(&[workspace, address.as_str()]);
    let Some(run) = find_run(env.runs, Some(host), workspace) else {
        sweep.warnings.push(format!(
            "NOT_AVAILABLE {}: the feed of {address} is adopted here, but this installation is not \
             logged into it (run `topos login {address}`)",
            sc.label
        ));
        return;
    };
    match &run.snapshot {
        Some(snap) => {
            let served: Vec<DeliverySkill> = snap.skills.clone();
            // The exchange SUCCEEDED and the workspace served nothing. Without this line the
            // receipt names only the rows that moved, so a person who has just been told the feed
            // would be applied reads the silence as the apply having failed. It is a DISCLOSURE —
            // the exchange worked — so it never joins the count the summary calls failed. Only an
            // empty SERVE says it: bundles that arrived and were skipped here (an `"off"` row, an
            // explicit claim, a target filter) are a local choice, not an empty workspace.
            if served.is_empty() && sweep.empty_feeds.insert(address.clone()) {
                sweep.disclosures.push(nothing_assigned_line(&address));
            }
            // The batch this feed converges — everything served, minus what this machine's own
            // file withholds (an `"off"` switch), what an explicit row already claims, what
            // `--target` narrowing skips, and what an earlier source in this sweep claimed. Every
            // filter runs HERE, in visit order, so the activity line counts exactly what this run
            // will sync (same hoisting rationale as the channel batch above).
            // Same in-batch dedupe as the channel batch: a duplicate id in one delivery frame is
            // bookkept but never counted (`claimed` cannot see it until something syncs).
            let mut picked: HashSet<&str> = HashSet::new();
            let batch: Vec<&DeliverySkill> = served
                .iter()
                .filter(|ds| {
                    if sc.plan.off_for(host, workspace, &ds.name).is_some()
                        || sc.plan.explicit_claims(host, workspace, &ds.name)
                    {
                        return false;
                    }
                    if !feed_selected && !targets.hit(&[ds.name.as_str()]) {
                        return false;
                    }
                    sweep.mention(&sc.label, &ds.name);
                    if sweep.claimed(&sc.label, &ds.skill_id) {
                        return false;
                    }
                    // Same as the channel batch: an unparseable id cannot open a phase, so it is
                    // warned about here and never counted.
                    if SkillId::parse(&ds.skill_id).is_err() {
                        sweep
                            .warnings
                            .push(format!("BAD_ID {}: served an invalid skill id", ds.name));
                        return false;
                    }
                    picked.insert(ds.skill_id.as_str())
                })
                .collect();
            let total = batch.len();
            for (position, ds) in batch.into_iter().enumerate() {
                let st = SyncTarget {
                    target: CatalogTarget {
                        skill_id: ds.skill_id.clone(),
                        name: ds.name.clone(),
                        version_id: to_hex(&ds.version_id),
                        generation: ds.generation,
                        bundle_digest: Some(ds.bundle_digest),
                        review_required: ds.review_required,
                    },
                    pin: None,
                    display: ds.name.clone(),
                    override_dir: None,
                    step: Some(Step {
                        index: position + 1,
                        total,
                    }),
                };
                sync_workspace_skill(env, sc, run, &st, sweep);
            }
        }
        None => {
            // No fresh delivery: converge what the cache says this workspace last served, from the
            // LOCAL store. Nothing is dialed; nothing is cleaned.
            let Some(prior) = env.prior.workspaces.get(&run.session.workspace_id) else {
                return;
            };
            let cached: Vec<(String, DeliveredSkill)> = prior
                .delivered
                .iter()
                .filter(|(_, ds)| !ds.withdrawn && !ds.via_manifest && !ds.name.is_empty())
                .map(|(id, ds)| (id.clone(), ds.clone()))
                .collect();
            for (skill_id, ds) in &cached {
                if sc.plan.off_for(host, workspace, &ds.name).is_some()
                    || sc.plan.explicit_claims(host, workspace, &ds.name)
                {
                    continue;
                }
                if !feed_selected && !targets.hit(&[ds.name.as_str()]) {
                    continue;
                }
                sweep.mention(&sc.label, &ds.name);
                if !sweep.claim(&sc.label, skill_id) {
                    continue;
                }
                let Ok(sid) = SkillId::parse(skill_id) else {
                    continue;
                };
                if !env.ctx.fs.exists(&env.ctx.layout.skill_dir(&sid)) {
                    continue;
                }
                let fc = FollowContext {
                    workspace_id: run.session.workspace_id.clone(),
                    mode: FollowMode::Auto,
                    review_required: ds.review_required,
                    following: true,
                    agents: Vec::new(),
                    excluded_agents: Vec::new(),
                };
                let run_ctx = super::pull::ctx_with_plane_and_follow(
                    env.ctx,
                    run.transports.plane.as_plane(),
                    env.follow,
                );
                match sync_engine::sync_one_with(&run_ctx, &sid, &fc, Invocation::Sweep, None) {
                    Ok(mut row) => {
                        row.workspace_id = Some(run.session.workspace_id.clone());
                        row.scope = Some(sc.label.clone());
                        if row.action == PullAction::DraftSynced {
                            sweep
                                .disclosures
                                .push(draft_synced_line(&ds.name, row.synced_placements));
                        }
                        sweep.push(row);
                    }
                    Err(e) => note_item_failure(env.ctx, &mut sweep.warnings, &ds.name, &e),
                }
            }
        }
    }
}

// =================================================================================================
// The workspace-bundle sync (the one path every workspace row and feed item takes).
// =================================================================================================

/// The one target shape the delivery and the catalog both resolve to.
struct CatalogTarget {
    skill_id: String,
    name: String,
    version_id: String,
    generation: u64,
    bundle_digest: Option<[u8; 32]>,
    review_required: bool,
}

impl CatalogTarget {
    fn from_entry(e: &WireSkillIndexEntry) -> Self {
        Self {
            skill_id: e.skill_id.clone(),
            name: e.name.clone(),
            version_id: e.version_id.clone(),
            generation: e.generation,
            bundle_digest: super::parse_hex32(&e.bundle_digest).ok(),
            review_required: false,
        }
    }
}

/// One bundle to converge, with everything the row said about how it should land.
struct SyncTarget {
    target: CatalogTarget,
    /// The row's version pin — it overrides the served current.
    pin: Option<String>,
    /// The placement directory's display name (the row's `name` field, else the catalog name).
    display: String,
    /// The project-relative placement override the row (or its kind default) resolved to.
    override_dir: Option<String>,
    /// Where this bundle sits in the BATCH its source is converging — what turns the activity line
    /// into "updating docs (2 of 7)". `None` for a lone explicit row, which is a batch of one and
    /// says so by not counting.
    step: Option<Step>,
}

/// One bundle's position in the batch a channel or a feed is converging. Counted over the set the
/// source actually hands this scope, AFTER the purely-local withholdings (an `"off"` switch, a row
/// that claims the name) — a denominator that includes rows nothing will ever visit is a wrong
/// denominator.
#[derive(Debug, Clone, Copy)]
struct Step {
    /// 1-based, so the first item reads "1 of 7" rather than "0 of 7".
    index: usize,
    total: usize,
}

impl Step {
    /// The activity label for `display` at this position — or the uncounted form when the item came
    /// from a lone row.
    fn label(step: Option<Self>, display: &str) -> String {
        match step {
            Some(Self { index, total }) => format!("updating {display} ({index} of {total})"),
            None => format!("updating {display}"),
        }
    }
}

/// Sync ONE workspace bundle toward its served (or pinned) version, at the resolved scope.
fn sync_workspace_skill<'a>(
    env: &Env<'a>,
    sc: &ScopeCtx<'_>,
    run: &'a SessionRun,
    st: &SyncTarget,
    sweep: &mut Sweep,
) {
    let ctx = env.ctx;
    let target = &st.target;
    let Ok(sid) = SkillId::parse(&target.skill_id) else {
        sweep.warnings.push(format!(
            "BAD_ID {}: served an invalid skill id",
            target.name
        ));
        return;
    };
    if !sweep.claim(&sc.label, &target.skill_id) {
        return; // already reconciled in this scope under another row
    }
    // The activity line for this item — opened AFTER the dedupe claim, so a bundle two rows both
    // name is announced once, and held for the whole converge (the engine's own `downloading …`
    // fallback stays quiet underneath it: naming the item beats naming the step).
    let _phase = crate::progress::phase(ctx.progress, &Step::label(st.step, &st.display));
    // The row's pin overrides the served version (the engine fetches by version id, so an older pin
    // resolves as long as the plane still serves its bytes).
    let version_id = st
        .pin
        .as_deref()
        .filter(|p| *p != target.version_id)
        .map_or_else(|| target.version_id.clone(), str::to_owned);
    let record = WireCurrentRecord {
        schema_version: WIRE_SCHEMA_VERSION,
        scope: PointerScope {
            workspace_id: run.session.workspace_id.clone(),
            skill_id: target.skill_id.clone(),
        },
        record: CurrentRecord {
            version_id,
            generation: target.generation,
        },
    };
    let fc = FollowContext {
        workspace_id: run.session.workspace_id.clone(),
        mode: FollowMode::Auto,
        review_required: target.review_required,
        following: true,
        agents: Vec::new(),
        excluded_agents: Vec::new(),
    };
    // The scope decides BOTH the placement plan and the STORE the engine runs against: person → the
    // home layout + home engine; project → the project's own store (`<project>/.topos/state/<user>/`)
    // + in-checkout dirs. Per-scope state is the independence guarantee — the same bundle at both
    // scopes has two state trees, two drafts, two baselines, with no cross-scope anything.
    let project_dir = match &sc.scope {
        ResolvedScope::Project { dir } => Some(dir.clone()),
        ResolvedScope::Person => None,
    };
    let store_layout = match &project_dir {
        Some(dir) => match sidecar::ensure_project_store(ctx.fs, dir) {
            Ok(layout) => layout,
            Err(e) => {
                note_item_failure(ctx, &mut sweep.warnings, &target.name, &e);
                return;
            }
        },
        None => ctx.layout.clone(),
    };
    let naming_slug = run.session.workspace_name.clone();
    let display = st.display.clone();
    let override_dir = st.override_dir.clone();
    // The incoming version's digest arms adopt-in-place: a by-name dir already holding a
    // byte-identical copy (a handed-over old-world placement, a teammate's committed copy) BECOMES
    // the placement instead of a namespaced sibling.
    let adopt_digest = target.bundle_digest;
    // A project root the containment rail refused is a placement that DID NOT HAPPEN — collected
    // from wherever the engine computes the plan, so the receipt says so instead of the bundle
    // quietly landing nowhere.
    let escapes: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
    let plan_fn = |ctx: &Ctx<'_>, skill_id: &str, lock: &Lock, map: &PlacementMap| {
        // The row's `name` is what the directory is called; everything else about the bundle keeps
        // its catalog identity.
        let mut named = lock.clone();
        named.name = display.clone();
        match &project_dir {
            Some(dir) => {
                let plan = placement::project_plan(
                    ctx,
                    dir,
                    skill_id,
                    topos_harness::PlacementNaming {
                        name: Some(&named.name),
                        workspace_slug: Some(&naming_slug),
                    },
                    override_dir.as_deref(),
                    Some(map),
                    adopt_digest,
                );
                escapes.borrow_mut().extend(plan.refused.iter().cloned());
                plan
            }
            None => placement::plan_for_skill(ctx, skill_id, &named, map),
        }
    };
    // Every engine step below runs against the SCOPE's store: same fs/clock/ids, the scope's layout,
    // and this session's plane + the run's follow seam.
    let run_ctx = super::pull::ctx_with_store(
        ctx,
        &store_layout,
        run.transports.plane.as_plane(),
        env.follow,
    );
    // A brand-new arrival lays the never-received baseline first (scope-planned).
    if !run_ctx.fs.exists(&run_ctx.layout.skill_dir(&sid)) {
        let baseline_lock = Lock {
            schema_version: PERSISTED_SCHEMA_VERSION,
            skill_id: target.skill_id.clone(),
            name: st.display.clone(),
            base_commit: String::new(),
            bundle_digest: String::new(),
            files: Vec::new(),
        };
        let empty = PlacementMap {
            schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
            placements: Vec::new(),
            applied_commit: String::new(),
            materialized_sha: String::new(),
            pre_existing_sha: None,
            swap_capability: SwapCapability::Unsupported,
            placement_state: Vec::new(),
            harness: None,
            harness_layer: None,
            harness_slug: None,
        };
        let plan = plan_fn(&run_ctx, &target.skill_id, &baseline_lock, &empty);
        if let Err(e) = lay_baseline_with_plan(
            &run_ctx,
            &sid,
            st.display.clone(),
            &plan,
            target.bundle_digest.as_ref(),
        ) {
            note_item_failure(ctx, &mut sweep.warnings, &target.name, &e);
            return;
        }
    }
    run.transports
        .plane
        .as_delivery()
        .bind_skill(&run.session.workspace_id, &target.skill_id);
    let outcome = sync_engine::sync_one_planned(
        &run_ctx,
        &sid,
        &fc,
        Invocation::Accept,
        Some(&record),
        Some(&plan_fn),
    );
    // The refused-root lines first (deduped): they explain a delivery that landed in fewer dirs
    // than it should have — or in none at all.
    {
        let mut refused = escapes.borrow_mut();
        refused.dedup();
        for line in refused.drain(..) {
            if !sweep.warnings.contains(&line) {
                sweep.warnings.push(line);
            }
        }
    }
    match outcome {
        Ok(mut row) => {
            row.workspace_id = Some(run.session.workspace_id.clone());
            row.scope = Some(sc.label.clone());
            // The settled-draft fan-out's receipt line is a DISCLOSURE — the fan-out succeeded, so
            // it must never land in the channel the summary counts as failures.
            if row.action == PullAction::DraftSynced {
                sweep
                    .disclosures
                    .push(draft_synced_line(&target.name, row.synced_placements));
            }
            // Disclose a delivery the naming ladder had to place BESIDE a same-named occupant the
            // record does not own (the never-clobber outcome) — and a project placement a
            // bundle's OWN root ignore file leaves visible to git.
            if let ResolvedScope::Project { .. } = sc.scope {
                disclose_namespaced(&run_ctx, &sid, &st.display, &mut sweep.warnings);
                disclose_git_visible(&run_ctx, &sid, &target.name, &mut sweep.warnings);
            }
            sweep.push(row);
        }
        Err(e) => note_item_failure(ctx, &mut sweep.warnings, &target.name, &e),
    }
}

/// The project-relative placement override a row resolves to: the row's own `path` beats the
/// `[defaults.<kind>]` path beats nothing (the registry mapping), and within each level the
/// harness-named key beats that level's `default`. PROJECT scope only — a person-scope delivery
/// lands through the home placement engine, which has no override seam.
///
/// The value must be a RELATIVE path that stays inside the project: a committed manifest must never
/// be able to aim managed bytes outside its own checkout, so a hostile/mistaken value is ignored with
/// a warning and the default placement engages.
fn row_override(
    sc: &ScopeCtx<'_>,
    row: &PlanRow,
    kind: &str,
    env: &Env<'_>,
    sweep: &mut Sweep,
) -> Option<String> {
    if !matches!(sc.scope, ResolvedScope::Project { .. }) {
        return None;
    }
    let fields = row.fields();
    let slug = env.ctx.harness.id().slug();
    let raw = scopes::path_override(fields.path.as_ref(), &sc.plan.defaults, kind, slug)?;
    if placement::safe_project_rel(&raw) {
        Some(raw)
    } else {
        sweep.warnings.push(format!(
            "PLACEMENT_OVERRIDE_IGNORED {}: the `path` value {raw:?} must be a relative path inside \
             the project (no `..`, not absolute) — using the default placement",
            row.display_name()
        ));
        None
    }
}

/// A LANDED publish whose local governance rewrite failed leaves a path row for a bundle that is
/// already GOVERNED — this converges it: the dir must be a recorded placement of a bundle tracked
/// in its OWNING store (the home store, or a cwd-chain project store — a project-scope publish's
/// pending transfer lives in the checkout's own store), and exactly ONE connected workspace's
/// catalog must hold that bundle (ambiguity never guesses). That single-catalog-home condition is
/// the whole safety: an ordinary local bundle — genesis `--propose` included until it lands — is
/// in no catalog, and a proposal that DID land remotely satisfies it. On a hit the manifest's
/// path line is rewritten to the canonical reference (the same rewrite the publish would have
/// run), disclosed with one line. Returns whether a rewrite happened.
fn converge_pending_governance(env: &Env<'_>, dir: &Path, sweep: &mut Sweep) -> bool {
    let ctx = env.ctx;
    let Ok(canonical) = dir.canonicalize() else {
        return false;
    };
    let Some((store, skill_id)) = owning_store_at(ctx, &canonical) else {
        return false;
    };
    let Ok(sid) = SkillId::parse(&skill_id) else {
        return false;
    };
    let sp = store.published(&sid);
    let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &sp.lock) else {
        return false;
    };
    // The ONE workspace whose catalog holds the bundle — several never guess.
    let mut homes = env.runs.iter().filter(|run| {
        run.catalog(&mut Vec::new())
            .is_some_and(|c| c.skills.iter().any(|e| e.skill_id == skill_id))
    });
    let (Some(run), None) = (homes.next(), homes.next()) else {
        return false;
    };
    match super::rewrite_to_governed(
        ctx,
        &lock.name,
        &run.session.host,
        &run.session.workspace_name,
        std::slice::from_ref(&canonical),
    ) {
        Ok(super::GovernedOutcome::Rewritten(rw)) => {
            sweep.warnings.push(format!(
                "GOVERNANCE_CONVERGED {}: {} — the \"{}\" line is now \"{}\" (a landed publish's \
                 pending transfer)",
                lock.name, rw.manifest, rw.from, rw.canonical
            ));
            true
        }
        // The row was removed while this converge ran — a completed removal is never re-added.
        Ok(super::GovernedOutcome::RowRemoved { .. } | super::GovernedOutcome::None) => false,
        Err(e) => {
            note_item_failure(ctx, &mut sweep.warnings, &lock.name, &e);
            false
        }
    }
}

/// The store TRACKING the bundle placed at `canonical` — the home store first, then every
/// cwd-chain project store (the same order the write verbs resolve names across stores) — with
/// the tracked skill id. `None` when no store records the dir as a placement.
fn owning_store_at(ctx: &Ctx<'_>, canonical: &Path) -> Option<(sidecar::Layout, String)> {
    if let Ok(Some(id)) = super::add::tracked_skill_at(ctx, canonical) {
        return Some((ctx.layout.clone(), id));
    }
    let roots = ctx.roots.as_ref()?;
    let cwd = roots.cwd.as_deref()?;
    for pd in scopes::manifest_dirs_up(ctx.fs, cwd, Some(&roots.home)) {
        let Some(playout) = sidecar::existing_project_store(ctx.fs, &pd) else {
            continue;
        };
        let pctx = super::pull::ctx_with_layout(ctx, &playout);
        if let Ok(Some(id)) = super::add::tracked_skill_at(&pctx, canonical) {
            return Some((playout, id));
        }
    }
    None
}

/// Where a local-folder row points: relative to the project dir at project scope, to the sidecar
/// home at person scope; `~/` resolves against the machine home when one is known.
fn local_dir(ctx: &Ctx<'_>, sc: &ScopeCtx<'_>, raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(roots) = &ctx.roots
    {
        return roots.home.join(rest);
    }
    if Path::new(raw).is_absolute() {
        return PathBuf::from(raw);
    }
    let base = match &sc.scope {
        ResolvedScope::Project { dir } => dir.clone(),
        ResolvedScope::Person => ctx.layout.home().to_path_buf(),
    };
    base.join(raw.trim_start_matches("./"))
}

/// The one receipt line a settled-draft fan-out earns (the sweep stays otherwise silent about it).
fn draft_synced_line(name: &str, synced: Option<u32>) -> String {
    let n = synced.unwrap_or(0);
    let folders = if n == 1 {
        "1 other agent folder".to_owned()
    } else {
        format!("{n} other agent folders")
    };
    format!("DRAFT_SYNCED {name}: synced your edits of {name} to {folders}")
}

/// The one receipt line an adopted feed earns when its exchange lands empty — the workspace was
/// reached, and it has nothing for this person yet. Says what happened, never what to do about it:
/// there is no command that makes a teammate share something.
fn nothing_assigned_line(address: &str) -> String {
    format!("NOTHING_ASSIGNED {address}: exchanged — nothing assigned to you yet")
}

/// Warn ONCE per bundle when a PROJECT placement is visible to git: the bundle ships its OWN
/// root `.gitignore` (content — the sentinel never overlays it) and that file does not
/// self-ignore the directory. Bundle content is never edited to fix it; the line is the fix's
/// whole surface.
fn disclose_git_visible(ctx: &Ctx<'_>, sid: &SkillId, name: &str, warnings: &mut Vec<String>) {
    let Ok(Some(map)) = doc::read_map(ctx.fs, &ctx.layout.published(sid).map) else {
        return;
    };
    for p in &map.placements {
        let ignore = Path::new(p).join(crate::scan::IGNORE_FILE);
        if let Ok(Some(bytes)) = ctx.fs.read_opt(&ignore)
            && bytes != crate::scan::IGNORE_SENTINEL
            && !crate::materialize::ignores_all(&bytes)
        {
            warnings.push(format!(
                "GIT_VISIBLE {name}: the bundle ships its own .gitignore, which does not ignore \
                 the placement — {p} is visible to git; commit or ignore it deliberately"
            ));
            return;
        }
    }
}

/// Warn when a skill's placement had to land under a NAMESPACED dir because the by-name dir is
/// occupied by content the record does not own (never clobbered — the occupant keeps its bytes).
fn disclose_namespaced(ctx: &Ctx<'_>, sid: &SkillId, name: &str, warnings: &mut Vec<String>) {
    let Some(sanitized) = topos_harness::sanitize_skill_dir(name) else {
        return;
    };
    let Ok(Some(map)) = doc::read_map(ctx.fs, &ctx.layout.published(sid).map) else {
        return;
    };
    for p in &map.placements {
        let placed = Path::new(p);
        let Some(base) = placed.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if base == sanitized {
            continue;
        }
        let Some(sibling) = placed.parent().map(|d| d.join(&sanitized)) else {
            continue;
        };
        // Only an UNOWNED occupant is worth a line: a sibling this skill's own record (or another
        // tracked skill's, in this store) names is an ordinary collision, not an anomaly.
        if ctx.fs.exists(&sibling)
            && !map.placements.iter().any(|q| Path::new(q) == sibling)
            && !placement::recorded_by_another_skill(ctx, sid.as_str(), &sibling)
        {
            warnings.push(format!(
                "NAMESPACED {name}: {} holds different content topos never placed; delivered \
                 beside it as {base}",
                sibling.display()
            ));
        }
    }
}

// =================================================================================================
// The forge arms — git rows move ONLY on an explicit update, and NEVER first-install: an origin
// the MACHINE's trust registry (`crate::forge_trust`, home sidecar) has not granted refuses toward
// the `topos add … --yes` gate. Neither a manifest row NOR per-checkout store contents vouch —
// both are repo facts anyone could have committed: demand, never consent.
// =================================================================================================

/// The store a forge row's tracked imports live in, per scope: the person scope's home store, or
/// the project's OWN store (pure paths — nothing is minted by the read; an absent project store
/// simply holds nothing).
fn forge_store_layout(ctx: &Ctx<'_>, scope: &ResolvedScope) -> crate::sidecar::Layout {
    match scope {
        ResolvedScope::Project { dir } => sidecar::project_store_layout(dir),
        ResolvedScope::Person => ctx.layout.clone(),
    }
}

/// The typed refusal an UNTRACKED forge origin earns on any update: nothing is fetched, nothing
/// installs, and the line names the exact gate command (`topos add … --yes` — the describe-first
/// first-trust ceremony) and where to run it.
fn first_trust_line(sc: &ScopeCtx<'_>, reference: &str) -> String {
    match &sc.scope {
        ResolvedScope::Person => format!(
            "FIRST_TRUST {}: \"{reference}\" — an external source this machine has not adopted \
             through its first-trust gate; nothing is fetched or installed by `update` until \
             `topos add -g {reference} --yes` adds it once",
            sc.label
        ),
        ResolvedScope::Project { dir } => format!(
            "FIRST_TRUST {}: \"{reference}\" — an external source this machine has not adopted \
             through its first-trust gate; nothing is fetched or installed by `update` until \
             `topos add {reference} --yes` (run from {}) adds it once",
            sc.label,
            dir.display()
        ),
    }
}

/// A whole repo: every skill it holds. `"*"` tracks the repo's default branch (one fetch per sweep,
/// compared against the recorded commit); a pinned row NEVER moves — a tracked import whose recorded
/// commit prefix-matches the pin is up to date, and a pin that MOVED re-imports at the new pin.
/// Tracked members ABSENT from the freshly-fetched archive get the ordinary undemanded cleaning
/// (snapshot-first) in the same explicit update that rendered them `-member`.
#[allow(clippy::too_many_arguments)]
fn reconcile_repo_set(
    env: &Env<'_>,
    sc: &ScopeCtx<'_>,
    row: &PlanRow,
    host: &str,
    owner: &str,
    repo: &str,
    targets: &mut Targets,
    sweep: &mut Sweep,
) {
    let origin = format!("{host}/{owner}/{repo}");
    let set_selected = targets.hit(&[row.reference.as_str(), repo, row.display_name().as_str()]);
    // Every store read/write below runs against THIS scope's store — a project row's imports
    // live in the project's own store, so two checkouts of one repo row never share state.
    let store_layout = forge_store_layout(env.ctx, &sc.scope);
    let sctx = super::pull::ctx_with_layout(env.ctx, &store_layout);
    // THE ROW IS DEMAND, and it is demand BEFORE any gate below can return: the members this
    // scope already tracks for the origin are mentioned first, so the undemanded clean can never
    // read a refusal as a drop. A first-trust refusal says "nothing is fetched or installed" —
    // and a run that installs nothing must destroy nothing either; the same reason a transient
    // fetch failure never retires a tracked member (the mention is what protects both).
    let tracked = tracked_repo_members(&sctx, &origin);
    for import in &tracked {
        sweep.mention(&sc.label, &import.lock.name);
    }
    // FIRST TRUST is the MACHINE registry (the home sidecar), consulted in EVERY scope — never
    // store contents: a checkout can commit a valid-looking `.topos/` store, and per-checkout
    // files are demand, not consent. The reconcile never first-installs an ungranted origin;
    // the add gate is the one way in.
    if !crate::forge_trust::is_trusted(env.ctx, &origin) {
        sweep.warnings.push(first_trust_line(sc, &row.reference));
        return;
    }
    let pin = row.pin();
    // The row's own placement decision (`harness = [...]`, written by a `-a` selector import):
    // one converge SLOT per named agent, so a refresh keeps each copy where it was asked for
    // instead of re-landing it in the default dir. No field = one default slot = the old behavior.
    let row_harness = row.fields().harness.unwrap_or_default();
    let roots = discovery_roots(env.ctx, &sc.scope);
    let global = matches!(sc.scope, ResolvedScope::Person);
    let slots_for = |name: &str| -> Vec<HarnessSlot<'_>> {
        harness_slots(&sctx, roots.as_ref(), global, &row_harness, &tracked, name)
    };
    let is_tracked_name = |name: &str| slots_for(name).iter().all(|s| s.import.is_some());

    // A pinned row that every tracked member already satisfies is settled — no forge call, ever.
    // (A granted origin with NOTHING tracked yet in this scope's store — a partial add landing,
    // a fresh checkout of a trusted row — is never "satisfied": the fetch below converges it.)
    //
    // "Every tracked member" is NOT "every member": a batch that landed two of five leaves two
    // rows, both at the pin, and would read as settled forever. So the predicate compares the
    // tracked names against the member set RECORDED at landing — the archive's own contents at
    // that commit — and a missing member makes the explicit update refetch and converge it.
    //
    // An import that recorded NO member set is not evidence of completeness either: "nothing is
    // missing" and "nothing is known" are different answers, and reading the second as the first
    // pins a legacy-shaped import to whatever partial landing it happens to hold, forever. So the
    // absent record is UNSETTLED: the next explicit update refetches ONCE, records the archive's
    // member list (below), and converges — after which the ordinary predicate answers. The quiet
    // sweep still never dials a forge (it carries no git source at all).
    let members_complete = recorded_member_set(&tracked)
        .is_some_and(|recorded| recorded.iter().all(|m| is_tracked_name(m)));
    let pin_satisfied = pin.as_ref().is_some_and(|p| {
        !tracked.is_empty()
            && members_complete
            && tracked
                .iter()
                .all(|i| commit_matches(i.origin.commit.as_deref().unwrap_or_default(), p))
    });

    let Some(git) = env.git.filter(|_| !pin_satisfied) else {
        // Offline (or settled): tracked members converge in place.
        for import in &tracked {
            if !set_selected && !targets.hit(&[import.lock.name.as_str()]) {
                continue;
            }
            sweep.push(plain_row(
                &import.lock.name,
                PullAction::UpToDate,
                None,
                &sc.label,
            ));
        }
        return;
    };

    let spec = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref: pin.clone(),
        subdir: None,
    };
    let targz = match fetch_repo(env, git, &spec) {
        Ok(t) => t,
        Err(e) => {
            note_item_failure(env.ctx, &mut sweep.warnings, &row.reference, &e);
            return;
        }
    };
    let tree = match crate::git_source::extract_tree(&targz) {
        Ok(t) => t,
        Err(e) => {
            note_item_failure(env.ctx, &mut sweep.warnings, &row.reference, &e);
            return;
        }
    };
    let resolved = tree.commit.clone().unwrap_or_default();
    let recorded = tracked
        .first()
        .and_then(|i| i.origin.commit.clone())
        .unwrap_or_default();
    let discovered = tree.skill_names(None, repo);
    for name in &discovered {
        sweep.mention(&sc.label, name);
    }
    // The fetch this run just made is also the answer to "what does this archive hold?" — so an
    // import that recorded no member set gets one now, from the archive itself. That is what makes
    // the refetch above a ONE-TIME cost: the next run's predicate can tell a partial landing from
    // a complete one, and a settled pin stops dialing.
    record_member_sets(&sctx, &tracked, &resolved, &discovered, &mut sweep.warnings);
    let is_tracked = is_tracked_name;
    // The repo has not moved AND every discovered member is already tracked: settled. (An
    // UNTRACKED member at the same commit — a partial add landing — still installs below.)
    if !resolved.is_empty()
        && commit_matches(&recorded, &resolved)
        && discovered.iter().all(|d| is_tracked(d))
    {
        for import in &tracked {
            if !set_selected && !targets.hit(&[import.lock.name.as_str()]) {
                continue;
            }
            sweep.push(plain_row(
                &import.lock.name,
                PullAction::UpToDate,
                None,
                &sc.label,
            ));
        }
        return;
    }
    // The commit motion an explicit update just landed — a DISCLOSURE of what worked, beside the
    // rows that carry the members. It must not ride the channel the summary counts as failures.
    if !recorded.is_empty() && !resolved.is_empty() && !commit_matches(&recorded, &resolved) {
        sweep.disclosures.push(git_updated_line(
            &origin,
            &recorded,
            &resolved,
            &tracked,
            &discovered,
        ));
    }
    // Members the NEW archive no longer holds leave with it: the ordinary undemanded clean
    // (snapshot-first — an edited copy is committed into the store before any dir goes), in this
    // scope's own store. The `-member` the receipt line rendered is thereby true on disk.
    for import in &tracked {
        let still_held = discovered
            .iter()
            .any(|d| import.lock.name == *d || subdir_leaf(&import.origin).as_deref() == Some(d));
        if still_held || (!set_selected && !targets.hit(&[import.lock.name.as_str()])) {
            continue;
        }
        match clean_written_placements(
            &sctx,
            &import.sid,
            matches!(sc.scope, ResolvedScope::Person),
        ) {
            Ok(Some(name)) => sweep.push(plain_row(&name, PullAction::Withdrawn, None, &sc.label)),
            Ok(None) => {}
            Err(e) => note_item_failure(env.ctx, &mut sweep.warnings, &import.lock.name, &e),
        }
    }
    for name in &discovered {
        if !set_selected && !targets.hit(&[name.as_str()]) {
            continue;
        }
        let slots = slots_for(name);
        // EVERY slot's copy already at the fetched commit is settled — only the unfilled (or
        // moved) ones go through the install/refresh below.
        if !resolved.is_empty()
            && slots.iter().all(|s| {
                s.import.is_some_and(|i| {
                    commit_matches(i.origin.commit.as_deref().unwrap_or_default(), &resolved)
                })
            })
        {
            let landed = slots
                .first()
                .and_then(|s| s.import)
                .map_or_else(|| name.clone(), |i| i.lock.name.clone());
            sweep.push(plain_row(&landed, PullAction::UpToDate, None, &sc.label));
            continue;
        }
        install_or_refresh_repo_skill(env, sc, &sctx, &spec, &targz, name, &slots, sweep);
    }
}

/// ONE skill inside a repo, by its leaf directory name (or the literal `subdir` the row spells).
/// The ORIGIN must already be granted in the machine's trust registry (the add gate's ceremony
/// covered the source); a new member of a trusted origin then flows on explicit update.
#[allow(clippy::too_many_arguments)]
fn reconcile_repo_skill(
    env: &Env<'_>,
    sc: &ScopeCtx<'_>,
    row: &PlanRow,
    host: &str,
    owner: &str,
    repo: &str,
    skill: &str,
    sweep: &mut Sweep,
) {
    let origin = format!("{host}/{owner}/{repo}");
    let fields = row.fields();
    let store_layout = forge_store_layout(env.ctx, &sc.scope);
    let sctx = super::pull::ctx_with_layout(env.ctx, &store_layout);
    // The same MACHINE-registry gate as the repo-set arm: store contents never grant.
    if !crate::forge_trust::is_trusted(env.ctx, &origin) {
        sweep.warnings.push(first_trust_line(sc, &row.reference));
        return;
    }
    let members = tracked_repo_members(&sctx, &origin);
    // Same placement decision as the set arm: the row's `harness` list is one converge slot per
    // agent (no field = the one default slot).
    let row_harness = fields.harness.clone().unwrap_or_default();
    let roots = discovery_roots(env.ctx, &sc.scope);
    let global = matches!(sc.scope, ResolvedScope::Person);
    let slots = harness_slots(&sctx, roots.as_ref(), global, &row_harness, &members, skill);
    let tracked = slots.first().and_then(|s| s.import);
    let pin = row.pin();
    let pin_satisfied = pin.as_ref().is_some_and(|p| {
        slots.iter().all(|s| {
            s.import
                .is_some_and(|i| commit_matches(i.origin.commit.as_deref().unwrap_or_default(), p))
        })
    });
    let Some(git) = env.git.filter(|_| !pin_satisfied) else {
        match &tracked {
            Some(import) => {
                sweep.push(plain_row(
                    &import.lock.name,
                    PullAction::UpToDate,
                    None,
                    &sc.label,
                ));
            }
            // The origin is trusted; only THIS member has not been fetched yet.
            None => sweep.warnings.push(format!(
                "NOT_INSTALLED {}: \"{}\" — an external skill this machine has not fetched yet \
                 (network required)",
                sc.label, row.reference
            )),
        }
        return;
    };
    let spec = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref: pin.clone(),
        // A literal in-repo path is the escape hatch a row spells explicitly; without one the leaf
        // NAME selects the skill.
        subdir: fields.subdir.clone(),
    };
    let targz = match fetch_repo(env, git, &spec) {
        Ok(t) => t,
        Err(e) => {
            note_item_failure(env.ctx, &mut sweep.warnings, &row.reference, &e);
            return;
        }
    };
    // Every slot's copy at the same commit is settled: nothing moves without a real change.
    if let Some(import) = &tracked
        && let Ok(tree) = crate::git_source::extract_tree(&targz)
    {
        let resolved = tree.commit.clone().unwrap_or_default();
        let recorded = import.origin.commit.clone().unwrap_or_default();
        let all_at_resolved = !resolved.is_empty()
            && slots.iter().all(|s| {
                s.import.is_some_and(|i| {
                    commit_matches(i.origin.commit.as_deref().unwrap_or_default(), &resolved)
                })
            });
        if all_at_resolved {
            sweep.push(plain_row(
                &import.lock.name,
                PullAction::UpToDate,
                None,
                &sc.label,
            ));
            return;
        }
        if !recorded.is_empty() && !resolved.is_empty() && !commit_matches(&recorded, &resolved) {
            // The single-skill row's twin of the set arm's motion line — a disclosure, not a
            // failure (see the set arm above).
            sweep.disclosures.push(format!(
                "GIT_UPDATED {origin}: {} → {}; skills: ~{}",
                short_commit(&recorded),
                short_commit(&resolved),
                import.lock.name
            ));
        }
    }
    install_or_refresh_repo_skill(env, sc, &sctx, &spec, &targz, skill, &slots, sweep);
}

/// ONE converge slot of a forge row's member: the harness the row aimed this copy at (`None` =
/// the row named none, so the default agent dir answers) and the tracked import already sitting
/// there.
struct HarnessSlot<'a> {
    slug: Option<String>,
    import: Option<&'a ForgeImport>,
}

/// The slots one member of a forge row converges through — the row's `harness` field made
/// operational.
///
/// A `-a` selector import wrote that field precisely so the copy would keep living in the agent
/// dir it was aimed at; without reading it back, the refresh below re-lands the copy through the
/// DEFAULT root and the selection silently evaporates on the first commit move. One slot per named
/// slug (paired with the import whose recorded placements sit under that slug's skills root), and
/// with a single slot the by-name import answers even when its placement predates the field — the
/// row says where that copy belongs, and the refresh is what takes it there.
///
/// No field at all → exactly one default slot holding the by-name import: today's behavior,
/// unchanged.
fn harness_slots<'a>(
    sctx: &Ctx<'_>,
    roots: Option<&super::DiscoveryRoots>,
    global: bool,
    harness: &[String],
    tracked: &'a [ForgeImport],
    name: &str,
) -> Vec<HarnessSlot<'a>> {
    let candidates: Vec<&ForgeImport> = tracked
        .iter()
        .filter(|i| i.lock.name == name || subdir_leaf(&i.origin).as_deref() == Some(name))
        .collect();
    let (Some(roots), false) = (roots, harness.is_empty()) else {
        return vec![HarnessSlot {
            slug: None,
            import: candidates.first().copied(),
        }];
    };
    let scope = if global {
        topos_harness::registry::SkillScope::User
    } else {
        topos_harness::registry::SkillScope::Project
    };
    let single = harness.len() == 1;
    harness
        .iter()
        .map(|slug| {
            let root = topos_harness::registry::skills_root(
                slug,
                scope,
                &roots.home,
                roots.cwd.as_deref(),
            );
            let placed = root.and_then(|root| {
                candidates
                    .iter()
                    .copied()
                    .find(|i| placed_under(sctx, i, &root))
            });
            HarnessSlot {
                slug: Some(slug.clone()),
                import: placed.or_else(|| single.then(|| candidates.first().copied()).flatten()),
            }
        })
        .collect()
}

/// Whether any recorded placement of `import` sits under `root` — how a tracked copy is matched to
/// the harness slot it occupies.
fn placed_under(sctx: &Ctx<'_>, import: &ForgeImport, root: &Path) -> bool {
    let Ok(Some(map)) = doc::read_map(sctx.fs, &sctx.layout.published(&import.sid).map) else {
        return false;
    };
    map.placements
        .iter()
        .any(|p| Path::new(p).starts_with(root))
}

/// Backfill the archive's MEMBER SET onto tracked imports that recorded none (see
/// [`recorded_member_set`]): the fetch that proved what the archive holds is the one chance to
/// write it down, and doing so is what lets a pinned row ever read as settled again.
///
/// ONLY onto imports sitting at the commit this fetch resolved — a member list is a fact about ONE
/// commit, and writing a newer archive's list beside an older import's hash would make the pair
/// lie. An import at another commit is about to be refreshed anyway, and its fresh `origin.json`
/// records the pair correctly. Best-effort per import — a document this build cannot read is left
/// exactly as it is, with a line.
fn record_member_sets(
    sctx: &Ctx<'_>,
    tracked: &[ForgeImport],
    resolved: &str,
    discovered: &[String],
    warnings: &mut Vec<String>,
) {
    if discovered.is_empty() || resolved.is_empty() {
        return;
    }
    for import in tracked.iter().filter(|i| {
        i.members.is_empty()
            && commit_matches(i.origin.commit.as_deref().unwrap_or_default(), resolved)
    }) {
        let path = sctx.layout.published(&import.sid).origin;
        let existing = match doc::read_doc::<super::add::OriginDoc>(sctx.fs, &path) {
            Ok(Some(d)) => d,
            Ok(None) => continue,
            Err(e) => {
                warnings.push(format!(
                    "MEMBERS_UNRECORDED {}: {}",
                    import.lock.name,
                    e.detail()
                ));
                continue;
            }
        };
        let next = super::add::OriginDoc {
            members: discovered.to_vec(),
            ..existing
        };
        if let Err(e) = doc::write_doc(sctx.fs, &path, &next) {
            warnings.push(format!(
                "MEMBERS_UNRECORDED {}: {}",
                import.lock.name,
                e.detail()
            ));
        }
    }
}

/// Install one repo skill, or re-import a tracked one at the new commit, ONCE PER SLOT — every
/// store op through `sctx`, the SCOPE's own store (the refresh's stash/restore therefore never
/// reaches across checkouts). The row's demand already exists — the reconcile NEVER writes a
/// manifest line of its own.
#[allow(clippy::too_many_arguments)]
fn install_or_refresh_repo_skill(
    env: &Env<'_>,
    sc: &ScopeCtx<'_>,
    sctx: &Ctx<'_>,
    spec: &crate::source::RemoteSpec,
    targz: &[u8],
    name: &str,
    slots: &[HarnessSlot<'_>],
    sweep: &mut Sweep,
) {
    let Some(roots) = discovery_roots(env.ctx, &sc.scope) else {
        return;
    };
    // A project install writes through the project store, whose self-ignoring `.topos/` shell
    // must exist first (idempotent; the tracked read above proved the store real, but a fresh
    // member install may be the store's first write after a hand-cleaned tree).
    if let ResolvedScope::Project { dir } = &sc.scope
        && let Err(e) = sidecar::ensure_project_store(env.ctx.fs, dir)
    {
        note_item_failure(env.ctx, &mut sweep.warnings, name, &e);
        return;
    }
    let mut landed: Option<String> = None;
    for slot in slots {
        let opts = super::AddRemoteOpts {
            // A row that spells a literal `subdir` has already narrowed the archive; otherwise the
            // leaf name picks the skill out of a multi-skill repo.
            skill: spec.subdir.is_none().then(|| name.to_owned()),
            // The row's own destination for THIS copy (`None` = the default agent dir).
            harness: slot.slug.clone(),
            global: matches!(sc.scope, ResolvedScope::Person),
        };
        let outcome = match slot.import {
            Some(import) => refresh_repo_skill(sctx, targz, spec, &opts, &roots, &import.sid),
            None => super::add_remote_fetched(sctx, targz, spec, &roots, &opts).map(|d| d.name),
        };
        match outcome {
            // One receipt row per MEMBER, whatever the slot count — the person asked for a skill,
            // not for a copy per agent.
            Ok(name) => landed = landed.or(Some(name)),
            Err(e) => note_item_failure(env.ctx, &mut sweep.warnings, name, &e),
        }
    }
    if let Some(name) = landed {
        sweep.push(plain_row(&name, PullAction::FastForwarded, None, &sc.label));
    }
}

/// Re-import a tracked external skill at a NEW commit: local edits refuse (never overwritten by an
/// import), a clean copy is snapshot-verified, the sidecar record replaced wholesale, and the fresh
/// import lands through the ordinary adopt.
///
/// The WHOLE replacement runs under this skill's own writer lock — it reads the map, scans every
/// placement, moves those dirs and the sidecar record aside, and replaces them; a second writer
/// (an `update` from another agent, a publish) crossing that sequence would read a record whose
/// bytes have already moved. And the stash is a PARK: each dir is renamed aside and only then
/// re-read, so an edit that arrived after the classifying scan is found — and honoured — instead
/// of being deleted by a decision taken before it existed.
fn refresh_repo_skill(
    ctx: &Ctx<'_>,
    targz: &[u8],
    spec: &crate::source::RemoteSpec,
    opts: &super::AddRemoteOpts,
    roots: &super::DiscoveryRoots,
    sid: &SkillId,
) -> Result<String, ClientError> {
    let _guard = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
    let sp = ctx.layout.published(sid);
    let map: PlacementMap = sync_engine::read_map_required(ctx, &sp)?;
    let lock: Lock = doc::read_doc::<Lock>(ctx.fs, &sp.lock)?
        .ok_or_else(|| ClientError::Corrupt(format!("{}: lock.json missing", sid.as_str())))?;
    let scans = placement::scan_placements(ctx, &map)?;
    if scans
        .iter()
        .any(|s| matches!(s.status, placement::ScanStatus::Modified { .. }))
    {
        return Err(ClientError::InvalidArgument(format!(
            "'{name}' has local edits ahead of its imported version — publish them (or `topos \
             update {name} --reset`) before the source refresh",
            name = lock.name
        )));
    }
    if scans
        .iter()
        .any(|s| matches!(s.status, placement::ScanStatus::Unscannable))
    {
        return Err(ClientError::PlacementUnsupported {
            reason: "a placement of this external skill cannot be read; refusing the refresh"
                .into(),
        });
    }
    // Prove the archive extracts + selects BEFORE any old byte is deleted — a bad archive must leave
    // the old install whole.
    {
        let repo_tree = crate::git_source::extract_tree(targz)?;
        repo_tree.select(
            spec.subdir.as_deref(),
            opts.skill.as_deref(),
            &spec.repo,
            &spec.label(),
        )?;
    }
    // Clean re-import: STASH the recorded placements (clean copies of the OLD commit) and the
    // sidecar record aside — sibling renames, same filesystem, each JOURNALED before its rename
    // (a crash mid-refresh must leave a record the next run's recovery restores from: the stash
    // names are unique, so no name-keyed sweep would ever find them) — then adopt afresh. The
    // install can still fail past the prefetch (an occupied destination, an io fault); a failure
    // RESTORES the stashes, so the valid old import is never lost to a refused new one. External
    // sources carry no local history worth preserving past their commit (bytes follow the
    // source), so a SUCCESSFUL swap deletes the stashes — each re-proven still-clean immediately
    // before its drop (a rename cannot revoke an already-open fd).
    let mut stashed: Vec<(PathBuf, PathBuf, Option<[u8; 32]>)> = Vec::new();
    let restore = |fs: &dyn crate::fs_seam::FsOps,
                   stashed: &[(PathBuf, PathBuf, Option<[u8; 32]>)]| {
        // Best-effort, newest-first; a restore failure leaves the stash sibling on disk (its
        // journal entry stands, so recovery retries) rather than deleting anything.
        for (orig, stash, _) in stashed.iter().rev() {
            if !fs.exists(orig) && fs.rename(stash, orig).is_ok() {
                crate::sidecar::settle_park_journal(fs, &ctx.layout, stash);
            }
        }
    };
    let stash_dir = |fs: &dyn crate::fs_seam::FsOps,
                     from: &Path,
                     digest: Option<[u8; 32]>,
                     stashed: &mut Vec<(PathBuf, PathBuf, Option<[u8; 32]>)>|
     -> Result<PathBuf, ClientError> {
        // A UNIQUE stash name — an existing sibling (a prior failed refresh's backup) is never
        // deleted to make room; the ladder suffixes past it. A placement stash (digest known)
        // auto-restores on crash recovery; the sidecar record does NOT (`restore = false`) — the
        // new record may already have landed, and a restored old one would double-track the dir,
        // so recovery preserves + discloses it instead.
        let to = crate::materialize::park_aside_journaled(
            fs,
            &ctx.layout,
            from,
            "refresh-old",
            digest.is_some(),
            Some(sid),
        )?;
        stashed.push((from.to_path_buf(), to.clone(), digest));
        Ok(to)
    };
    let mut stash_all = || -> Result<(), ClientError> {
        for scan in &scans {
            let placement::ScanStatus::Clean { digest } = &scan.status else {
                continue;
            };
            if !ctx.fs.exists(&scan.dir) {
                continue;
            }
            let parked = stash_dir(ctx.fs, &scan.dir, Some(*digest), &mut stashed)?;
            // PARK-THEN-VERIFY: the classification above rode a scan taken before the archive
            // fetch and the extract, so "clean" is a claim about a directory anyone could have
            // edited since. Re-read the PARKED tree — nothing can reach it by path now — and
            // treat a difference exactly as an up-front local edit: refuse, restoring every
            // stash. An import never overwrites work it did not put there, whenever it arrived.
            let still_clean =
                crate::scan::scan(&parked).is_ok_and(|fresh| fresh.bundle_digest == *digest);
            if !still_clean {
                return Err(ClientError::InvalidArgument(format!(
                    "'{name}' was edited while its source refresh was running — publish those \
                     edits (or `topos update {name} --reset`) before the refresh",
                    name = lock.name
                )));
            }
        }
        let sidecar_dir = ctx.layout.skill_dir(sid);
        if ctx.fs.exists(&sidecar_dir) {
            // The sidecar record: topos's own engine state, mutated only under the skill lock
            // this fn holds — no content digest to re-prove (`None`).
            stash_dir(ctx.fs, &sidecar_dir, None, &mut stashed)?;
        }
        Ok(())
    };
    // A stash failure MID-LOOP restores what was already moved — the old import stays coherent.
    if let Err(e) = stash_all() {
        restore(ctx.fs, &stashed);
        return Err(e);
    }
    match super::add_remote_fetched(ctx, targz, spec, roots, opts) {
        Ok(d) => {
            for (_, stash, expect) in &stashed {
                // Re-prove immediately before each drop: a placement stash must STILL hold its
                // recorded clean digest across two consecutive reads (an fd-write that landed
                // after the park would otherwise die unexamined). A stash that moved, or cannot
                // be read, is PRESERVED — its journal entry stands, and recovery discloses it.
                let disposable = match expect {
                    None => true, // the sidecar record — engine state under the held lock
                    Some(d) => {
                        let mut agreed = false;
                        let mut seen = 0u8;
                        while seen < 2 {
                            match crate::scan::scan(stash) {
                                Ok(fresh) if fresh.bundle_digest == *d => seen += 1,
                                _ => break,
                            }
                            agreed = seen == 2;
                        }
                        agreed
                    }
                };
                if disposable && ctx.fs.remove_dir_all(stash).is_ok() {
                    crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, stash);
                }
            }
            Ok(d.name)
        }
        Err(e) => {
            restore(ctx.fs, &stashed);
            Err(e)
        }
    }
}

/// One tarball per `(origin, ref)` per sweep — a repo named by both a set row and a skill row is
/// fetched once.
fn fetch_repo(
    env: &Env<'_>,
    git: &dyn crate::git_source::GitTarballSource,
    spec: &crate::source::RemoteSpec,
) -> Result<Rc<Vec<u8>>, ClientError> {
    let key = format!(
        "{}#{}",
        spec.origin(),
        spec.git_ref.as_deref().unwrap_or("")
    );
    if let Some((_, bytes)) = env.repos.borrow().iter().find(|(k, _)| *k == key) {
        return Ok(Rc::clone(bytes));
    }
    let bytes = Rc::new(git.fetch(spec)?);
    env.repos.borrow_mut().push((key, Rc::clone(&bytes)));
    Ok(bytes)
}

/// The ONE receipt line a moved git source earns: what it was, what it is, and which members that
/// moved — a source that silently swaps bytes under an agent is exactly what this line prevents.
fn git_updated_line(
    origin: &str,
    old: &str,
    new: &str,
    tracked: &[ForgeImport],
    discovered: &[String],
) -> String {
    let had: Vec<&str> = tracked.iter().map(|i| i.lock.name.as_str()).collect();
    let mut parts: Vec<String> = Vec::new();
    for name in discovered {
        if had.contains(&name.as_str()) {
            parts.push(format!("~{name}"));
        } else {
            parts.push(format!("+{name}"));
        }
    }
    for name in &had {
        if !discovered.iter().any(|d| d == name) {
            parts.push(format!("-{name}"));
        }
    }
    let mut line = format!(
        "GIT_UPDATED {origin}: {} → {}",
        short_commit(old),
        short_commit(new)
    );
    if !parts.is_empty() {
        line.push_str("; skills: ");
        line.push_str(&parts.join(" "));
    }
    line
}

/// A commit's first 12 characters — enough to recognize, short enough to read.
fn short_commit(c: &str) -> &str {
    &c[..c.len().min(12)]
}

/// The discovery roots an external install resolves its destination against — project scope roots at
/// the demanding checkout (the import lands in-project), person scope at the machine cwd.
fn discovery_roots(ctx: &Ctx<'_>, scope: &ResolvedScope) -> Option<super::DiscoveryRoots> {
    let roots = ctx.roots.as_ref()?;
    let cwd = match scope {
        ResolvedScope::Project { dir } => Some(dir.clone()),
        ResolvedScope::Person => roots.cwd.clone(),
    };
    Some(super::DiscoveryRoots {
        home: roots.home.clone(),
        cwd,
    })
}

/// Every skill id this MACHINE currently holds managed bytes for — a recorded placement that
/// still exists on disk — across the home store and every visited project store. What keeps the
/// applied report (and the cache's provenance rows) complete across checkouts: an id whose store
/// or placements are gone contributes nothing, so retired holdings drop out naturally.
fn held_skill_ids(ctx: &Ctx<'_>, visited: &[sidecar::Layout]) -> HashSet<String> {
    let mut out = HashSet::new();
    for layout in std::iter::once(&ctx.layout).chain(visited.iter()) {
        let Ok(entries) = ctx.fs.read_dir(&layout.skills_dir()) else {
            continue;
        };
        for entry in entries {
            let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Ok(sid) = SkillId::parse(id) else {
                continue;
            };
            let Ok(Some(map)) = doc::read_map(ctx.fs, &layout.published(&sid).map) else {
                continue;
            };
            if map.placements.iter().any(|p| ctx.fs.exists(Path::new(p))) {
                out.insert(sid.into_string());
            }
        }
    }
    out
}

/// Every tracked skill imported from `origin_source` in THIS ctx's store, by walking the
/// sidecar's origin docs. Best-effort: unreadable entries are skipped.
pub(crate) fn tracked_repo_members(ctx: &Ctx<'_>, origin_source: &str) -> Vec<ForgeImport> {
    forge_imports(ctx)
        .into_iter()
        .filter(|i| i.origin.source == origin_source)
        .collect()
}

/// The MEMBER SET a pinned repo-set row must see landed before it is settled: the union of the
/// member lists the tracked imports recorded at landing. `None` when NO tracked import records one
/// (a pre-`members` import, or none at all) — the caller then cannot tell a partial landing from a
/// complete one and keeps the older, weaker predicate rather than inventing a fact.
fn recorded_member_set(tracked: &[ForgeImport]) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut any = false;
    for import in tracked {
        if import.members.is_empty() {
            continue;
        }
        any = true;
        out.extend(import.members.iter().cloned());
    }
    if !any {
        return None;
    }
    out.sort();
    out.dedup();
    Some(out)
}

/// The last segment of a recorded in-repo path (`skills/alpha` → `alpha`).
fn subdir_leaf(origin: &topos_types::results::SkillOrigin) -> Option<String> {
    origin
        .subdir
        .as_deref()
        .and_then(|s| s.rsplit('/').next())
        .map(str::to_owned)
}

/// Whether a recorded commit satisfies a pin (git-style prefix match, either direction).
pub(crate) fn commit_matches(recorded: &str, pin: &str) -> bool {
    !recorded.is_empty() && (recorded.starts_with(pin) || pin.starts_with(recorded))
}

// =================================================================================================
// Disclosures.
// =================================================================================================

/// The lines a table cannot carry per row: the LOUD note when a hand-written global manifest
/// withholds a connected workspace's feed, and the honest note when a row delivers a bundle the
/// person declined on the web.
fn disclose(env: &Env<'_>, person: Option<&ScopePlan>, sweep: &mut Sweep) {
    let adopted = sweep
        .synced_in(&ResolvedScope::Person.label())
        .into_iter()
        .map(str::to_owned)
        .collect::<HashSet<String>>();
    if let Some(plan) = person.filter(|p| p.file_backed()) {
        let total = adopted.len();
        for run in env.runs {
            let Some(snap) = &run.snapshot else { continue };
            if plan.has_feed(&run.session.host, &run.session.workspace_name) {
                continue;
            }
            let unadopted = snap
                .skills
                .iter()
                .filter(|ds| !adopted.contains(&ds.skill_id))
                .count();
            if unadopted == 0 {
                continue;
            }
            sweep.warnings.push(format!(
                "GLOBAL_MANIFEST {}/{}: global manifest adopts {total} bundles; {unadopted} \
                 assigned bundles are not adopted here (no feed row) — `topos add -g @{}` restores \
                 them",
                run.session.host, run.session.workspace_name, run.session.workspace_name
            ));
        }
    }
    // A decline is the everywhere stance; a machine's own row still wins locally — say so rather
    // than let the two disagree silently.
    for run in env.runs {
        let Some(snap) = &run.snapshot else { continue };
        for (skill_id, name) in &snap.declined {
            if sweep.explicit.contains(skill_id) {
                sweep.warnings.push(format!(
                    "DECLINED_OVERRIDE {name}: declined on the web, delivered here by your manifest"
                ));
            }
        }
    }
}

// =================================================================================================
// Cleaning.
// =================================================================================================

/// Retire what nothing demands any more, per scope:
///
/// - PERSON scope: a cached delivery today's recipe no longer adopts — a dropped row, a withdrawn
///   feed item, a new `"off"` switch, a removed feed row, a file that withholds the whole feed. A
///   feed WITHDRAWAL resets to never-received (a later re-delivery reinstalls); every other cause is
///   this machine's own choice, so the placements go and the sidecar bytes stay.
/// - PROJECT scope: ONLY the NEAREST project's own store is cleaned, against ITS plan — an
///   ancestor's store is never cleaned from below, because that file governs its own subtree.
///
/// Frozen throughout: a scope this run did not DRIVE, a scope whose manifest failed to parse, a
/// workspace with no fresh delivery, the members of a set that failed to expand, and every name a
/// row still mentions. The driven gate comes first on purpose — an undriven scope resolved no rows
/// this run, so its whole recorded set would read as undemanded and retire.
fn clean_undemanded(
    env: &Env<'_>,
    driven: &Driven,
    person: Option<&ScopePlan>,
    project: Option<&(PathBuf, ScopePlan)>,
    project_frozen: bool,
    sweep: &mut Sweep,
) {
    let ctx = env.ctx;

    // ---- Person scope. ----
    if let Some(plan) = person.filter(|_| driven.person) {
        let label = ResolvedScope::Person.label();
        // The PERSON scope's own mentions decide the person clean — scopes are unblended, so a
        // project manifest naming the same NAME must not shield an unrelated person-scope
        // placement from retiring.
        let mentioned: HashSet<String> = sweep.mentioned.get(&label).cloned().unwrap_or_default();
        let adopted: HashSet<String> = sweep
            .synced_in(&label)
            .into_iter()
            .map(str::to_owned)
            .collect();
        for run in env.runs {
            if run.snapshot.is_none() {
                continue; // no fresh delivery: everything freezes
            }
            let Some(prior) = env.prior.workspaces.get(&run.session.workspace_id) else {
                continue;
            };
            let host = run.session.host.clone();
            let ws = run.session.workspace_name.clone();
            let rows: Vec<(String, DeliveredSkill)> = prior
                .delivered
                .iter()
                .map(|(id, ds)| (id.clone(), ds.clone()))
                .collect();
            for (skill_id, cached) in &rows {
                if cached.withdrawn
                    || adopted.contains(skill_id)
                    || mentioned.contains(&cached.name)
                    || cached.via_channels.iter().any(|c| {
                        sweep
                            .failed_channels
                            .contains(&(run.session.workspace_id.clone(), c.clone()))
                    })
                {
                    continue;
                }
                let Ok(sid) = SkillId::parse(skill_id) else {
                    continue;
                };
                if !ctx.fs.exists(&ctx.layout.skill_dir(&sid)) {
                    continue;
                }
                // WHY it left decides how much goes: this machine's own choice keeps the bytes; a
                // feed withdrawal resets, so a re-delivery installs afresh instead of reading as
                // already-current.
                let switched_off = plan.off_for(&host, &ws, &cached.name).is_some();
                let withheld = plan.file_backed() && !plan.has_feed(&host, &ws);
                let by_choice = switched_off || withheld || cached.via_manifest;
                let cleaned = if by_choice {
                    clean_written_placements(ctx, &sid, true)
                } else {
                    withdraw_person_scope(ctx, &sid).map(Some)
                };
                match cleaned {
                    Ok(Some(name)) => sweep.push(plain_row(
                        &name,
                        PullAction::Withdrawn,
                        Some(run.session.workspace_id.clone()),
                        &label,
                    )),
                    Ok(None) => {}
                    Err(e) => note_item_failure(ctx, &mut sweep.warnings, skill_id, &e),
                }
            }
        }
        // Forge imports the person recipe no longer names ride the SAME undemanded clean: a
        // dropped repo row's members retire exactly like a dropped workspace row's placements —
        // snapshot-first, the sidecar bytes kept, in-checkout placements untouched (they are a
        // project scope's business). A row that still names the origin mentioned every tracked
        // member above, so a transient forge failure can never read as a drop.
        for import in forge_imports(ctx) {
            if mentioned.contains(&import.lock.name) || adopted.contains(import.sid.as_str()) {
                continue;
            }
            match clean_written_placements(ctx, &import.sid, true) {
                Ok(Some(name)) => {
                    sweep.push(plain_row(&name, PullAction::Withdrawn, None, &label));
                }
                Ok(None) => {}
                Err(e) => note_item_failure(ctx, &mut sweep.warnings, &import.lock.name, &e),
            }
        }
    }

    // ---- Project scope: the NEAREST file's own store, against its own plan. ----
    if !driven.project || project_frozen {
        return;
    }
    let Some((pd, _plan)) = project else { return };
    let label = ResolvedScope::Project { dir: pd.clone() }.label();
    let demanded: HashSet<String> = sweep.mentioned.get(&label).cloned().unwrap_or_default();
    let adopted: HashSet<String> = sweep
        .synced_in(&label)
        .into_iter()
        .map(str::to_owned)
        .collect();
    // A project without a store has nothing recorded to clean; the probe never mints one.
    let Some(playout) = sidecar::existing_project_store(ctx.fs, pd) else {
        return;
    };
    let pctx = super::pull::ctx_with_layout(ctx, &playout);
    let Ok(entries) = pctx.fs.read_dir(&playout.skills_dir()) else {
        return;
    };
    for entry in entries {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if adopted.contains(id) {
            continue; // reconciled this run — demanded by construction
        }
        let Ok(sid) = SkillId::parse(id) else {
            continue;
        };
        let sp = playout.published(&sid);
        let Ok(Some(lock)) = doc::read_doc::<Lock>(pctx.fs, &sp.lock) else {
            continue;
        };
        let Ok(Some(map)) = doc::read_map(pctx.fs, &sp.map) else {
            continue;
        };
        let stale: Vec<usize> = map
            .placements
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                // A failed set expansion freezes everything under its project dir — a member's dir
                // must survive the sweep that could not see the member list.
                if sweep
                    .unexpanded
                    .iter()
                    .any(|sd| Path::new(p).starts_with(sd))
                {
                    return false;
                }
                Path::new(p).starts_with(pd) && !demanded.contains(&lock.name)
            })
            .map(|(i, _)| i)
            .collect();
        if stale.is_empty() {
            continue;
        }
        let cleaned = crate::sidecar::lock_skill(pctx.fs, &pctx.layout, &sid)
            .and_then(|_guard| clean_placements(&pctx, &sid, &lock, &map, &stale));
        if let Err(e) = cleaned {
            note_item_failure(ctx, &mut sweep.warnings, &lock.name, &e);
        }
    }
}

/// A row-dropped bundle's by-choice clean over ONE store: exactly the placements topos itself
/// WROTE (`materialized_sha` present — an adopted-in-place source dir, which topos never wrote,
/// is never deleted), snapshot-first; the sync doc is NOT reset. With `exclude_project` (the
/// HOME store's posture) placements inside some project checkout are left alone — a project
/// manifest may still demand the bundle there, and that checkout reconciles lazily when visited;
/// a PROJECT store passes `false` (its placements are its own). `Ok(Some(name))` when something
/// was actually cleaned.
fn clean_written_placements(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    exclude_project: bool,
) -> Result<Option<String>, ClientError> {
    let sp = ctx.layout.published(sid);
    let _guard = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
    let lock: Option<Lock> = doc::read_doc(ctx.fs, &sp.lock)?;
    let map: Option<PlacementMap> = doc::read_map(ctx.fs, &sp.map)?;
    let (Some(lock), Some(map)) = (lock, map) else {
        return Ok(None);
    };
    let targets: Vec<usize> = map
        .placements
        .iter()
        .zip(&map.placement_state)
        .enumerate()
        .filter(|(_, (p, st))| {
            st.materialized_sha.is_some()
                && (!exclude_project || !is_project_placement(ctx, Path::new(p)))
        })
        .map(|(i, _)| i)
        .collect();
    if targets.is_empty() {
        return Ok(None);
    }
    clean_placements(ctx, sid, &lock, &map, &targets)?;
    Ok(Some(lock.name))
}

/// ONE forge-imported skill a store tracks: its id, its lock, the recorded provenance, and the
/// member set the archive held at that commit (empty for an import written before the field
/// existed).
pub(crate) struct ForgeImport {
    pub sid: SkillId,
    pub lock: Lock,
    pub origin: topos_types::results::SkillOrigin,
    pub members: Vec<String>,
}

/// Every forge-imported skill a store tracks (an `origin.json` beside its docs), whatever the
/// origin. Best-effort: unreadable entries are skipped.
pub(crate) fn forge_imports(ctx: &Ctx<'_>) -> Vec<ForgeImport> {
    let mut out = Vec::new();
    let Ok(entries) = ctx.fs.read_dir(&ctx.layout.skills_dir()) else {
        return out;
    };
    for entry in entries {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(sid) = SkillId::parse(id) else {
            continue;
        };
        let sp = ctx.layout.published(&sid);
        let Ok(Some(origin)) = doc::read_doc::<super::add::OriginDoc>(ctx.fs, &sp.origin) else {
            continue;
        };
        let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &sp.lock) else {
            continue;
        };
        out.push(ForgeImport {
            sid,
            lock,
            origin: origin.origin,
            members: origin.members,
        });
    }
    out.sort_by(|a, b| a.lock.name.cmp(&b.lock.name));
    out
}

/// A feed-withdrawn bundle leaves the PERSON scope: snapshot every edited copy, clean the placements
/// that are NOT inside some project checkout (a project manifest may still demand it there — that
/// checkout reconciles lazily when visited), keep every sidecar byte, and reset the sync doc to
/// never-received so a later re-delivery reinstalls. Returns the catalog name.
fn withdraw_person_scope(ctx: &Ctx<'_>, sid: &SkillId) -> Result<String, ClientError> {
    let sp = ctx.layout.published(sid);
    let name;
    {
        // The guard is scoped: `reset_to_never_received` below takes the SAME per-skill flock on a
        // fresh fd, which would deadlock against a still-held one.
        let _guard = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
        let lock: Option<Lock> = doc::read_doc(ctx.fs, &sp.lock)?;
        let map: Option<PlacementMap> = doc::read_map(ctx.fs, &sp.map)?;
        name = lock
            .as_ref()
            .map_or_else(|| sid.as_str().to_owned(), |l| l.name.clone());
        if let (Some(lock), Some(map)) = (lock.as_ref(), map.as_ref()) {
            let person: Vec<usize> = map
                .placements
                .iter()
                .enumerate()
                .filter(|(_, p)| !is_project_placement(ctx, Path::new(p)))
                .map(|(i, _)| i)
                .collect();
            clean_placements(ctx, sid, lock, map, &person)?;
        }
    }
    let sync: Option<SyncState> = doc::read_doc(ctx.fs, &sp.sync)?;
    super::pull::reset_to_never_received(ctx, sid, sync.as_ref())?;
    Ok(name)
}

/// Whether a placement dir belongs to some PROJECT checkout — an ancestor holds a `topos.toml` (the
/// manifest travels with the repo; its placements are that scope's business). The ONE heuristic,
/// shared with the person plan's prior-stability rule.
fn is_project_placement(ctx: &Ctx<'_>, dir: &Path) -> bool {
    crate::placement::under_project_manifest(ctx, dir)
}

/// Snapshot-first clean of exactly `indices` placements: every distinct edited copy is committed into
/// the sidecar store BEFORE any dir is removed; Foreign dirs are never touched; the cleaned dirs
/// leave the placement record (demand ended — the explicit act was the manifest edit). Fails closed
/// on an unscannable placement.
///
/// PARK-THEN-VERIFY is what makes that true with no window at all. The snapshot pass above rides
/// ONE scan of every placement, and time passes before the deletes — so each dir is first moved
/// ASIDE (a rename: atomic, nothing can land in it afterwards) and only THEN read. Whatever the
/// parked tree turns out to hold is captured before it goes; a parked tree that cannot be read is
/// put back and the clean refuses. The invariant is the materializer's: no byte differing from its
/// recorded baseline is destroyed unless a snapshot taken after the last revalidation holds it —
/// and here the "last revalidation" reads bytes no one can still be writing.
fn clean_placements(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    lock: &Lock,
    map: &PlacementMap,
    indices: &[usize],
) -> Result<(), ClientError> {
    if indices.is_empty() {
        return Ok(());
    }
    let scans = placement::scan_placements(ctx, map)?;
    if indices
        .iter()
        .any(|&i| matches!(scans[i].status, placement::ScanStatus::Unscannable))
    {
        return Err(ClientError::PlacementUnsupported {
            reason: "a placement cannot be read; refusing to remove it — inspect or move the \
                     directory by hand"
                .into(),
        });
    }
    for (idx, _) in placement::distinct_modified(&scans) {
        if let placement::ScanStatus::Modified { scanned } = &scans[idx].status {
            sync_engine::snapshot_draft(ctx, &ctx.layout.published(sid), lock, scanned)?;
        }
    }
    let mut removed: HashSet<usize> = HashSet::new();
    for &i in indices {
        if matches!(scans[i].status, placement::ScanStatus::Foreign) {
            continue; // never ours to delete
        }
        let p = &scans[i].dir;
        if ctx.fs.exists(p) {
            // PARK FIRST — journaled: after this rename the tree is unreachable by its old path,
            // and the journal entry means a crash anywhere before the drop concludes leaves a
            // record recovery restores from (a park under a unique name is otherwise invisible
            // to every sweep). The read below then sees exactly the bytes about to be dropped —
            // not a snapshot of a moving target.
            let parked = crate::materialize::park_aside_journaled(
                ctx.fs,
                &ctx.layout,
                p,
                "retiring",
                true,
                Some(sid),
            )?;
            // The SETTLE rail: a rename cannot revoke an already-open file descriptor, so the
            // drop is authorized only by TWO CONSECUTIVE AGREEING READS — every distinct content
            // seen along the way snapshotted before it could die. A tree that keeps moving, or
            // becomes unreadable, is put back and the clean refuses.
            let mut prev: Option<String> = None;
            let mut absorbed: Vec<String> = Vec::new();
            let mut settled = false;
            let mut fault: Option<ClientError> = None;
            for _ in 0..4 {
                let fresh = match crate::scan::scan(&parked) {
                    Ok(f) => f,
                    Err(e) => {
                        fault = Some(ClientError::PlacementUnsupported {
                            reason: format!(
                                "{} changed while it was being retired and can no longer be read \
                                 ({e}); refusing to remove it — inspect or move the directory by \
                                 hand",
                                p.display()
                            ),
                        });
                        break;
                    }
                };
                // Already captured by the pass above (same bytes), or equal to this placement's
                // own recorded baseline (nothing of the user's to lose), or absorbed by an
                // earlier loop pass — otherwise these exact bytes are snapshotted now.
                let captured = matches!(
                    &scans[i].status,
                    placement::ScanStatus::Modified { scanned }
                        if scanned.bundle_digest == fresh.bundle_digest
                );
                let now_hex = topos_core::digest::to_hex(&fresh.bundle_digest);
                let at_baseline = map
                    .placement_state
                    .get(i)
                    .and_then(|s| s.materialized_sha.as_deref())
                    == Some(now_hex.as_str());
                if !captured && !at_baseline && !absorbed.contains(&now_hex) {
                    if let Err(e) =
                        sync_engine::snapshot_draft(ctx, &ctx.layout.published(sid), lock, &fresh)
                    {
                        fault = Some(e);
                        break;
                    }
                    absorbed.push(now_hex.clone());
                }
                if prev.as_deref() == Some(now_hex.as_str()) {
                    settled = true;
                    break;
                }
                prev = Some(now_hex);
            }
            if settled {
                // A failed remove leaves the park + its journal entry — recovery restores it.
                ctx.fs.remove_dir_all(&parked)?;
                crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, &parked);
            } else {
                // Put it back and refuse. Bytes this run cannot account for are never dropped —
                // and if the original path has since been taken, the park keeps them under its
                // own name (the journal entry stands; recovery discloses it).
                let restored = crate::materialize::restore_parked(ctx.fs, &parked, p);
                if restored {
                    crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, &parked);
                }
                return Err(match fault {
                    Some(e) if restored => e,
                    Some(e) => ClientError::PlacementUnsupported {
                        reason: format!(
                            "{} (its bytes are parked at {})",
                            crate::render::safe_message(&e),
                            parked.display()
                        ),
                    },
                    None => ClientError::PlacementUnsupported {
                        reason: format!(
                            "{} kept changing while it was being retired; refusing to remove it{}",
                            p.display(),
                            if restored {
                                String::new()
                            } else {
                                format!(" (its bytes are parked at {})", parked.display())
                            }
                        ),
                    },
                });
            }
        }
        removed.insert(i);
    }
    if removed.is_empty() {
        return Ok(());
    }
    let mut next = map.clone();
    let keep: Vec<bool> = (0..map.placements.len())
        .map(|i| !removed.contains(&i))
        .collect();
    let mut it = keep.iter();
    next.placements.retain(|_| *it.next().unwrap_or(&true));
    let mut it = keep.iter();
    next.placement_state.retain(|_| *it.next().unwrap_or(&true));
    doc::write_map(ctx.fs, &ctx.layout.published(sid).map, &next)
}

// =================================================================================================
// `--rebuild`.
// =================================================================================================

/// Rebuild ONE store: for every bundle it tracks, ABSORB each distinct edited copy into the store,
/// then drop the recorded placement dirs and reset the bundle to never-received, so the ordinary
/// sweep re-projects it pristine. The order is the whole guarantee — a rebuild is a repair, and a
/// repair that can lose an edit is not one. A bundle whose placements cannot be read is left exactly
/// as it is, with a line saying so.
fn rebuild_store(ctx: &Ctx<'_>, layout: &crate::sidecar::Layout, warnings: &mut Vec<String>) {
    let Ok(entries) = ctx.fs.read_dir(&layout.skills_dir()) else {
        return;
    };
    for entry in entries {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(sid) = SkillId::parse(id) else {
            continue;
        };
        if let Err(e) = rebuild_skill(ctx, &sid) {
            warnings.push(format!(
                "REBUILD_SKIPPED {id}: {}",
                crate::render::safe_message(&e)
            ));
        }
    }
}

/// [`rebuild_store`] for one bundle (see its doc for the ordering rule).
fn rebuild_skill(ctx: &Ctx<'_>, sid: &SkillId) -> Result<(), ClientError> {
    let sp = ctx.layout.published(sid);
    {
        let _guard = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
        let lock: Option<Lock> = doc::read_doc(ctx.fs, &sp.lock)?;
        let map: Option<PlacementMap> = doc::read_map(ctx.fs, &sp.map)?;
        let (Some(lock), Some(map)) = (lock, map) else {
            return Ok(());
        };
        if map.placements.is_empty() {
            return Ok(());
        }
        let all: Vec<usize> = (0..map.placements.len()).collect();
        clean_placements(ctx, sid, &lock, &map, &all)?;
    }
    let sync: Option<SyncState> = doc::read_doc(ctx.fs, &sp.sync)?;
    super::pull::reset_to_never_received(ctx, sid, sync.as_ref())
}

// =================================================================================================
// Small shared helpers.
// =================================================================================================

/// A row that reports state without an engine run (a settled forge import, a present local folder,
/// a retired placement).
fn plain_row(
    name: &str,
    action: PullAction,
    workspace_id: Option<String>,
    scope: &str,
) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        workspace_id,
        observed: 0,
        applied: 0,
        action,
        offer: None,
        conflict: None,
        merge: None,
        merge_preview: None,
        synced_placements: None,
        scope: Some(scope.to_owned()),
    }
}

/// The session a `(host, workspace)` reference resolves through.
fn find_run<'a>(
    runs: &'a [SessionRun],
    host: Option<&str>,
    workspace: &str,
) -> Option<&'a SessionRun> {
    match host {
        Some(h) => runs
            .iter()
            .find(|r| r.session.host == h && r.session.workspace_name == workspace),
        None => runs.iter().find(|r| r.session.workspace_name == workspace),
    }
}

/// The honest "not available" line for a workspace reference with no session — phrased from LOCAL
/// knowledge (which recipe asked; which login is missing), never a server answer.
fn not_connected_line(reference: &str, host: &str, workspace: &str) -> String {
    format!(
        "NOT_AVAILABLE {reference}: referenced here, but this installation is not logged into \
         {host}/{workspace} (run `topos login {host}/{workspace}`)"
    )
}

/// Disclose one isolated per-item failure (stderr + diagnostics log + a stable warning).
fn note_item_failure(ctx: &Ctx<'_>, warnings: &mut Vec<String>, name: &str, e: &ClientError) {
    let _ = crate::logfile::append_error_event(
        ctx.fs,
        &ctx.layout.log_path(),
        "update",
        e.code(),
        &format!("item {name}: {}", e.detail()),
        None,
        ctx.clock.now_unix_millis(),
    );
    eprintln!("topos update: {name}: {}", crate::render::safe_message(e));
    warnings.push(format!(
        "{} {name}: {}",
        e.code(),
        crate::render::safe_message(e)
    ));
}

/// Pre-1.0 old-state handover (no compatibility machinery): before per-scope stores, ONE home map
/// blended home and project placements. A home-store row that points INTO the ACTIVE project
/// directory is legacy — dropped from the home map with its BYTES LEFT IN PLACE — but ONLY once
/// the project store has VERIFIABLY ADOPTED the skill (a project-store skill records that exact
/// placement path): custody must be established before the old record lets go, so an empty or
/// not-yet-reconciled project manifest hands nothing over (the next sweep, after the project pass
/// adopts, does). The caller passes ONLY the active project plan's dir — never an ancestor a
/// nearer manifest shadows, and nothing at all when the nearest manifest failed to parse (a typo
/// must keep state, never retire it). Two kinds of rows are additionally NOT handed over:
///
/// - agent-less native rows (kind `native`, no agent) — the user's own chosen locations (an
///   adopt-in-place working copy, an explicit placement pin), which the person-scope record keeps
///   managing;
/// - rows of a skill imported from an external source (`origin.json` present) — those keep their home
///   custody for now.
///
/// A skill whose home map ends EMPTY after the drop was project-only. Its home state dir is
/// retired by PARKING, never deleting: the embedded history and draft snapshots in it were not
/// carried into the project store's fresh baseline, so the dir is renamed to a
/// `.topos-handover-*` sibling (outside every sweep's reach), disclosed with a warning line, and
/// journaled in the log — a person can delete it deliberately; topos does not.
pub(crate) fn handover_legacy_project_rows(
    ctx: &Ctx<'_>,
    project_dirs: &[PathBuf],
    warnings: &mut Vec<String>,
) {
    use topos_types::persisted::PlacementKind;
    if project_dirs.is_empty() {
        return;
    }
    let Ok(entries) = ctx.fs.read_dir(&ctx.layout.skills_dir()) else {
        return;
    };
    for entry in entries {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Ok(sid) = SkillId::parse(id) else {
            continue;
        };
        let sp = ctx.layout.published(&sid);
        if matches!(ctx.fs.read_opt(&sp.origin), Ok(Some(_))) {
            continue; // an external import keeps its home custody
        }
        let Ok(_guard) = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, &sid) else {
            continue;
        };
        let Ok(Some(map)) = doc::read_map(ctx.fs, &sp.map) else {
            continue;
        };
        let legacy: Vec<usize> = map
            .placements
            .iter()
            .zip(&map.placement_state)
            .enumerate()
            .filter(|(_, (p, st))| {
                (st.agent.is_some() || st.kind == PlacementKind::Shared)
                    && project_dirs.iter().any(|pd| {
                        Path::new(p).starts_with(pd)
                            // The adoption witness: retire the home row only once the project
                            // store's own record covers this exact path — custody first.
                            && project_store_tracks(ctx, pd, Path::new(p))
                    })
            })
            .map(|(i, _)| i)
            .collect();
        if legacy.is_empty() {
            continue;
        }
        let mut next = map.clone();
        let keep: Vec<bool> = (0..map.placements.len())
            .map(|i| !legacy.contains(&i))
            .collect();
        let mut it = keep.iter();
        next.placements.retain(|_| *it.next().unwrap_or(&true));
        let mut it = keep.iter();
        next.placement_state.retain(|_| *it.next().unwrap_or(&true));
        let done = if next.placements.is_empty() {
            // Project-only: the project store owns the scope now, but the home dir still holds
            // embedded history + draft snapshots no adoption carried over — PARK it (rename to a
            // `.topos-handover-*` sibling no sweep touches), disclose, and log; never delete.
            // The park is JOURNALED before the rename and the entry settled only AFTER the
            // disclosure lands: a crash in between leaves the entry, recovery restores the dir,
            // and the handover simply re-runs — the park can never sit undisclosed.
            match crate::materialize::park_aside_journaled(
                ctx.fs,
                &ctx.layout,
                &ctx.layout.skill_dir(&sid),
                "handover",
                true,
                Some(&sid),
            ) {
                Ok(parked) => {
                    warnings.push(format!(
                        "STATE_HANDOVER {id}: the project store now tracks it; the old home-side \
                         state (history + draft snapshots) is preserved at {} — delete it \
                         deliberately when you no longer want it",
                        parked.display()
                    ));
                    let _ = crate::logfile::append_event(
                        ctx.fs,
                        &ctx.layout.log_path(),
                        &serde_json::json!({
                            "action": "handover_store_parked",
                            "skill_id": id,
                            "kept_at": parked.to_string_lossy(),
                            "at": ctx.clock.now_unix_millis(),
                        }),
                    );
                    // The disclosure above IS this park's conclusion — the parked history is a
                    // deliberate, named leftover now, not a stranded one.
                    crate::sidecar::settle_park_journal(ctx.fs, &ctx.layout, &parked);
                    Ok(())
                }
                Err(e) => Err(e),
            }
        } else {
            crate::materialize::mirror_first_placement(&mut next);
            doc::write_map(ctx.fs, &sp.map, &next)
        };
        if let Err(e) = done {
            warnings.push(format!("STATE_HANDOVER_FAILED {id}: {}", e.detail()));
        }
    }
}

/// Whether the project store at `pd` verifiably tracks a skill whose recorded placements cover
/// `path` (canonical compare — the same predicate `add`'s already-tracked guard uses). `false`
/// when the store does not exist, the path no longer resolves, or nothing records it — every
/// one of which means custody is NOT established and the home row must stay.
fn project_store_tracks(ctx: &Ctx<'_>, pd: &Path, path: &Path) -> bool {
    let Some(playout) = sidecar::existing_project_store(ctx.fs, pd) else {
        return false;
    };
    let Ok(canonical) = path.canonicalize() else {
        return false;
    };
    let pctx = super::pull::ctx_with_layout(ctx, &playout);
    matches!(super::add::tracked_skill_at(&pctx, &canonical), Ok(Some(_)))
}

/// Upcast helpers — `Box<dyn ReconcileTransport>` to its two supertrait views.
trait TransportViews {
    fn as_plane(&self) -> &dyn PlaneSource;
    fn as_delivery(&self) -> &dyn crate::plane::DeliverySource;
}

impl TransportViews for Box<dyn ReconcileTransport> {
    fn as_plane(&self) -> &dyn PlaneSource {
        &**self
    }
    fn as_delivery(&self) -> &dyn crate::plane::DeliverySource {
        &**self
    }
}

// =================================================================================================
// The never-received baseline — the sidecar scaffold a brand-new arrival's first receive lands into.
// =================================================================================================

/// The all-zero sentinel a first-receive baseline carries.
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// The genesis generation sentinel.
const GENESIS: u64 = 0;

/// Lay the never-received baseline with the placement plan already computed — the reconcile's entry
/// for arrivals at either scope (a project arrival's targets root at the demanding checkout, not the
/// home harness dirs).
///
/// # Errors
/// A store / io failure; a raced concurrent baseline is not an error (theirs is kept).
pub(crate) fn lay_baseline_with_plan(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    name: String,
    plan: &crate::placement::PlacementPlan,
    incoming_digest: Option<&[u8; 32]>,
) -> Result<(), ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    if ctx.fs.exists(&ctx.layout.skill_dir(skill_id)) {
        return Ok(());
    }

    let (staging_base, sp) = ctx.layout.staging(skill_id);
    if ctx.fs.exists(&staging_base) {
        ctx.fs.remove_dir_all(&staging_base)?;
    }
    ctx.fs.create_dir_all(&sp.store)?;
    // An empty embedded-git store the first received version is later written into. The full-tree
    // durability set is exactly right HERE (and only here + `add`'s staging import): the store is a
    // fresh `init_bare`, so the whole tree IS this op's writes (the repo scaffolding — HEAD / config /
    // objects/ / refs/) and never carries history.
    let store = Store::init(&sp.store)?;
    sync_engine::fsync_batch(ctx, &store.durability_set()?)?;
    doc::write_doc(
        ctx.fs,
        &sp.sync,
        &SyncState {
            schema_version: PERSISTED_SCHEMA_VERSION,
            observed: GENESIS,
            observed_version_id: ZERO_HEX.to_owned(),
            applied: GENESIS,
            base_commit: ZERO_HEX.to_owned(),
            work_hash: ZERO_HEX.to_owned(),
            held: false,
            draft_observed: None,
        },
    )?;
    let baseline = PlacementMap {
        schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
        placements: Vec::new(),
        applied_commit: ZERO_HEX.to_owned(),
        materialized_sha: ZERO_HEX.to_owned(),
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        placement_state: Vec::new(),
        harness: Some(ctx.harness.id()),
        harness_layer: None,
        harness_slug: Some(ctx.harness.id().slug().to_owned()),
    };
    let mut map = crate::placement::reconcile_map(&baseline, plan);
    // Record the ADOPTIONS durably: a planned dir that already exists under the display name with
    // byte-identical content gets its digest into `pre_existing_sha` — the reservation later plans
    // reuse (and the sticky prior-bytes record uninstall restores). `materialized_sha` stays None:
    // no bytes move at baseline time; the consented accept heals the dir in place.
    if let Some(digest) = incoming_digest {
        crate::placement::record_adoptions(ctx, &mut map, skill_id.as_str(), &name, digest);
    }
    doc::write_map(ctx.fs, &sp.map, &map)?;
    // lock LAST — the commit marker (recovery keeps a dir only when lock.json is present).
    doc::write_doc(
        ctx.fs,
        &sp.lock,
        &Lock {
            schema_version: PERSISTED_SCHEMA_VERSION,
            skill_id: skill_id.to_string(),
            name,
            base_commit: ZERO_HEX.to_owned(),
            bundle_digest: ZERO_HEX.to_owned(),
            files: Vec::new(),
        },
    )?;

    match ctx
        .fs
        .rename_dir_noreplace(&staging_base, &ctx.layout.skill_dir(skill_id))
    {
        Ok(()) => {}
        // Raced a concurrent baseline/receive — keep theirs, clean our staging.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            ctx.fs.remove_dir_all(&staging_base)?;
            return Ok(());
        }
        Err(e) => return Err(ClientError::Io(format!("publish baseline {skill_id}: {e}"))),
    }
    ctx.fs.fsync_dir(&ctx.layout.skills_dir())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Step;

    #[test]
    fn the_activity_label_counts_a_batch_and_stays_quiet_about_a_lone_row() {
        // A channel or a feed hands the sweep a batch, so the line says where in it we are —
        // 1-based, because "0 of 7" reads as nothing having started.
        assert_eq!(
            Step::label(Some(Step { index: 1, total: 7 }), "docs"),
            "updating docs (1 of 7)"
        );
        assert_eq!(
            Step::label(Some(Step { index: 7, total: 7 }), "docs"),
            "updating docs (7 of 7)"
        );
        // A lone explicit row is a batch of one and says so by not counting.
        assert_eq!(Step::label(None, "docs"), "updating docs");
        // The label carries the DISPLAY name (the folder the person sees), never an opaque id.
        assert_eq!(
            Step::label(None, "deploy-runbook"),
            "updating deploy-runbook"
        );
    }
}
