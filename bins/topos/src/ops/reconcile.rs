//! The RECONCILE — what `update` runs. Two UNBLENDED scopes, each converged on its own recipe:
//!
//! - the **person scope** — the global manifest (`~/.topos/topos.toml`), the machine's COMPLETE
//!   recipe: only its rows deliver, and a workspace's feed flows iff a feed row says so (`topos
//!   login` writes that row on this machine's first connection; a deleted row stays deleted).
//!   With no file nothing is demanded machine-wide. Bytes land in the home harness dirs, state
//!   in `~/.topos/`.
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use topos_core::digest::to_hex;
use topos_gitstore::Store;
use topos_types::PERSISTED_SCHEMA_VERSION;
use topos_types::persisted::{Lock, PlacementMap, SwapCapability, SyncState};
use topos_types::requests::{WireChannelIndex, WireSkillIndex, WireSkillIndexEntry};
use topos_types::results::{
    ExchangeFault, PullAction, PullData, PullSkill, TargetOutcome, WorkspaceSyncReport,
};
use topos_types::{CurrentRecord, PointerScope, WIRE_SCHEMA_VERSION, WireCurrentRecord};

use crate::bundle_kind::BundleKind;
use crate::ctx::Ctx;
use crate::error::{ClientError, FetchFault};
use crate::forge_check::{self, CheckFailure, SourceCheck};
use crate::git_source::{GitTarballSource, RepoHead};
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

use super::pull::{PullOutcome, StaleForge, StaleReason, UnreachableWorkspace};
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
    /// `--force`: absorb every edited copy into its store, drop the recorded placement dirs, and
    /// let the ordinary sweep re-project them from the store. The absorb-then-drop ORDER is the
    /// whole guarantee — a rebuild must never be a way to lose an edit. Gated per scope, like
    /// everything else this run drives. What it uniquely serves is the folder that EXISTS but is
    /// damaged: an ABSENT placement is refilled by an ordinary run (the converge targets
    /// [`placement::ScanStatus::Absent`]), while a CHANGED one is protected forever as the
    /// person's draft.
    pub rebuild: bool,
    /// Which scope(s) to DRIVE (see [`UpdateScope`]). Both plans are still read either way — the
    /// selector decides which ones converge, clean, and disclose.
    pub scope: UpdateScope,
    /// Whether this run may contact a forge now, or only when the auto-update clock says so.
    pub forge: ForgeCadence,
}

/// When a run is allowed to contact a forge.
///
/// The two lanes keep separate clocks because they cost different things. Delivery asks ONE server
/// about every bundle at once, so the sweep's own few-minute throttle already bounds it; a forge
/// answers about one repository per request, against an allowance shared by everyone behind an
/// address. So the silent sweep carries the forge lane on the much slower schedule
/// `crate::forge_check` keeps, while a person who typed the command gets an answer now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ForgeCadence {
    /// A hand-run `update`: dial now, whatever the clock says.
    #[default]
    Now,
    /// The silent sweep: dial only once the interval has elapsed. Either way the clock advances,
    /// so a round that failed waits exactly as long as one that worked.
    Scheduled,
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

    /// The receipt's scope name: `"project <dir>"`, `"machine"`, or `"both"`. The directory comes
    /// in DISPLAY spelling (`~`-abbreviated) — the receipt names it once and then writes every
    /// path below it relative to it, which only works while both are spelled the same way.
    fn label(&self, project_dir: Option<&str>) -> String {
        match (self.project, self.person) {
            (true, true) => "both".to_owned(),
            (true, false) => {
                project_dir.map_or_else(|| "project".to_owned(), |d| format!("project {d}"))
            }
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
    /// The RUNNABLE fix for a fault this sweep reported, when the fault has one. A warning line
    /// tells a person what to do; an agent reading `--json` acts on `next_actions` and nothing
    /// else, so a fault whose remedy lived only in prose left the machine channel offering
    /// whatever generic advice happened to apply — or nothing at all. Written only beside the
    /// warning it belongs to.
    fault_actions: Vec<topos_types::NextAction>,
    /// FAILURES only — the isolated per-skill faults the receipt counts and the renderer calls
    /// failed, and the one channel that makes the run exit non-zero. A line that describes
    /// something that WORKED belongs in `disclosures`, or a clean run reports itself as broken;
    /// a bundle waiting on a person belongs in `decisions`, or an answer nobody has given yet
    /// reads as a fault to go and fix.
    warnings: Vec<String>,
    /// Bundles left as they are until the PERSON decides (see [`super::PendingDecision`]) — their
    /// own edits standing in the way of a newer version. Counted under `waiting on you`, never
    /// under `failed`, and the run still exits 0.
    decisions: Vec<super::PendingDecision>,
    /// Successful facts worth stating — the settled-draft fan-out, a cross-scope version split.
    /// They ride the same `--json` `warnings` array (one stable machine channel) but are never
    /// counted as failures.
    disclosures: Vec<String>,
    /// The BUNDLES this sweep could not carry forward — written by, and only by, the two
    /// per-bundle failure recorders ([`note_item_failure`] and pull's twin). It is what the
    /// receipt counts as failed, because `warnings` is a LINE channel and a line is not a
    /// bundle: a scope-level fault (an unavailable lock, an unreadable custody document) is one
    /// line about no bundle at all, and counting lines made the summary invent bundles that do
    /// not exist and then report them failed.
    ///
    /// KNOWN COLLAPSE, deliberately left: the key is the DISPLAY NAME, so two same-named bundles
    /// from different workspaces that BOTH fail in one sweep are counted once and the summary
    /// undercounts by one. Every other channel still reports both (a warning line each, and each
    /// bundle's own row), so nothing is hidden — only the tally is short. The same name-keying
    /// costs one more thing, in the converge fold: the row RETAIN that stands a failed bundle's
    /// row down matches on name+scope, so a same-named healthy twin in that scope loses its row
    /// alongside the failed one. Both are the one re-key by scope+identity, and it is a follow-up.
    failed_bundles: std::collections::BTreeSet<String>,
    /// ADVISORIES — real `warning:` lines about a row that still DELIVERED (an unknown MCP dest
    /// entry dropped from a bundle's narrowing). They ride the same `--json` `warnings` array and
    /// print with the warnings, but the summary never counts them: the bundle they annotate has
    /// its own row, and counting the line too would invent a second, failed bundle.
    advisories: Vec<String>,
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
    /// Per scope label: the MCP bundles this sweep resolved (`kind = "mcp"` rows and feed items,
    /// with the stored `server.json` bytes and the reach their rows asked for). Planned onto the
    /// scope's config surfaces — once, in [`run_mcp_converge`] — into what
    /// [`crate::mcp_engine::converge`] runs on.
    mcp_demands: BTreeMap<String, Vec<crate::mcp_engine::DemandedBundle>>,
    /// Per scope label: mcp bundle ids whose demand state is UNKNOWABLE this run (a row whose
    /// resolution failed, a store not yet holding bytes) — the engine holds their config entries.
    mcp_hold: BTreeMap<String, HashSet<String>>,
    /// `dest` entries already warned about as unknown MCP config files (one line per entry per
    /// run).
    mcp_warned_dests: HashSet<String>,
    /// The delivery cache was unreadable this run: the mcp hold computation is blind, so removal
    /// convergence is withheld entirely (freeze, never guess).
    mcp_blind: bool,
    /// `(scope label, bundle identity)` → the index in `rows` of the receipt row that bundle's
    /// per-agent states belong to. The join key is the IDENTITY, never the display name: a
    /// workspace `linear` and a local `linear` can stand in one scope, and a name match would hand
    /// one bundle's config outcomes to the other's row.
    mcp_rows: BTreeMap<(String, String), usize>,
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

    /// The index the NEXT pushed row will take — captured by the mcp arms so a converge outcome
    /// finds its own receipt row by identity (see [`Sweep::mcp_rows`]).
    fn next_row_index(&self) -> usize {
        self.rows.len()
    }

    /// File a bundle's receipt row under its identity in this scope. `None` = the bundle produced
    /// no row this run (a sync that failed, a governance converge that spoke for it), which simply
    /// leaves the converge's states off the receipt.
    /// `unreachable` is the clause a row states when its own `dest` narrowed the bundle down to no
    /// agent at all: without it the row would print the ordinary install line and read as healthy
    /// while nothing was placed anywhere.
    fn note_mcp_row(
        &mut self,
        label: &str,
        bundle_id: &str,
        index: Option<usize>,
        unreachable: Option<&str>,
    ) {
        if let Some(index) = index {
            self.mcp_rows
                .insert((label.to_owned(), bundle_id.to_owned()), index);
            // The row is a config-placed bundle's, and says so on the receipt — a fact that
            // survives a scope where NO agent was engaged and the per-agent states came back
            // empty, so a summary counting these rows never calls a server a skill.
            if let Some(row) = self.rows.get_mut(index) {
                row.kind = BundleKind::Mcp.tag();
                if let Some(clause) = unreachable {
                    row.note = Some(format!("reaches no agent — {clause}"));
                }
            }
        }
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
    /// The forge lane. `None` when this run may not contact a forge at all — a scheduled sweep
    /// inside its interval — and the arms then converge tracked bytes in place, unchanged.
    forge: Option<&'a ForgeLane<'a>>,
    prior: &'a sync_status::SyncStatus,
    /// The ONE connected host the `@ws` sugar resolves at — how installed/removed receipt rows
    /// qualify their names (`@ws/name` there, the full `host/ws/name` everywhere else).
    default_host: Option<String>,
}

impl Env<'_> {
    /// The workspace-QUALIFIED display name a receipt row leads with — the ONE shared rule
    /// ([`super::manifest_edit::qualify_display`]).
    fn qualified(&self, host: &str, workspace: &str, name: &str) -> String {
        super::manifest_edit::qualify_display(self.default_host.as_deref(), host, workspace, name)
    }
}

// =================================================================================================
// The forge lane — one round's contact with the outside, and everything that round learns.
// =================================================================================================

/// ONE reconcile round's forge lane: the transport, the round's circuit breaker, the tarball cache,
/// and the check outcomes the round records.
///
/// The breaker is the twin of the delivery lane's ([`super::pull`]): the FIRST fault that never
/// reached a forge short-circuits every remaining source, so a machine with no network pays one
/// connect timeout for the whole round instead of one per row — which is what keeps a session-start
/// sweep inside its budget. A fault the forge ANSWERED with never trips it: a 500 about one
/// repository says nothing about the next. Nothing is blacklisted and no source is remembered as
/// bad; the breaker dies with the round.
struct ForgeLane<'a> {
    git: &'a dyn GitTarballSource,
    /// The forge could not be reached earlier in this round.
    down: std::cell::Cell<bool>,
    /// What was recorded BEFORE this round — the settled refusals, and how long each source has
    /// gone without an answer.
    prior: forge_check::ForgeCheck,
    /// Whether this round waits on each question's own clock, or answers now because a person
    /// asked. Held here because the decision is made per question, at the moment the row is
    /// reconciled — one clock for the whole lane would let the first checkout visited after a due
    /// time spend the turn for a second checkout the sweep never even looked at.
    cadence: ForgeCadence,
    /// This round's outcome per QUESTION (`<source>#<ref>`). Per question, not per source: one
    /// repository named at two refs produces two independent facts, and filing them together would
    /// let a floating row that works erase a pinned row's final verdict.
    seen: std::cell::RefCell<BTreeMap<String, SourceCheck>>,
    /// The furthest-out instant any host asked to be left alone until this round.
    backoff: std::cell::Cell<Option<i64>>,
    /// One tarball per `(origin, ref)` per round — a repo named by both a set row and a skill row
    /// is fetched once.
    repos: std::cell::RefCell<Vec<(String, Rc<Vec<u8>>)>>,
    /// The same rule for the cheap check: one probe per `(origin, ref)` per round. Two rows over
    /// one repository ask one question — the answer cannot differ between them.
    heads: std::cell::RefCell<Vec<(String, RepoHead)>>,
    /// The FINAL refusals this round discovered, as person-facing sentences. A deletion is said
    /// ONCE, and the silent sweep's warnings channel is discarded — so the line has to travel on a
    /// structured channel the quiet renderer reads, or the one time it would ever be said is a
    /// time nobody hears it.
    announce: std::cell::RefCell<Vec<String>>,
    /// The `(question, scope)` turns this round took — what the clock is written against. A turn
    /// is a scope's, not a machine's: two checkouts share a question but not a state.
    turns: std::cell::RefCell<BTreeSet<(String, String)>>,
    /// The `origin#ref` questions this round actually put a request out for. A question ASKED is
    /// asked once; a question nobody asked is not answered by someone else's failure — which is
    /// why this is tracked apart from the outcome map.
    dialed: std::cell::RefCell<BTreeSet<String>>,
    now_ms: i64,
}

/// Why the lane will not dial for a source right now — each with what, if anything, to say.
enum ForgeHold {
    /// The forge answered about this source and the answer was final. Said ONCE, then never again
    /// until the row that names it changes.
    Gone { reason: String, first_time: bool },
    /// This question is inside its own interval. Nothing is dialed and nothing is said: waiting
    /// is the ordinary state of a row that was checked recently.
    NotDue,
    /// This source already had its turn in this round and it did not work. The row that reached it
    /// first has already said so; a second row saying the same thing is noise.
    Attempted,
    /// The forge was already unreachable earlier in this round.
    Down,
}

impl<'a> ForgeLane<'a> {
    fn new(
        git: &'a dyn GitTarballSource,
        prior: forge_check::ForgeCheck,
        now_ms: i64,
        cadence: ForgeCadence,
    ) -> Self {
        Self {
            git,
            cadence,
            down: std::cell::Cell::new(false),
            prior,
            seen: std::cell::RefCell::new(BTreeMap::new()),
            backoff: std::cell::Cell::new(None),
            repos: std::cell::RefCell::new(Vec::new()),
            heads: std::cell::RefCell::new(Vec::new()),
            announce: std::cell::RefCell::new(Vec::new()),
            dialed: std::cell::RefCell::new(BTreeSet::new()),
            turns: std::cell::RefCell::new(BTreeSet::new()),
            now_ms,
        }
    }

    /// Whether this source may be dialed, and why not when it may not.
    ///
    /// The unit is the QUESTION — a `(source, ref)` pair — not the source. Two rows naming one
    /// repository at the same ref ask one question, so a failure already suffered answers the
    /// second row too. Two rows naming it at DIFFERENT refs ask different questions, and one
    /// failing says nothing about the other: editing a row's ref is the documented way to reopen a
    /// settled verdict, and a sibling row must not be able to silence it.
    ///
    /// A verdict reached EARLIER IN THIS ROUND counts as much as one carried in from a previous
    /// one: two manifest rows can name a single repository (a set line and a member line of it),
    /// and they must not cost two requests and two identical sentences about the same repo.
    fn hold(&self, origin: &str, git_ref: &str, scope: &str) -> Option<ForgeHold> {
        // ONE TURN PER QUESTION PER ROUND. A question already asked has already been answered —
        // whatever the answer was. Without this a failing repo named twice costs two requests and
        // says the same sentence twice in one breath, because only the never-reached fault opens
        // the breaker and every other fault would fall straight through to a second dial.
        let key = forge_check::question(origin, git_ref);
        // NOT DUE YET — this question's own clock, not the lane's. A round that is early for one
        // row is not early for another, and the row this sweep cannot see is exactly the row a
        // machine-wide clock strands.
        //
        // And per SCOPE, because a turn is a scope's. A checkout that has never asked has never had
        // one, so a fresh clone fetches now instead of sitting empty until an interval another
        // checkout started runs out; a scope that HAS asked waits, whatever the answer was, which
        // is what stops one dead network becoming a request per session.
        if self.cadence == ForgeCadence::Scheduled
            && !self.dialed.borrow().contains(&key)
            && !forge_check::due(&self.prior, &key, scope, self.now_ms)
        {
            return Some(ForgeHold::NotDue);
        }
        // This scope is taking its turn now, whatever comes of it.
        self.turns
            .borrow_mut()
            .insert((key.clone(), scope.to_owned()));
        if self.dialed.borrow().contains(&key)
            && self
                .seen
                .borrow()
                .get(&key)
                .is_some_and(|c| c.failure.is_some())
        {
            return Some(ForgeHold::Attempted);
        }
        if let Some(settled) = self
            .prior
            .sources
            .get(&key)
            .filter(|c| !c.worth_dialing())
            .cloned()
            && let Some(f) = settled.failure.clone()
        {
            // The standing record already says everything, so it is left exactly where it is.
            // Writing it back would make a round that dialed NOTHING look like one that did, and
            // the clock would then be pushed out on every run over sources nobody asked about. The
            // one thing worth persisting is having finally said it out loud.
            if !f.reported {
                let mut seen = self.seen.borrow_mut();
                let entry = seen.entry(key).or_insert(settled);
                if let Some(e) = &mut entry.failure {
                    e.reported = true;
                }
            }
            if !f.reported {
                self.announce_gone(origin, &f.reason);
            }
            return Some(ForgeHold::Gone {
                reason: f.reason,
                first_time: !f.reported,
            });
        }
        // The breaker is open. The source still had its turn this round, so it is recorded as
        // checked-and-unanswered — otherwise the clock would treat it as never tried and the whole
        // round would come back at the next session start.
        if self.down.get() {
            self.note_short_circuit(origin, git_ref);
            return Some(ForgeHold::Down);
        }
        None
    }

    /// What commit a source points at now, without downloading it.
    fn probe(
        &self,
        origin: &str,
        git_ref: &str,
        spec: &crate::source::RemoteSpec,
    ) -> Result<RepoHead, ClientError> {
        let key = forge_check::question(origin, git_ref);
        if let Some((_, head)) = self.heads.borrow().iter().find(|(k, _)| *k == key) {
            return Ok(head.clone());
        }
        self.dialed.borrow_mut().insert(key.clone());
        match self.git.probe(spec) {
            Ok(head) => {
                self.heads.borrow_mut().push((key, head.clone()));
                // A host's own backoff signal is honored in ONE direction: it may push the next
                // round further out, never pull it in.
                if let Some(at) = head.retry_after_ms {
                    let held = self.backoff.get().unwrap_or(i64::MIN);
                    self.backoff.set(Some(held.max(at)));
                }
                self.note_answer(origin, git_ref, &head.commit);
                Ok(head)
            }
            Err(e) => {
                self.note_fault(origin, git_ref, &e);
                Err(e)
            }
        }
    }

    /// A source's archive, fetched at most once per round.
    fn fetch(
        &self,
        origin: &str,
        git_ref: &str,
        spec: &crate::source::RemoteSpec,
    ) -> Result<Rc<Vec<u8>>, ClientError> {
        let key = forge_check::question(origin, git_ref);
        if let Some((_, bytes)) = self.repos.borrow().iter().find(|(k, _)| *k == key) {
            return Ok(Rc::clone(bytes));
        }
        self.dialed.borrow_mut().insert(key.clone());
        match self.git.fetch(spec) {
            Ok(bytes) => {
                let bytes = Rc::new(bytes);
                self.repos.borrow_mut().push((key, Rc::clone(&bytes)));
                Ok(bytes)
            }
            Err(e) => {
                self.note_fault(origin, git_ref, &e);
                Err(e)
            }
        }
    }

    /// Record a source whose ARCHIVE arrived and decoded — the pinned row's equivalent of a probe
    /// answering, recorded here rather than at the fetch because an HTTP 200 is not yet a landing:
    /// a body that turns out to be unreadable must not be filed as a successful check, and the
    /// commit worth recording is the one the bytes actually carry.
    fn note_landed(&self, origin: &str, git_ref: &str, commit: &str) {
        // The archive names its commit in the SHORT form; a probe earlier this round may have
        // named the same commit in full. Same commit, better spelling — keep the better one rather
        // than writing the fuller answer back down to its own prefix. (The borrow is released
        // before `note_answer` takes its own.)
        let key = forge_check::question(origin, git_ref);
        let already = self.seen.borrow().get(&key).and_then(|c| c.commit.clone());
        if !commit.is_empty()
            && let Some(seen) = already
            && commit_matches(&seen, commit)
            && seen.len() >= commit.len()
        {
            self.note_answer(origin, git_ref, &seen);
            return;
        }
        if commit.is_empty() {
            // No commit could be read from the archive. The check still happened, so record it as
            // answered; claiming a commit nobody found would be worse than admitting none.
            let prior = self.prior.sources.get(&key);
            let record = SourceCheck {
                checked_at_ms: self.now_ms,
                answered_at_ms: Some(self.now_ms),
                commit: prior.and_then(|p| p.commit.clone()),
                failure: None,
                next_check_at: self.carried_due(&key),
                settled_at: self.carried_settled(&key),
            };
            self.seen.borrow_mut().insert(key, record);
            return;
        }
        self.note_answer(origin, git_ref, commit);
    }

    /// Record a source that answered. Clears any standing failure — including a settled one, so a
    /// repo that comes back is simply back.
    fn note_answer(&self, origin: &str, git_ref: &str, commit: &str) {
        let key = forge_check::question(origin, git_ref);
        let record = SourceCheck {
            checked_at_ms: self.now_ms,
            answered_at_ms: Some(self.now_ms),
            commit: Some(commit.to_owned()),
            failure: None,
            next_check_at: self.carried_due(&key),
            settled_at: self.carried_settled(&key),
        };
        self.seen.borrow_mut().insert(key, record);
    }

    /// The convergence marks already standing for a question. They survive every outcome: whether
    /// a check answered, failed, or was skipped says nothing about whether a scope had previously
    /// finished converging, and dropping them would send the next round back to the archive.
    fn carried_settled(&self, key: &str) -> BTreeMap<String, forge_check::Settled> {
        self.seen
            .borrow()
            .get(key)
            .map(|c| c.settled_at.clone())
            .filter(|m| !m.is_empty())
            .or_else(|| self.prior.sources.get(key).map(|c| c.settled_at.clone()))
            .unwrap_or_default()
    }

    /// Record a failed check, and trip the breaker if the forge was never reached at all.
    fn note_fault(&self, origin: &str, git_ref: &str, e: &ClientError) {
        let fault = match e {
            ClientError::RemoteFetch { fault, .. } => *fault,
            // Anything else got far enough to be a fault about the bytes, not about the dial.
            _ => FetchFault::unavailable(),
        };
        if !fault.reached() {
            self.down.set(true);
        }
        // A host that answered "not now, come back at T" said it on the way to failing. Honor it
        // in the one direction backoffs are ever honored: later, never sooner.
        if let Some(at) = fault.retry_after_ms() {
            let held = self.backoff.get().unwrap_or(i64::MIN);
            self.backoff.set(Some(held.max(at)));
        }
        let key = forge_check::question(origin, git_ref);
        let prior = self.prior.sources.get(&key);
        let record = SourceCheck {
            checked_at_ms: self.now_ms,
            answered_at_ms: prior.and_then(|p| p.answered_at_ms),
            commit: prior.and_then(|p| p.commit.clone()),
            failure: Some(CheckFailure {
                reason: crate::render::safe_message(e),
                gone: fault.permanent(),
                reached: fault.reached(),
                git_ref: git_ref.to_owned(),
                // A final answer is SAID by the arm that received it, in this same round, so it is
                // recorded as already said. Recording it unsaid would make the round that
                // discovered the deletion and the round after it both announce it.
                reported: fault.permanent(),
            }),
            next_check_at: self.carried_due(&key),
            settled_at: self.carried_settled(&key),
        };
        self.seen.borrow_mut().insert(key, record);
        if fault.permanent() {
            self.announce_gone(origin, &crate::render::safe_message(e));
        }
    }

    /// The per-scope turns already recorded for a question, so an outcome that reschedules only
    /// the scope that ran does not wipe another scope's standing turn.
    fn carried_due(&self, key: &str) -> BTreeMap<String, i64> {
        self.seen
            .borrow()
            .get(key)
            .map(|c| c.next_check_at.clone())
            .filter(|m| !m.is_empty())
            .or_else(|| self.prior.sources.get(key).map(|c| c.next_check_at.clone()))
            .unwrap_or_default()
    }

    /// Whether this question was already reconciled to completion in this scope AT `head`, AND the
    /// scope's store still proves it — every member that head holds is tracked here, at that head.
    ///
    /// The re-proof is the whole point. The mark lives in machine state, which outlives any one
    /// checkout: delete a project's store or reclone at the same path and the mark survives while
    /// the bytes it vouches for do not. Trusting it unread would leave the fresh checkout with the
    /// row installed nowhere and the lane convinced there was nothing to do. A head holding NO
    /// members has nothing to prove, which is the case the mark exists for.
    fn settled_here(
        &self,
        origin: &str,
        git_ref: &str,
        scope: &str,
        head: &str,
        tracked_at: impl Fn(&str, &str) -> bool,
    ) -> bool {
        if head.is_empty() {
            return false;
        }
        self.prior
            .sources
            .get(&forge_check::question(origin, git_ref))
            .and_then(|c| c.settled_at.get(scope))
            .is_some_and(|mark| {
                mark.head == head && mark.members.iter().all(|m| tracked_at(m, head))
            })
    }

    /// Record that this question is fully converged in this scope at `head`, holding `members`.
    fn note_settled(
        &self,
        origin: &str,
        git_ref: &str,
        scope: &str,
        head: &str,
        members: &[String],
    ) {
        if head.is_empty() {
            return;
        }
        let key = forge_check::question(origin, git_ref);
        let carried = self.carried_settled(&key);
        let mut seen = self.seen.borrow_mut();
        let entry = seen.entry(key).or_default();
        if entry.settled_at.is_empty() {
            entry.settled_at = carried;
        }
        entry.settled_at.insert(
            scope.to_owned(),
            forge_check::Settled {
                head: head.to_owned(),
                members: members.to_vec(),
            },
        );
    }

    /// A final refusal, as the one sentence a person gets about it.
    fn announce_gone(&self, origin: &str, reason: &str) {
        let line = format!(
            "topos: {origin} — {reason}; the copies here still work, and `topos update` retries \
             once the row names something that resolves"
        );
        let mut said = self.announce.borrow_mut();
        if !said.contains(&line) {
            said.push(line);
        }
    }

    /// Record the round's short-circuit for a source the breaker skipped — it had its turn, and
    /// the clock must treat it as checked.
    fn note_short_circuit(&self, origin: &str, git_ref: &str) {
        let key = forge_check::question(origin, git_ref);
        let prior = self.prior.sources.get(&key);
        self.seen
            .borrow_mut()
            .entry(key.clone())
            .or_insert_with(|| SourceCheck {
                checked_at_ms: self.now_ms,
                answered_at_ms: prior.and_then(|p| p.answered_at_ms),
                commit: prior.and_then(|p| p.commit.clone()),
                failure: Some(CheckFailure {
                    reason: "the forge was already unreachable earlier in this run — the \
                             remaining sources were skipped"
                        .to_owned(),
                    gone: false,
                    reached: false,
                    git_ref: git_ref.to_owned(),
                    reported: false,
                }),
                next_check_at: prior.map(|p| p.next_check_at.clone()).unwrap_or_default(),
                settled_at: prior.map(|p| p.settled_at.clone()).unwrap_or_default(),
            });
    }
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
/// A session-file read failure; a manifest this run would DRIVE that fails to load (the typed
/// manifest refusal, naming the file and the fix — the run refuses whole rather than print a
/// success-claiming receipt over a recipe it never read); or an unmatched `update <target>` (the
/// typed refusal names the fix).
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
            // The mcp hold computation reads this cache; blind, it must freeze removals.
            sweep.mcp_blind = true;
            sync_status::SyncStatus::default()
        }
    };

    // ---- 1. Build the two scope plans, FIRST. A manifest this run would DRIVE that fails to
    // load REFUSES the run whole — before any session is dialed, any state is touched, or any
    // receipt could claim a sweep over a recipe it never read. A scope this run does NOT drive
    // never blocks it: its failure degrades to a warning and its plan is simply absent, which
    // freezes it exactly like a scope that was not driven (the failure mode of a mistake must be
    // keeping bytes, never a success-claiming no-op). ----
    let person_load = scopes::person_plan(ctx.fs, &ctx.layout);
    let cwd = ctx.roots.as_ref().and_then(|r| r.cwd.clone());
    let home = ctx.roots.as_ref().map(|r| r.home.clone());
    let mut project: Option<(PathBuf, ScopePlan)> = None;
    let mut project_err: Option<ClientError> = None;
    if let Some(cwd) = &cwd {
        match scopes::nearest_project_plan(ctx.fs, cwd, home.as_deref()) {
            Ok(found) => project = found,
            Err(e) => project_err = Some(e),
        }
    }
    let project_frozen = project_err.is_some();
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
    if driven.project
        && let Some(e) = project_err.take()
    {
        return Err(e);
    }
    let person: Option<ScopePlan> = match person_load {
        Ok(p) => Some(p),
        Err(e) => {
            if driven.person {
                return Err(e);
            }
            sweep
                .warnings
                .push(format!("MANIFEST_INVALID {}", e.detail()));
            None
        }
    };
    if let Some(e) = &project_err {
        sweep
            .warnings
            .push(format!("MANIFEST_INVALID {}", e.detail()));
    }
    let project_display = project_dir
        .as_deref()
        .map(|d| super::inventory::pretty(ctx, d));
    let scope_label = driven.label(project_display.as_deref());

    // ---- 2. Dial each live session's delivery. ----
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
            // The server refuses this build outright — the one fault a person can clear right now,
            // so the warning names the fix instead of the network. It degrades like the three
            // below (the offline cache keeps the local converge working); the sweep never aborts
            // for one session.
            Err(PlaneError::UpdateRequired { min }) => {
                sweep.warnings.push(format!(
                    "CLI_UPDATE_REQUIRED {}: {} — run `topos self-update`",
                    s.workspace_name,
                    crate::render::safe_message(&ClientError::UpdateRequired { min })
                ));
                unreachable.push(stale_signal(s, StaleReason::Unavailable));
                runs.push(offline_run(s, transports));
            }
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

    // Record every fault above against its workspace's freshness row. The quiet hook's warning is
    // transient (and silent inside the staleness window), so without this a later read would show
    // plain history with no hint that nothing has refreshed it. Best-effort, exactly like the
    // successful write below: a freshness-cache failure must not turn a degraded run into a hard one.
    let faults: Vec<(String, ExchangeFault)> = unreachable
        .iter()
        .map(|u| (u.workspace_id.clone(), u.reason.fault()))
        .collect();
    if let Err(e) = sync_status::record_faults(ctx.fs, &ctx.layout, &faults) {
        sweep
            .warnings
            .push(format!("SYNC_STATUS_WRITE_FAILED: {}", e.detail()));
    }

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
    handover_legacy_project_rows(
        ctx,
        &handover_dirs,
        &mut sweep.advisories,
        &mut sweep.warnings,
    );

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
                    },
                );
            }
        }
    }

    // ---- The forge lane's own clock. Delivery has already run above on the sweep's cadence;
    // whether a repository is asked about is a separate decision on a much slower schedule, and a
    // run that is not due simply carries no lane (the arms then converge tracked bytes in place,
    // exactly as they do offline).
    let forge_state = forge_check::read(ctx.fs, &ctx.layout);
    let lane = git.map(|g| ForgeLane::new(g, forge_state, now_millis, opts.forge));

    let env = Env {
        ctx,
        runs: &runs,
        follow: &follow,
        forge: lane.as_ref(),
        prior: &prior_sync,
        default_host: super::manifest_edit::default_host(ctx),
    };

    // ---- 3. `--force`, BEFORE the fan-out: absorb, then drop, then let the sweep re-project.
    // A rebuild rebuilds exactly what this run converges: re-projecting a store no scope drives
    // would drop placement dirs nothing is about to write back.
    if opts.rebuild {
        if driven.person {
            rebuild_store(ctx, &ctx.layout, &mut sweep);
        }
        if driven.project
            && let Some((dir, _)) = &project
            && let Some(playout) = sidecar::existing_project_store(ctx.fs, dir)
        {
            let pctx = super::pull::ctx_with_layout(ctx, &playout);
            rebuild_store(&pctx, &playout, &mut sweep);
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
        // A dial that already happened must be paid for even though this run is about to refuse:
        // the sources contacted above had their turn, and leaving the clock untouched would make
        // a mistyped target a way to re-dial the forge as often as someone retypes it.
        if let Some(lane) = &lane {
            let _ = close_forge_round(ctx, lane, now_millis, &mut sweep.warnings, &mut Vec::new());
        }
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

    // A re-demanded identity REVIVES a retired record: something claimed it again this run (a
    // row, a feed, a targeted update), so the custody record returns to every surface it
    // describes. Runs whatever the targets — reviving rides the claim, not the sweep shape.
    revive_reclaimed(&env, &driven, project.as_ref(), &sweep);

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
        // ---- 6.1 The ONE-TIME ORPHAN RESOLUTION: each store record no row claims and nothing
        // delivers resolves once — said on this receipt, then retired from every surface. Only a
        // full sweep may resolve (a targeted update cannot know the whole demand set), under the
        // same freeze discipline the cleaner keeps.
        resolve_orphans(
            &env,
            &driven,
            &all_sessions,
            person.is_some(),
            project.as_ref(),
            project_frozen,
            &mut sweep,
        );
    }

    // ---- 6.5 MCP convergence, per scope: the demanded bundles' server entries land in (or
    // leave) each engaged agent's config, from the store's held bytes — offline included. Runs
    // AFTER the cleaning (removal convergence must see the settled demand set) and BEFORE the
    // report (the per-agent states ride the applied rows and the delivery cache).
    let mcp_states = run_mcp_converge(
        &env,
        &driven,
        person.as_ref(),
        project.as_ref(),
        project_frozen,
        opts,
        &mut sweep,
    );

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
    // The receipt's closing arithmetic about the scope this run left alone (see [`stale_scopes`]).
    let mut behind_elsewhere: Vec<topos_types::results::BehindElsewhere> = Vec::new();
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
        // The workspace's CURRENT per bundle, straight from the delivery answer — the only thing
        // that makes "behind" a fact rather than a difference. Manifest-row deliveries are absent
        // on purpose: their served version may itself be a pin, which is not a current.
        let current: HashMap<&str, [u8; 32]> = snap
            .skills
            .iter()
            .map(|s| (s.skill_id.as_str(), s.version_id))
            .collect();
        let mut report_ok = false;
        match super::pull::applied_snapshot(ctx, &delivered_ids, &visited_stores, &current) {
            Ok(snapshot) => {
                // The wire carries ONE row per (session, bundle); a store this run did not bring
                // current says so on the receipt, where the pick was made — a local fact, stated
                // whether or not the report reaches the plane.
                behind_elsewhere.extend(stale_scopes(
                    ctx,
                    &snapshot.splits,
                    snap,
                    &run.session,
                    &driven,
                    person.as_ref(),
                    project.as_ref(),
                ));
                // Each mcp row carries its per-agent states (the fleet page's truth); file
                // bundles ride with an empty list, byte-identical to the prior wire shape.
                let applied_rows: Vec<crate::plane::AppliedSkillReport> = snapshot
                    .applied
                    .iter()
                    .map(|(skill_id, commit)| crate::plane::AppliedSkillReport {
                        skill_id: skill_id.clone(),
                        version_id: *commit,
                        harnesses: mcp_states.get(skill_id).cloned().unwrap_or_default(),
                    })
                    .collect();
                match run
                    .transports
                    .plane
                    .report_applied(&run.session.workspace_id, &applied_rows)
                {
                    Ok(()) => report_ok = true,
                    Err(e) => {
                        let m = match e {
                            PlaneError::NotFound => "access gone".to_owned(),
                            PlaneError::UpdateRequired { .. } => {
                                "this topos is too old for that server".to_owned()
                            }
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
            // The per-scope batches already WARNED about a kind this build cannot deliver; the
            // cache must not record it either, or the next offline sweep would read the row back
            // and place it as a skill.
            let Some(kind) = BundleKind::parse(&ds.kind) else {
                continue;
            };
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
                    kind: kind.tag(),
                    harness_states: Vec::new(),
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
        // The converge's per-agent states ride the cache rows (the offline `list <name>` answer).
        for (skill_id, row) in &mut delivered_cache {
            if let Some(states) = mcp_states.get(skill_id) {
                row.harness_states = states.clone();
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
                // This exchange LANDED — the entry is replaced wholesale, so any fault a previous
                // run recorded for this workspace goes with it.
                last_exchange_fault: None,
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
                        PlaneError::UpdateRequired { .. } => {
                            "this topos is too old for that server".to_owned()
                        }
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

    // ---- 8. CLOSE THE FORGE ROUND. This is the single place the clock is written, and it is
    // written whether the round found everything current, moved bytes, or reached nothing at all.
    // A failed round that left the due time untouched would retry at the very next session start
    // — turning the one failure into a request per session, which is the traffic the interval
    // exists to prevent. So: the lane ran, therefore it waits.
    let mut forge_gone: Vec<String> = Vec::new();
    let stale_forge = match lane {
        Some(lane) => {
            close_forge_round(ctx, &lane, now_millis, &mut sweep.warnings, &mut forge_gone)
        }
        None => Vec::new(),
    };

    behind_elsewhere.sort_by(|a, b| (&a.project_dir, &a.bundle).cmp(&(&b.project_dir, &b.bundle)));
    behind_elsewhere.dedup();

    Ok(PullOutcome {
        data: PullData {
            skills: sweep.rows,
            proposals_awaiting,
            notices,
            sync,
            scope: Some(scope_label),
            behind_elsewhere,
        },
        warnings: sweep.warnings,
        failed_bundles: sweep.failed_bundles,
        fault_actions: sweep.fault_actions,
        decisions: sweep.decisions,
        advisories: sweep.advisories,
        disclosures: sweep.disclosures,
        access_gone,
        unreachable,
        stale_forge,
        forge_gone,
        failed_channels: sweep.failed_channels,
    })
}

/// The cross-scope STALENESS this run has standing to speak about — the receipt's closing line,
/// as data (the renderer counts and phrases it).
///
/// **Staleness, not difference.** Two scopes holding different bytes is the design working: they
/// never blend. What is worth a line is a copy that is accidentally OLD — behind the current the
/// delivery answer just stated — and that a command from here would fix. Three filters get there,
/// each one removing a way the line could nag forever with no cure:
///
/// - **A driven scope is never named.** Its outcome is on the receipt already, row by row; a
///   second line about the same bundle would say it twice, and the suggested command is the one
///   that was just run.
/// - **Only a scope whose recipe this run READ** — the person plan, and the project plan covering
///   the cwd. Another checkout's `topos.toml` was never opened, so its copy cannot be told apart
///   from a deliberate pin, and guessing would be exactly the nagging this filter exists to stop.
/// - **Only a row that is not deliberately fixed there** (see [`deliberately_fixed`]).
fn stale_scopes(
    ctx: &Ctx<'_>,
    splits: &[super::pull::VersionSplit],
    snap: &DeliverySnapshot,
    session: &Session,
    driven: &Driven,
    person: Option<&ScopePlan>,
    project: Option<&(PathBuf, ScopePlan)>,
) -> Vec<topos_types::results::BehindElsewhere> {
    let names: HashMap<&str, &str> = snap
        .skills
        .iter()
        .map(|s| (s.skill_id.as_str(), s.name.as_str()))
        .collect();
    splits
        .iter()
        .filter(|s| s.behind)
        .filter_map(|s| {
            let bundle = *names.get(s.skill_id.as_str())?;
            let (plan, dir) = match &s.project_dir {
                // The machine's own store: read whenever this run got that far, and named by the
                // absent `project_dir` (the renderer spells it `machine-wide`).
                None => (person?, None),
                // Exactly ONE project store has a plan here: the one covering the cwd.
                Some(dir) => {
                    let (pdir, plan) = project?;
                    if pdir != dir {
                        return None;
                    }
                    (plan, Some(dir.as_path()))
                }
            };
            let this_scope_ran = if dir.is_none() {
                driven.person
            } else {
                driven.project
            };
            if this_scope_ran
                || deliberately_fixed(plan, &session.host, &session.workspace_name, bundle)
            {
                return None;
            }
            Some(topos_types::results::BehindElsewhere {
                bundle: bundle.to_owned(),
                project_dir: dir.map(|d| super::inventory::pretty(ctx, d)),
            })
        })
        .collect()
}

/// Whether a scope's recipe deliberately holds `bundle` somewhere other than current — an `"off"`
/// switch, or a version pin on the bundle's own row. Either is a choice a person wrote down, and
/// no `topos update` would ever undo it: naming one as "behind" would be a line that can never be
/// cleared.
///
/// There is deliberately no third, SET-level test. The only sets a workspace bundle can arrive
/// through are channels, and the manifest grammar refuses a pin on a channel in both spellings
/// (`= "<hash>"` and a `version` field) — so a pinned set that delivers this bundle is not a state
/// the parser can produce, and a branch for it would be a rule no fixture could ever reach.
fn deliberately_fixed(plan: &ScopePlan, host: &str, workspace: &str, bundle: &str) -> bool {
    let is_this_bundle = |r: &PlanRow| {
        matches!(&r.shape, KeyShape::WorkspaceBundle { host: h, workspace: w, bundle: b }
            if h == host && w == workspace && b == bundle)
    };
    plan.off_for(host, workspace, bundle).is_some()
        || plan
            .things
            .iter()
            .any(|r| is_this_bundle(r) && r.pin().is_some())
}

/// Persist what the forge round learned and hand back the per-HOST staleness signals the silent
/// renderer gates on.
///
/// Per host, never per row: five rows behind one unreachable forge is ONE thing that happened, and
/// saying it five times into an agent's context window would be five times the interruption for
/// the same fact. The signal carries the OLDEST last-answer across the host's failing sources, so
/// the renderer's staleness question is asked about the source that has gone longest without one.
fn close_forge_round(
    ctx: &Ctx<'_>,
    lane: &ForgeLane<'_>,
    now_ms: i64,
    warnings: &mut Vec<String>,
    gone: &mut Vec<String>,
) -> Vec<StaleForge> {
    gone.extend(lane.announce.borrow().iter().cloned());
    let outcomes: Vec<(String, SourceCheck)> = lane.seen.borrow().clone().into_iter().collect();
    if outcomes.is_empty() {
        // Nothing was checked — every row settled on its pin, or this machine tracks no external
        // source at all. There is no turn to have taken, so there is no next turn to schedule.
        return Vec::new();
    }
    // Each question schedules ITSELF, with its own independent spread: a fleet that jittered as
    // one machine would still arrive in waves per repository.
    let turns = lane.turns.borrow();
    let outcomes: Vec<(String, SourceCheck)> = outcomes
        .into_iter()
        .map(|(key, mut check)| {
            // Each SCOPE that took a turn schedules its own next one, with its own independent
            // spread: a fleet that jittered as one machine would still arrive in waves.
            for (_, scope) in turns.iter().filter(|(q, _)| *q == key) {
                let jitter = i64::try_from(
                    ctx.ids
                        .jitter_below(u64::try_from(forge_check::CHECK_JITTER_MS).unwrap_or(0)),
                )
                .unwrap_or(0);
                check
                    .next_check_at
                    .insert(scope.clone(), forge_check::next_due(now_ms, jitter));
            }
            (key, check)
        })
        .collect();
    if let Err(e) = forge_check::record_round(ctx.fs, &ctx.layout, lane.backoff.get(), &outcomes) {
        warnings.push(format!("FORGE_CHECK_WRITE_FAILED: {}", e.detail()));
    }

    let mut by_host: BTreeMap<String, StaleForge> = BTreeMap::new();
    for (source, check) in &outcomes {
        // A source the forge ANSWERED about — including "it is gone" — is not a host that went
        // quiet: it said its piece, and the row-level line already carried it. The REASON is not
        // carried into this line either: it is one sentence in an agent's context window, and
        // `status`/`list` are where the detail belongs.
        let Some(failure) = check.failure.as_ref().filter(|f| !f.gone) else {
            continue;
        };
        let host = source.split('/').next().unwrap_or(source).to_owned();
        let entry = by_host.entry(host.clone()).or_insert_with(|| StaleForge {
            host,
            sources: 0,
            answered_at: check.answered_at_ms,
            reached: failure.reached,
        });
        entry.sources += 1;
        // A host that answered ANY of its rows was reached. Calling it unreachable because a
        // different row also failed would name the wrong problem, and the reason clause beside it
        // would contradict the word.
        entry.reached |= failure.reached;
        entry.answered_at = match (entry.answered_at, check.answered_at_ms) {
            // "Never answered" is the oldest thing there is; it just is not stale (see
            // `forge_check::is_stale`), so it wins the fold and stays honest.
            (_, None) | (None, _) => None,
            (Some(a), Some(b)) => Some(a.min(b)),
        };
    }
    by_host.into_values().collect()
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
            let Some(kind) = served_kind(&entry.kind, &entry.name, &sc.label, &mut sweep.warnings)
            else {
                return;
            };
            let mcp = kind.is_mcp();
            let target = CatalogTarget::from_entry(entry, kind);
            // A config-placed bundle has no placement dirs — its dest entries are config FILES
            // and ride the demand's narrowing; a skill row's dest is its frozen placement plan.
            let dest = if mcp { None } else { row.fields().dest };
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
                    kind: kind.tag(),
                    harness_states: Vec::new(),
                    picked: false,
                },
            ));
            // `warn_unknown` rides the row's KIND: a SKILL row's dest entries are placement
            // FOLDERS (`~/.codex/skills`), not MCP config files — only an mcp row's dest can
            // mean config files, so only it may warn about an unknown one.
            let narrowing = mcp_filter(
                sc,
                Some(row),
                &display,
                mcp,
                &mut sweep.mcp_warned_dests,
                &mut sweep.advisories,
                &mut sweep.warnings,
            );
            let st = SyncTarget {
                mcp_dest_filter: narrowing.filter,
                mcp_unreachable: narrowing.unreachable,
                target,
                pin: row.pin(),
                display: display.clone(),
                dest,
                step: None,
            };
            sync_workspace_skill(env, sc, run, &st, sweep);
        }
        KeyShape::RepoSkill {
            host,
            owner,
            repo,
            skill,
        } => {
            reconcile_repo_skill(env, sc, row, host, owner, repo, skill, sweep);
        }
        KeyShape::LocalPath { raw } => {
            let dir = local_dir(env.ctx, sc, raw);
            if env.ctx.fs.exists(&dir) {
                // A path row whose dir is a placement of an already-GOVERNED bundle is a landed
                // publish's PENDING transfer (the local rewrite half failed) — converge it here,
                // idempotently, disclosed.
                let mut row_index = None;
                if !converge_pending_governance(env, &dir, sweep) {
                    row_index = Some(sweep.next_row_index());
                    let mut r = plain_row(&display, PullAction::UpToDate, None, &sc.label);
                    // An adopted folder the person is still editing is a DRAFT, and this row is
                    // the one `update` prints about it. Delivery owed it nothing — which is why
                    // the row reads `up to date` — but `list` calls it a draft and `status`
                    // counts it as one, and a receipt that stayed silent made three surfaces
                    // disagree about one machine.
                    r.draft = local_row_drafted(env, sc, &dir);
                    sweep.push(r);
                }
                // A `kind = "mcp"` path row: the dir IS the bundle (`server.json` at its root) —
                // adopted-path custody as ever, no skill placement; the demand feeds the scope's
                // MCP converge with `workspace_slug: None`.
                if row.value.declared_kind() == Some(BundleKind::Mcp) {
                    local_mcp_demand(env, sc, row, &dir, &display, row_index, sweep);
                } else if let Some(dest) = row.fields().dest.filter(|d| !d.is_empty()) {
                    // A SKILL path row with `dest = [...]`: the adopted folder stays the
                    // person's own working copy, and a managed COPY is kept at each named
                    // destination — the same dest planner (and grow/shrink discipline) the
                    // workspace rows ride.
                    converge_local_dest(env, sc, &dir, &display, &dest, sweep);
                }
            } else {
                // The remediation is SCOPE-EXACT, like every other command this CLI offers: the
                // row lives in one file, and `remove` without `-g` edits the other one. The
                // command used to be spelled the same whichever file carried the row, so on the
                // machine-wide file it refused — leaving the only named way out of a permanent
                // warning as a command that could not clear it.
                let (g, whose) = if matches!(sc.scope, ResolvedScope::Person) {
                    (" -g", "machine-wide")
                } else {
                    ("", "by this project")
                };
                // The scope is named in the PERSON'S vocabulary, not the resolver's: `person` is
                // an internal word for the machine-wide scope, and it shipped verbatim to anyone
                // who deleted a folder a row still asks for. The sentence also reads whole with
                // the leading code removed, which is how the TTY prints it.
                sweep.warnings.push(format!(
                    "PATH_MISSING \"{raw}\" is demanded {whose} but the folder is gone — \
                     `topos remove{g} {raw}` drops the row"
                ));
                // The BUNDLE is what could not be carried forward, so the bundle is what the
                // summary counts. Pushing only a line left a one-row failing sweep printing
                // "Checked 0 skills" and exiting 1 — a receipt that reported nothing wrong beside
                // a status that said something was.
                sweep.failed_bundles.insert(display.clone());
                // …and the way out rides the machine channel too, spelled for the file that
                // actually carries the row.
                let mut argv = vec!["topos".to_owned(), "remove".to_owned()];
                if !g.is_empty() {
                    argv.push("-g".to_owned());
                }
                argv.push(raw.to_string());
                argv.push("--json".to_owned());
                sweep.fault_actions.push(crate::actions::next_action(
                    topos_types::ActionCode::from("REMOVE_MISSING_ROW".to_owned()),
                    argv,
                ));
                if row.value.declared_kind() == Some(BundleKind::Mcp) {
                    // The bundle cannot be read this run: hold its config entries in place.
                    sweep
                        .mcp_hold
                        .entry(sc.label.clone())
                        .or_default()
                        .insert(local_bundle_identity(env, sc, &dir, &display));
                }
            }
        }
        // A feed or channel never lands in `things`; a repo set never does either.
        _ => {}
    }
}

/// Resolve one demand's MCP config-file narrowing AND say what it cost. `warn_unknown` is true
/// only for rows whose dest can ONLY mean config files (an mcp bundle row, a local mcp row); a
/// SKILL row's dest names placement folders and a CHANNEL's dest may name folders for its skill
/// members, so their unmapped entries stay silent. `bundle` is the display name every line leads
/// with.
///
/// The two outcomes are told apart because they are not the same news. A dest that maps SOME of
/// its entries still delivers, so the dropped entries are an advisory beside a working row. A dest
/// that maps NONE of them delivers nowhere at all — a typo silently costing the bundle every
/// agent — so it is a warning, and the row it belongs to carries the same clause (returned here as
/// `unreachable`) rather than reading like a healthy install.
fn mcp_filter(
    sc: &ScopeCtx<'_>,
    row: Option<&PlanRow>,
    bundle: &str,
    warn_unknown: bool,
    warned: &mut HashSet<String>,
    advisories: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> McpNarrowing {
    let scope = manifest_scope_of(sc);
    let narrowing = mcp_dest_narrowing(row.and_then(|r| r.fields().dest), scope);
    let label = &sc.label;
    if !warn_unknown || narrowing.unknown.is_empty() {
        return McpNarrowing {
            filter: narrowing.filter,
            unreachable: None,
        };
    }
    if narrowing.reaches_nothing() {
        let clause = crate::manifest::dest::dest_names_no_mcp_file(&narrowing.unknown, scope);
        // Deduped per BUNDLE, not per entry: this line is about a bundle that reaches nothing, and
        // keying it on the entry would let an advisory raised for some OTHER bundle's typo of the
        // same spelling swallow it — the exact silence this warning exists to end. (Its own key
        // space, so it can never collide with the per-entry advisory keys either.)
        if warned.insert(format!("no-agent\u{1f}{label}\u{1f}{bundle}")) {
            warnings.push(format!(
                "MCP_DEST_NO_AGENT {label}: \"{bundle}\" reaches no agent — {clause}"
            ));
        }
        return McpNarrowing {
            filter: narrowing.filter,
            unreachable: Some(clause),
        };
    }
    for entry in &narrowing.unknown {
        if warned.insert(entry.clone()) {
            advisories.push(format!(
                "MCP_DEST_UNKNOWN {label}: \"{bundle}\" — {}",
                crate::manifest::dest::unknown_mcp_file(entry, scope)
            ));
        }
    }
    McpNarrowing {
        filter: narrowing.filter,
        unreachable: None,
    }
}

/// What [`mcp_filter`] hands one demand: the harness narrowing itself, plus the clause the row
/// must state when that narrowing leaves the bundle reaching NOTHING.
struct McpNarrowing {
    filter: Option<Vec<String>>,
    unreachable: Option<String>,
}

/// The grammar scope a resolved scope reads/writes in.
fn manifest_scope_of(sc: &ScopeCtx<'_>) -> crate::manifest::document::ManifestScope {
    match sc.scope {
        ResolvedScope::Person => crate::manifest::document::ManifestScope::Global,
        ResolvedScope::Project { .. } => crate::manifest::document::ManifestScope::Project,
    }
}

/// One demand's resolved dest-file narrowing.
pub(crate) struct DestNarrowing {
    /// The harnesses to place into. `None` = the row has no `dest` (every MCP-capable agent, now
    /// and later); `Some` = exactly the named files' agents — possibly EMPTY, because a dest row
    /// is FROZEN to what it names.
    pub filter: Option<Vec<String>>,
    /// The dest entries no MCP-capable harness claims, in row order.
    pub unknown: Vec<String>,
}

impl DestNarrowing {
    /// Whether the narrowing leaves the bundle no agent at all to reach — a dest naming only
    /// files topos cannot edit. Never true of a row carrying no `dest`, which reaches every
    /// MCP-capable agent.
    pub(crate) fn reaches_nothing(&self) -> bool {
        self.filter.as_ref().is_some_and(Vec::is_empty)
    }
}

/// The ONE resolution of an MCP demand's dest-file narrowing — shared by the sweep (through
/// [`mcp_filter`]) and `add --kind mcp`'s inline converge, so the add can never fan out past what
/// the next sweep would keep. Each entry is matched against the descriptor table's config-file
/// spellings for the scope (default spelling, or the resolved env-override path); an entry no
/// harness claims is dropped from the narrowing and reported back in `unknown` — this resolution
/// decides reach, never wording.
pub(crate) fn mcp_dest_narrowing(
    row_dest: Option<Vec<String>>,
    scope: crate::manifest::document::ManifestScope,
) -> DestNarrowing {
    let Some(dest) = row_dest else {
        return DestNarrowing {
            filter: None,
            unknown: Vec::new(),
        };
    };
    let mut mapped: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    for entry in &dest {
        match crate::manifest::dest::mcp_slug_for_dest(entry, scope) {
            Some(slug) => {
                if !mapped.iter().any(|s| s == slug) {
                    mapped.push(slug.to_owned());
                }
            }
            None => {
                if !unknown.contains(entry) {
                    unknown.push(entry.clone());
                }
            }
        }
    }
    DestNarrowing {
        filter: Some(mapped),
        unknown,
    }
}

/// The custody identity of a LOCAL `kind = "mcp"` row: the tracked skill id when THIS SCOPE'S
/// OWN store records the dir (custody survives a later publish, which keeps the id), else a
/// name-keyed local identity. ONLY the scope's store is asked — the same rule `add --kind mcp`'s
/// inline converge minted the config key under — because the OTHER scope may track the same
/// folder under its own id, and answering with it would retire this scope's standing key and
/// re-mint a suffixed one: the one way a config entry's name could move, stranding any OAuth
/// token a harness filed under it.
fn local_bundle_identity(env: &Env<'_>, sc: &ScopeCtx<'_>, dir: &Path, display: &str) -> String {
    let scope_layout = match &sc.scope {
        ResolvedScope::Person => Some(env.ctx.layout.clone()),
        ResolvedScope::Project { dir: project_dir } => {
            sidecar::existing_project_store(env.ctx.fs, project_dir)
        }
    };
    dir.canonicalize()
        .ok()
        .zip(scope_layout)
        .and_then(|(canonical, layout)| {
            let sctx = super::pull::ctx_with_layout(env.ctx, &layout);
            super::add::tracked_skill_at(&sctx, &canonical)
                .ok()
                .flatten()
        })
        .unwrap_or_else(|| format!("local:{display}"))
}

/// Whether the record a LOCAL path row tracks carries local edits — the same scan `list` and
/// `status` read, over the store that owns the record (the scope's own, not always the home's).
/// Best-effort: an unresolvable record or an unreadable map reads as "no draft", exactly like the
/// other two surfaces, so the three never disagree by construction.
fn local_row_drafted(env: &Env<'_>, sc: &ScopeCtx<'_>, dir: &Path) -> bool {
    let scope_layout = match &sc.scope {
        ResolvedScope::Person => Some(env.ctx.layout.clone()),
        ResolvedScope::Project { dir: project_dir } => {
            sidecar::existing_project_store(env.ctx.fs, project_dir)
        }
    };
    let Some((canonical, layout)) = dir.canonicalize().ok().zip(scope_layout) else {
        return false;
    };
    let sctx = super::pull::ctx_with_layout(env.ctx, &layout);
    let Ok(Some(id)) = super::add::tracked_skill_at(&sctx, &canonical) else {
        return false;
    };
    SkillId::parse(&id).is_ok_and(|sid| super::store_has_draft(&sctx, &sid))
}

/// Feed the scope's MCP demand list from a LOCAL path row: `server.json` read straight from the
/// row's dir (the dir IS the bundle). An unreadable/absent `server.json` warns and HOLDS the
/// bundle's standing entries rather than reading absence as an empty server.
fn local_mcp_demand(
    env: &Env<'_>,
    sc: &ScopeCtx<'_>,
    row: &PlanRow,
    dir: &Path,
    display: &str,
    row_index: Option<usize>,
    sweep: &mut Sweep,
) {
    let bundle_id = local_bundle_identity(env, sc, dir, display);
    match env.ctx.fs.read_opt(&dir.join("server.json")) {
        Ok(Some(bytes)) => {
            let narrowing = mcp_filter(
                sc,
                Some(row),
                display,
                true,
                &mut sweep.mcp_warned_dests,
                &mut sweep.advisories,
                &mut sweep.warnings,
            );
            sweep.note_mcp_row(
                &sc.label,
                &bundle_id,
                row_index,
                narrowing.unreachable.as_deref(),
            );
            sweep.mcp_demands.entry(sc.label.clone()).or_default().push(
                crate::mcp_engine::DemandedBundle {
                    bundle_id,
                    name: display.to_owned(),
                    workspace_slug: None,
                    version_id: String::new(),
                    server_json: bytes,
                    reach: narrowing.filter,
                },
            );
        }
        Ok(None) => {
            sweep.warnings.push(format!(
                "MCP_UNPLACEABLE {}: \"{display}\" — the folder holds no server.json at its root",
                sc.label
            ));
            sweep
                .mcp_hold
                .entry(sc.label.clone())
                .or_default()
                .insert(bundle_id);
        }
        Err(e) => {
            note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                display,
                &e.into(),
            );
            sweep
                .mcp_hold
                .entry(sc.label.clone())
                .or_default()
                .insert(bundle_id);
        }
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
                let Some(kind) =
                    served_kind(&entry.kind, &entry.name, &sc.label, &mut sweep.warnings)
                else {
                    continue;
                };
                let mcp = kind.is_mcp();
                let target = CatalogTarget::from_entry(entry, kind);
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
                        kind: kind.tag(),
                        harness_states: Vec::new(),
                        picked: false,
                    },
                ));
                let display = target.name.clone();
                // The channel row's dest governs every member — config files narrow its mcp
                // members, folders freeze its skill members' placement (a folder entry is not
                // an unknown FILE on a channel, so no warning fires for it).
                let dest = if mcp { None } else { row.fields().dest };
                let narrowing = mcp_filter(
                    sc,
                    Some(row),
                    &display,
                    false,
                    &mut sweep.mcp_warned_dests,
                    &mut sweep.advisories,
                    &mut sweep.warnings,
                );
                let st = SyncTarget {
                    mcp_dest_filter: narrowing.filter,
                    mcp_unreachable: narrowing.unreachable,
                    target,
                    pin: None,
                    display,
                    dest,
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
                    // A kind this build cannot deliver is refused at the door, before anything
                    // is synced or cached for it.
                    if served_kind(&ds.kind, &ds.name, &sc.label, &mut sweep.warnings).is_none() {
                        return false;
                    }
                    picked.insert(ds.skill_id.as_str())
                })
                .collect();
            let total = batch.len();
            for (position, ds) in batch.into_iter().enumerate() {
                let st = SyncTarget {
                    // A feed item has no row — no dest, no narrowing: the default placement.
                    mcp_dest_filter: None,
                    mcp_unreachable: None,
                    target: CatalogTarget {
                        skill_id: ds.skill_id.clone(),
                        name: ds.name.clone(),
                        // Parsed by the batch filter above; an unknown kind never reaches here.
                        kind: BundleKind::parse(&ds.kind).unwrap_or_default(),
                        version_id: to_hex(&ds.version_id),
                        generation: ds.generation,
                        bundle_digest: Some(ds.bundle_digest),
                        review_required: ds.review_required,
                    },
                    pin: None,
                    display: ds.name.clone(),
                    dest: None,
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
                };
                let run_ctx = super::pull::ctx_with_plane_and_follow(
                    env.ctx,
                    run.transports.plane.as_plane(),
                    env.follow,
                );
                // OFFLINE MCP convergence: the store's held bytes still feed the config engine
                // (config files heal without a network), through the same store-only route the
                // online path takes — never the dir-placement planner.
                let Some(kind) = BundleKind::of_tag(ds.kind.as_deref()) else {
                    served_kind(
                        ds.kind.as_deref().unwrap_or_default(),
                        &ds.name,
                        &sc.label,
                        &mut sweep.warnings,
                    );
                    continue;
                };
                let mcp = kind.is_mcp();
                // The marker back-fills offline too: a store synced before the marker existed
                // gains it from the cache's word, so the cache row's loss stops mattering.
                crate::bundle_kind::write_kind_marker(&run_ctx, &sid, kind);
                // The ENTRIES plan: the config files this scope's surfaces put the bundle's
                // entries in. The offline arm resolves no row `dest`, so it plans the full reach
                // — the scope converge below re-plans from the same seam with whatever the row
                // narrowed to.
                let scope_root = match &sc.scope {
                    ResolvedScope::Project { dir } => Some(dir.clone()),
                    ResolvedScope::Person => None,
                };
                let plan_fn: Option<&sync_engine::PlanFn<'_>> = if mcp {
                    Some(&|ctx: &Ctx<'_>, _: &str, _: &Lock, _: &PlacementMap| {
                        crate::placement::entries_plan(ctx, scope_root.as_deref(), None)
                    })
                } else {
                    None
                };
                let mut row_index = None;
                match sync_engine::sync_one_planned(
                    &run_ctx,
                    &sid,
                    &fc,
                    Invocation::Sweep,
                    None,
                    plan_fn,
                ) {
                    Ok(mut row) => {
                        row.workspace_id = Some(run.session.workspace_id.clone());
                        row.scope = Some(sc.label.clone());
                        if row.action == PullAction::Installed {
                            row.display = Some(env.qualified(
                                &run.session.host,
                                &run.session.workspace_name,
                                &ds.name,
                            ));
                        }
                        if row.action == PullAction::DraftSynced {
                            sweep
                                .disclosures
                                .push(draft_synced_line(&ds.name, row.synced_placements));
                        }
                        row_index = Some(sweep.next_row_index());
                        sweep.push(row);
                    }
                    Err(e) => note_item_failure(
                        env.ctx,
                        &mut sweep.warnings,
                        &mut sweep.failed_bundles,
                        &ds.name,
                        &e,
                    ),
                }
                if mcp {
                    push_stored_mcp_demand(
                        env,
                        sc,
                        &run_ctx,
                        &sid,
                        &ds.name,
                        Some(&run.session.workspace_name),
                        None,
                        None,
                        row_index,
                        sweep,
                    );
                }
            }
        }
    }
}

// =================================================================================================
// The workspace-bundle sync (the one path every workspace row and feed item takes).
// =================================================================================================

/// The one target shape the delivery and the catalog both resolve to.
/// The kind a served catalog/delivery row names, or the sweep's REFUSAL for a kind this build
/// cannot deliver — a bundle published by a newer server. The row is skipped WHOLE: no store
/// sync, no placement, no cache entry, so nothing on this machine is left claiming to be a
/// bundle nobody here knows how to place. One warning line names the bundle, the kind, and the
/// way out.
fn served_kind(
    word: &str,
    bundle: &str,
    label: &str,
    warnings: &mut Vec<String>,
) -> Option<BundleKind> {
    let kind = BundleKind::parse(word);
    if kind.is_none() {
        warnings.push(format!(
            "UNKNOWN_KIND {label}: \"{bundle}\" is a \"{word}\" bundle — this topos does not \
             know how to deliver that kind; run `topos self-update`"
        ));
    }
    kind
}

struct CatalogTarget {
    skill_id: String,
    name: String,
    /// The catalog's bundle kind — an MCP target takes the STORE-ONLY sync (no dir placement)
    /// and feeds the scope's MCP demand list. Parsed at the sweep's door ([`served_kind`]), so
    /// nothing past it carries a kind this build cannot place.
    kind: BundleKind,
    version_id: String,
    generation: u64,
    bundle_digest: Option<[u8; 32]>,
    review_required: bool,
}

impl CatalogTarget {
    fn from_entry(e: &WireSkillIndexEntry, kind: BundleKind) -> Self {
        Self {
            skill_id: e.skill_id.clone(),
            name: e.name.clone(),
            kind,
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
    /// The row's `dest` — the FROZEN destination set a skill bundle's plan becomes (one target
    /// per entry, detection ignored). `None` = no dest: today's default placement, every agent.
    dest: Option<Vec<String>>,
    /// For an `"mcp"` target: the harnesses whose config files the row's `dest` names —
    /// carried onto the scope's [`mcp_engine::McpDemand`]. `None` = every MCP harness.
    mcp_dest_filter: Option<Vec<String>>,
    /// For an `"mcp"` target whose `dest` names NO config file topos can edit: why the bundle
    /// reaches no agent, so the receipt row says it instead of printing a healthy install.
    mcp_unreachable: Option<String>,
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
                note_item_failure(
                    ctx,
                    &mut sweep.warnings,
                    &mut sweep.failed_bundles,
                    &target.name,
                    &e,
                );
                return;
            }
        },
        None => ctx.layout.clone(),
    };
    let naming_slug = run.session.workspace_name.clone();
    let display = st.display.clone();
    let dest = st.dest.clone();
    // The incoming version's digest arms adopt-in-place: a by-name dir already holding a
    // byte-identical copy (a handed-over old-world placement, a teammate's committed copy) BECOMES
    // the placement instead of a namespaced sibling.
    let adopt_digest = target.bundle_digest;
    // THE MCP DIVERT: a `kind = "mcp"` bundle runs the SAME store sync — fetch + store +
    // lock.json custody, so `list`/`log`/`diff`/`update <n>@<v>` all answer — but with an EMPTY
    // placement plan: no dir placement, no baselines, no drafts, no diff3. The engine over zero
    // placements degenerates to pure store/lock/sync advancement; the bundle's bytes reach agents
    // only through the scope's config converge (`mcp_engine`), fed below.
    let mcp = target.kind.is_mcp();
    // A project root the containment rail refused is a placement that DID NOT HAPPEN — collected
    // from wherever the engine computes the plan, so the receipt says so instead of the bundle
    // quietly landing nowhere.
    let escapes: std::cell::RefCell<Vec<String>> = std::cell::RefCell::new(Vec::new());
    let mcp_reach = st.mcp_dest_filter.clone();
    let plan_fn = |ctx: &Ctx<'_>, skill_id: &str, lock: &Lock, map: &PlacementMap| {
        if mcp {
            // A config-placed bundle plans ENTRIES, not dirs: the config files its row's `dest`
            // narrowing leaves standing at this scope. The dir half of the engine sees no target
            // and the store sync degenerates to pure lock/sync advancement, exactly as before.
            return crate::placement::entries_plan(
                ctx,
                project_dir.as_deref(),
                mcp_reach.as_deref(),
            );
        }
        // The row's `name` is what the directory is called; everything else about the bundle keeps
        // its catalog identity.
        let mut named = lock.clone();
        named.name = display.clone();
        let naming = topos_harness::PlacementNaming {
            name: Some(&named.name),
            workspace_slug: Some(&naming_slug),
        };
        // A row WITH `dest` is FROZEN to exactly those destinations — one target per entry,
        // detection ignored, in BOTH scopes (project entries pass the containment rail; a
        // refused root is disclosed, never redirected).
        if let Some(dest) = &dest {
            let plan = placement::dest_plan(
                ctx,
                skill_id,
                naming,
                dest,
                project_dir.as_deref(),
                Some(map),
                adopt_digest,
            );
            escapes.borrow_mut().extend(plan.refused.iter().cloned());
            return plan;
        }
        match &project_dir {
            Some(dir) => {
                let plan =
                    placement::project_plan(ctx, dir, skill_id, naming, Some(map), adopt_digest);
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
            note_item_failure(
                ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                &target.name,
                &e,
            );
            return;
        }
    }
    // The DURABLE kind marker, laid the moment the scope store exists — for EVERY bundle, not
    // just the config-placed ones: kind classification for every later targeted verb reads this
    // and nothing else, so it must not hang on a delivery cache row a sweep can drop.
    crate::bundle_kind::write_kind_marker(&run_ctx, &sid, target.kind);
    run.transports
        .plane
        .as_delivery()
        .bind_skill(&run.session.workspace_id, &target.skill_id);
    // The dest-freeze convergence below tells GROWTH apart from what already stood: the dirs
    // that were materialized BEFORE this sync ran.
    let pre_materialized: HashSet<String> =
        doc::read_map(run_ctx.fs, &run_ctx.layout.published(&sid).map)
            .ok()
            .flatten()
            .map(|m| {
                m.placements
                    .iter()
                    .zip(&m.placement_state)
                    .filter(|(_, pst)| pst.materialized_sha.is_some())
                    .map(|(p, _)| p.clone())
                    .collect()
            })
            .unwrap_or_default();
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
    let mut row_index = None;
    match outcome {
        Ok(mut row) => {
            row.workspace_id = Some(run.session.workspace_id.clone());
            row.scope = Some(sc.label.clone());
            // An installed row leads with the workspace-QUALIFIED name — where the bundle came
            // from is the fact a first materialization discloses.
            if row.action == PullAction::Installed {
                row.display = Some(env.qualified(
                    &run.session.host,
                    &run.session.workspace_name,
                    &target.name,
                ));
            }
            // The settled-draft fan-out's receipt line is a DISCLOSURE — the fan-out succeeded, so
            // it must never land in the channel the summary counts as failures.
            if row.action == PullAction::DraftSynced {
                sweep
                    .disclosures
                    .push(draft_synced_line(&target.name, row.synced_placements));
            }
            // Disclose a delivery the naming ladder had to place BESIDE a same-named occupant the
            // record does not own (the never-clobber outcome) — and a project placement a
            // bundle's OWN root ignore file leaves visible to git. An mcp bundle placed nothing.
            // Both are ADVISORIES: the bundle DELIVERED and has its own row above, so the line is
            // an annotation on a success, not a fault to count or to exit non-zero on.
            if !mcp && let ResolvedScope::Project { .. } = sc.scope {
                disclose_namespaced(&run_ctx, &sid, &st.display, &mut sweep.advisories);
                disclose_git_visible(&run_ctx, &sid, &target.name, &mut sweep.advisories);
            }
            row_index = Some(sweep.next_row_index());
            sweep.push(row);
        }
        Err(e) => note_item_failure(
            ctx,
            &mut sweep.warnings,
            &mut sweep.failed_bundles,
            &target.name,
            &e,
        ),
    }
    // DEST-FROZEN convergence (skill rows with `dest` only): a hand-edited dest change converges
    // on this update. GROW already landed through the plan above — disclose it as the install it
    // is; SHRINK retires the recorded copies the row no longer names, through the ordinary
    // park-then-verify rail with the keep-edited-in-place discipline. The no-dest default keeps
    // the never-drop behavior untouched.
    if !mcp && row_index.is_some() && dest.is_some() {
        converge_dest_freeze(
            env,
            sc,
            &run_ctx,
            &sid,
            &display,
            &naming_slug,
            dest.as_deref().unwrap_or_default(),
            project_dir.as_deref(),
            &pre_materialized,
            run,
            row_index,
            sweep,
        );
    }
    // The demand feeds the config converge from the STORE's held bytes — whatever the sync above
    // decided, as long as a received version is on disk (an update that failed this run still
    // heals config entries from the last held version; a store with nothing yet HOLDS the
    // bundle's standing entries instead).
    if mcp {
        push_stored_mcp_demand(
            env,
            sc,
            &run_ctx,
            &sid,
            &target.name,
            Some(&run.session.workspace_name),
            st.mcp_dest_filter.clone(),
            st.mcp_unreachable.as_deref(),
            row_index,
            sweep,
        );
    }
}

/// The dest-frozen row's post-sync convergence: the GROW disclosure (a destination this run
/// first materialized reads as the install it is) and the SHRINK retire (a recorded,
/// topos-materialized copy outside the frozen target set leaves through [`retire_split`] — an
/// edited copy stays in place with a `kept` line; the adopted-in-place source is never touched).
#[allow(clippy::too_many_arguments)]
fn converge_dest_freeze(
    env: &Env<'_>,
    sc: &ScopeCtx<'_>,
    run_ctx: &Ctx<'_>,
    sid: &SkillId,
    display: &str,
    naming_slug: &str,
    dest: &[String],
    project_dir: Option<&Path>,
    pre_materialized: &HashSet<String>,
    run: &SessionRun,
    row_index: Option<usize>,
    sweep: &mut Sweep,
) {
    let guard = match crate::sidecar::lock_skill(run_ctx.fs, &run_ctx.layout, sid) {
        Ok(g) => g,
        Err(e) => {
            note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                display,
                &e,
            );
            return;
        }
    };
    let sp = run_ctx.layout.published(sid);
    let (Ok(Some(lock)), Ok(Some(map))) = (
        doc::read_doc::<Lock>(run_ctx.fs, &sp.lock),
        doc::read_map(run_ctx.fs, &sp.map),
    ) else {
        return;
    };
    // The frozen target set, recomputed by the ONE dest planner (same fn, same answer).
    let plan = placement::dest_plan(
        run_ctx,
        sid.as_str(),
        topos_harness::PlacementNaming {
            name: Some(display),
            workspace_slug: Some(naming_slug),
        },
        dest,
        project_dir,
        Some(&map),
        None,
    );
    // GROW: a dir this run first materialized is an install — say so with its destination. A row
    // the engine's own converge already flipped (a healed placement) keeps what it named; the
    // grown destinations JOIN it rather than silently replacing or being withheld.
    let grown: Vec<String> = map
        .placements
        .iter()
        .zip(&map.placement_state)
        .filter(|(p, pst)| pst.materialized_sha.is_some() && !pre_materialized.contains(*p))
        .map(|(p, _)| super::inventory::pretty(run_ctx, Path::new(p)))
        .collect();
    if !grown.is_empty()
        && let Some(row) = row_index.and_then(|i| sweep.rows.get_mut(i))
        && matches!(
            row.action,
            PullAction::UpToDate | PullAction::Installed | PullAction::Refreshed
        )
    {
        // A row the engine flipped to `refreshed` (a stale copy caught up) that ALSO grew leads
        // with the install — a folder appeared — and moves the caught-up copies to its second
        // fact, so neither set of folders is renamed or dropped.
        if row.action == PullAction::Refreshed {
            row.note = Some(sync_engine::also_line("updated", &row.destinations));
            row.destinations.clear();
        }
        row.action = PullAction::Installed;
        row.display = Some(env.qualified(&run.session.host, &run.session.workspace_name, display));
        for g in grown {
            if !row.destinations.contains(&g) {
                row.destinations.push(g);
            }
        }
    }
    // SHRINK: recorded, topos-materialized copies outside the frozen set retire; the adopted
    // source dir is the person's own and never leaves.
    let stale: Vec<usize> = map
        .placements
        .iter()
        .zip(&map.placement_state)
        .enumerate()
        .filter(|(_, (p, pst))| {
            pst.materialized_sha.is_some()
                && !pst.adopted_source
                && !plan.holds_planned_dir(Path::new(p))
        })
        .map(|(i, _)| i)
        .collect();
    if stale.is_empty() {
        return;
    }
    match retire_split(run_ctx, sid, &lock, &map, &stale) {
        Ok(Some(clean)) => {
            let mut row = plain_row(
                &clean.name,
                PullAction::Removed,
                Some(run.session.workspace_id.clone()),
                &sc.label,
            );
            row.destinations = clean.removed;
            row.kept = clean.kept;
            sweep.push(row);
        }
        Ok(None) => {}
        Err(e) => note_item_failure(
            env.ctx,
            &mut sweep.warnings,
            &mut sweep.failed_bundles,
            display,
            &e,
        ),
    }
    drop(guard);
}

/// A LOCAL path row's `dest = [...]` made operational: the adopted folder is the person's own
/// working copy (never touched); each named destination keeps a managed COPY of the row's
/// applied version, projected from the scope store — GROW lands missing copies through the
/// ordinary materialize rail (snapshot-first over anything edited), SHRINK retires copies the
/// row no longer names (park-then-verify, edited copies kept in place). A dir the scope's store
/// does not track yet contributes nothing (adopting it is `add`'s act, not the sweep's).
fn converge_local_dest(
    env: &Env<'_>,
    sc: &ScopeCtx<'_>,
    dir: &Path,
    display: &str,
    dest: &[String],
    sweep: &mut Sweep,
) {
    let store_layout = match &sc.scope {
        ResolvedScope::Project { dir } => {
            match sidecar::existing_project_store(env.ctx.fs, dir) {
                Some(l) => l,
                None => return, // nothing was ever adopted in this scope
            }
        }
        ResolvedScope::Person => env.ctx.layout.clone(),
    };
    let sctx = super::pull::ctx_with_layout(env.ctx, &store_layout);
    let Some(id) = dir
        .canonicalize()
        .ok()
        .and_then(|c| super::add::tracked_skill_at(&sctx, &c).ok().flatten())
    else {
        return;
    };
    let Ok(sid) = crate::id::SkillId::parse(&id) else {
        return;
    };
    if let Err(e) = local_dest_apply(&sctx, sc, &sid, display, dest, sweep) {
        note_item_failure(
            env.ctx,
            &mut sweep.warnings,
            &mut sweep.failed_bundles,
            display,
            &e,
        );
    }
}

fn local_dest_apply(
    ctx: &Ctx<'_>,
    sc: &ScopeCtx<'_>,
    sid: &SkillId,
    display: &str,
    dest: &[String],
    sweep: &mut Sweep,
) -> Result<(), ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
    let sp = ctx.layout.published(sid);
    let (Some(lock), Some(map), Some(sync)) = (
        doc::read_doc::<Lock>(ctx.fs, &sp.lock)?,
        doc::read_map(ctx.fs, &sp.map)?,
        doc::read_doc::<SyncState>(ctx.fs, &sp.sync)?,
    ) else {
        return Ok(());
    };
    let project_dir = match &sc.scope {
        ResolvedScope::Project { dir } => Some(dir.as_path()),
        ResolvedScope::Person => None,
    };
    let plan = placement::dest_plan(
        ctx,
        sid.as_str(),
        topos_harness::PlacementNaming {
            name: Some(display),
            workspace_slug: None,
        },
        dest,
        project_dir,
        Some(&map),
        None,
    );
    for line in &plan.refused {
        if !sweep.warnings.contains(line) {
            sweep.warnings.push(line.clone());
        }
    }
    let planned = placement::reconcile_map(&map, &plan);
    // GROW: the planned destinations whose copy is missing — the adopted source dir is NEVER a
    // managed target here, and a recorded copy that still stands (edited or not) is left to the
    // ordinary sync flows.
    let grow: Vec<usize> = placement::managed_indices(&planned, &plan)
        .into_iter()
        .filter(|&i| !planned.placement_state[i].adopted_source)
        .filter(|&i| {
            planned.placement_state[i].materialized_sha.is_none()
                || !ctx.fs.exists(Path::new(&planned.placements[i]))
        })
        .collect();
    if !grow.is_empty() {
        let base = super::parse_hex32(&lock.base_commit)?;
        let digest = super::parse_hex32(&lock.bundle_digest)?;
        let store = Store::open(&sp.store)?;
        let bundle = store.render_verified(base, digest)?;
        sync_engine::fsync_batch(ctx, &store.version_durability(&base)?)?;
        crate::materialize::materialize(
            ctx.fs,
            &crate::materialize::MaterializeReq {
                skill_id: sid.as_str(),
                target_indices: &grow,
                bundle: &bundle,
                next_map: sync_engine::next_map(&planned, base, &lock.bundle_digest),
                next_lock: &lock,
                next_sync: &sync,
                sp: &sp,
                snapshot: Some(&|scanned| {
                    sync_engine::snapshot_draft(ctx, &sp, &lock, scanned).map(|_| ())
                }),
                takeover: None,
                self_ignore: ctx.layout.is_project_scope(),
                expected: None,
                project_root: ctx.layout.project_root(),
            },
        )?;
        let dirs: Vec<String> = grow
            .iter()
            .map(|&i| super::inventory::pretty(ctx, Path::new(&planned.placements[i])))
            .collect();
        let mut row = plain_row(display, PullAction::Installed, None, &sc.label);
        row.destinations = dirs;
        sweep.push(row);
    }
    // SHRINK: recorded, topos-materialized copies outside the frozen set retire — the adopted
    // source dir is the person's own and never leaves.
    let map_now = doc::read_map(ctx.fs, &sp.map)?.unwrap_or(planned);
    let stale: Vec<usize> = map_now
        .placements
        .iter()
        .zip(&map_now.placement_state)
        .enumerate()
        .filter(|(_, (p, st))| {
            st.materialized_sha.is_some()
                && !st.adopted_source
                && !plan.holds_planned_dir(Path::new(p))
        })
        .map(|(i, _)| i)
        .collect();
    if !stale.is_empty() {
        match retire_split(ctx, sid, &lock, &map_now, &stale) {
            Ok(Some(clean)) => {
                let mut row = plain_row(&clean.name, PullAction::Removed, None, &sc.label);
                row.destinations = clean.removed;
                row.kept = clean.kept;
                sweep.push(row);
            }
            Ok(None) => {}
            Err(e) => note_item_failure(
                ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                display,
                &e,
            ),
        }
    }
    Ok(())
}

/// Feed one scope's MCP demand list from the scope store's CURRENT tree (see
/// [`crate::mcp_engine::stored_server_json`]). A store holding no received version yet — or one
/// this build cannot read — HOLDS the bundle instead: its standing config entries must not read
/// as undemanded while the bytes are unknowable.
#[allow(clippy::too_many_arguments)]
fn push_stored_mcp_demand(
    env: &Env<'_>,
    sc: &ScopeCtx<'_>,
    run_ctx: &Ctx<'_>,
    sid: &SkillId,
    name: &str,
    workspace_slug: Option<&str>,
    reach: Option<Vec<String>>,
    unreachable: Option<&str>,
    row_index: Option<usize>,
    sweep: &mut Sweep,
) {
    match crate::mcp_engine::stored_server_json(run_ctx, sid) {
        Ok(Some((version_id, server_json))) => {
            sweep.note_mcp_row(&sc.label, sid.as_str(), row_index, unreachable);
            sweep.mcp_demands.entry(sc.label.clone()).or_default().push(
                crate::mcp_engine::DemandedBundle {
                    bundle_id: sid.as_str().to_owned(),
                    name: name.to_owned(),
                    workspace_slug: workspace_slug.map(str::to_owned),
                    version_id,
                    server_json,
                    reach,
                },
            );
        }
        Ok(None) => {
            sweep
                .mcp_hold
                .entry(sc.label.clone())
                .or_default()
                .insert(sid.as_str().to_owned());
        }
        Err(e) => {
            note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                name,
                &e,
            );
            sweep
                .mcp_hold
                .entry(sc.label.clone())
                .or_default()
                .insert(sid.as_str().to_owned());
        }
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
        // The rewrite LANDED — an advisory about a row that delivered, never a failure: a run
        // that finished someone's pending transfer must not report itself as broken, and must not
        // hand an agent a non-zero status for it.
        Ok(super::GovernedOutcome::Rewritten(rw)) => {
            sweep.advisories.push(format!(
                "GOVERNANCE_CONVERGED {}: {} — the \"{}\" line is now \"{}\" (a landed publish's \
                 pending transfer)",
                lock.name, rw.manifest, rw.from, rw.canonical
            ));
            true
        }
        // The row was removed while this converge ran — a completed removal is never re-added.
        Ok(super::GovernedOutcome::RowRemoved { .. } | super::GovernedOutcome::None) => false,
        Err(e) => {
            note_item_failure(
                ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                &lock.name,
                &e,
            );
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
        "1 other folder".to_owned()
    } else {
        format!("{n} other folders")
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
// The forge arms — a `topos.toml` row pointing at a repository is kept current like everything
// else: the sweep checks it on the forge lane's own clock and a change lands silently. The row IS
// the demand and the consent alike, the same way a dependency listed in a project's package file is
// fetched by the ordinary install; the two-phase describe lives in `add`, where a person is present
// to read it, not in the automatic converge.
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

/// A whole repo: every skill it holds. `"*"` tracks the repo's default branch — PROBED first
/// (which commit does it point at now?) and downloaded only when that answer differs from what is
/// recorded, so a repo that has not moved costs one small request instead of an archive. A pinned
/// row NEVER moves: a tracked import whose recorded commit prefix-matches the pin is up to date, and
/// a pin that MOVED re-imports at the new pin. Tracked members ABSENT from the freshly-fetched
/// archive get the ordinary undemanded cleaning (snapshot-first) in the same update that rendered
/// them `-member`.
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
    // THE ROW IS DEMAND, and it is demand BEFORE anything below can return early: the members
    // this scope already tracks for the origin are mentioned first, so the undemanded clean can
    // never read a failed check as a drop. A run that installs nothing must destroy nothing
    // either — the same reason a transient fetch failure never retires a tracked member (the
    // mention is what protects both).
    let tracked = tracked_repo_members(&sctx, &origin);
    for import in &tracked {
        sweep.mention(&sc.label, &import.lock.name);
    }
    let pin = row.pin();
    // The row's own placement decision (`dest = [...]`, written by a `-a` selector import): one
    // converge SLOT per named destination dir, so a refresh keeps each copy where it was asked
    // for instead of re-landing it in the default dir. No field = one default slot = the old
    // behavior.
    let row_dest = row.fields().dest.unwrap_or_default();
    let roots = discovery_roots(env.ctx, &sc.scope);
    let global = matches!(sc.scope, ResolvedScope::Person);
    let slots_for = |name: &str| -> Vec<DestSlot<'_>> {
        dest_slots(&sctx, roots.as_ref(), global, &row_dest, &tracked, name)
    };
    let is_tracked_name = |name: &str| slots_for(name).iter().all(|s| s.import.is_some());
    // Tracked, AT a given commit, and its BYTES STILL ON DISK — the one predicate every
    // "nothing to do here" decision in this arm is allowed to rest on.
    //
    // Presence alone is not convergence: a refresh the engine refused (local edits, a busy lock)
    // leaves that member tracked at the OLD commit while its siblings move on. And a record alone
    // is not bytes: the sidecar survives a wiped agent directory — an ordinary harness reinstall —
    // so an import can claim a commit for a copy that is no longer anywhere.
    let tracked_at = |name: &str, at: &str| {
        let slots = slots_for(name);
        !slots.is_empty()
            && slots.iter().all(|s| {
                s.import.is_some_and(|i| {
                    commit_matches(i.origin.commit.as_deref().unwrap_or_default(), at)
                        && doc::read_map(sctx.fs, &sctx.layout.published(&i.sid).map)
                            .ok()
                            .flatten()
                            .is_some_and(|m| {
                                m.placements.iter().any(|pl| sctx.fs.exists(Path::new(pl)))
                            })
                })
            })
    };
    // EVERY member the last landing recorded, each really converged at `at`.
    //
    // Every member, not the first one: `forge_imports` sorts by name, so a "the recorded commit
    // matches" test reads the ALPHABETICALLY FIRST member's commit and calls the whole row settled
    // on it. A repo holding `alpha` and `beta` where only `beta`'s refresh was refused then looks
    // converged the moment `alpha` moves — and `beta` sits a commit behind, reporting up to date,
    // with no event left to heal it.
    let recorded_all_at = |at: &str| {
        !tracked.is_empty()
            && recorded_member_set(&tracked)
                .is_some_and(|members| members.iter().all(|m| tracked_at(m, at)))
    };

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
    // absent record is UNSETTLED: the next update refetches ONCE, records the archive's member
    // list (below), and converges — after which the ordinary predicate answers.
    let members_complete = recorded_member_set(&tracked)
        .is_some_and(|recorded| recorded.iter().all(|m| is_tracked_name(m)));
    let pin_satisfied = pin.as_ref().is_some_and(|p| {
        !tracked.is_empty()
            && members_complete
            && tracked
                .iter()
                .all(|i| commit_matches(i.origin.commit.as_deref().unwrap_or_default(), p))
    });

    let converge_in_place = |sweep: &mut Sweep, targets: &mut Targets| {
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
    };
    let Some(lane) = env.forge.filter(|_| !pin_satisfied) else {
        // Not dialing this round (the clock says wait), or settled: converge in place.
        converge_in_place(sweep, targets);
        return;
    };
    let git_ref = pin.clone().unwrap_or_default();
    if let Some(hold) = lane.hold(&origin, &git_ref, &sc.label) {
        forge_hold_line(sc, &row.reference, &hold, sweep);
        converge_in_place(sweep, targets);
        return;
    }

    let spec = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref: pin.clone(),
        subdir: None,
    };
    // THE CHEAP CHECK. A floating row asks what the default branch points at now; if that is what
    // is already installed and the recorded member list is fully landed, the round is over for
    // this source without a byte of archive moving. A PINNED row skips the probe: it is either
    // settled above (and never dials) or must fetch the pinned bytes regardless.
    if pin.is_none() {
        let head = match lane.probe(&origin, &git_ref, &spec) {
            Ok(h) => h,
            Err(e) => {
                // Ending here is the point: falling through to the archive would pay a second
                // round-trip for an answer the first one already failed to get.
                note_item_failure(
                    env.ctx,
                    &mut sweep.warnings,
                    &mut sweep.failed_bundles,
                    &row.reference,
                    &e,
                );
                converge_in_place(sweep, targets);
                return;
            }
        };
        renamed_line(&origin, &head, sweep);
        let unchanged = recorded_all_at(&head.commit)
            // …or this scope already finished with this exact head AND its store still proves it.
            // The mark answers the one case nothing installed can — a repository holding no skills
            // at all — but it is machine state vouching for a checkout's bytes, so it is never
            // taken on trust.
            || lane.settled_here(&origin, &git_ref, &sc.label, &head.commit, tracked_at);
        if unchanged {
            converge_in_place(sweep, targets);
            return;
        }
    }
    let targz = match lane.fetch(&origin, &git_ref, &spec) {
        Ok(t) => t,
        Err(e) => {
            note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                &row.reference,
                &e,
            );
            converge_in_place(sweep, targets);
            return;
        }
    };
    let tree = match crate::git_source::extract_tree(&targz) {
        Ok(t) => t,
        Err(e) => {
            // An archive that arrived and would not decode is a FAILED check, not a passed one:
            // the request succeeded, the answer did not.
            lane.note_fault(&origin, &git_ref, &e);
            note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                &row.reference,
                &e,
            );
            converge_in_place(sweep, targets);
            return;
        }
    };
    let resolved = tree.commit.clone().unwrap_or_default();
    lane.note_landed(&origin, &git_ref, &resolved);
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
    // Every discovered member really sits on the fetched commit: settled. (An UNTRACKED member at
    // the same commit — a partial add landing — still installs below.)
    //
    // A PINNED row reaches this every due round, because a pin nothing satisfies never settles
    // above. Testing presence here rather than the commit meant a member whose refresh was refused
    // was skipped before the install loop could try again — and a pin never moves, so unlike the
    // floating row there was no later push to heal it: a permanent silent non-update, re-fetching
    // the same archive every interval to skip the same member.
    //
    // "Nothing to do" also means nothing to UNDO: a member the archive no longer holds has to be
    // retired by the loop below, so a row whose repository dropped something is never settled here.
    // (The old commit comparison guarded that by accident; a repository that emptied out entirely
    // would otherwise short-circuit past its own cleanup.)
    let nothing_to_retire = tracked.iter().all(|i| {
        discovered
            .iter()
            .any(|d| i.lock.name == *d || subdir_leaf(&i.origin).as_deref() == Some(d))
    });
    if !resolved.is_empty()
        && nothing_to_retire
        && discovered.iter().all(|d| tracked_at(d, &resolved))
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
            Err(e) => note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                &import.lock.name,
                &e,
            ),
        }
    }
    let decisions_before = sweep.decisions.len();
    for name in &discovered {
        if !set_selected && !targets.hit(&[name.as_str()]) {
            continue;
        }
        let slots = slots_for(name);
        // EVERY slot's copy already at the fetched commit AND still on disk is settled — only the
        // unfilled, moved, or vanished ones go through the install/refresh below. The same rule as
        // every other "nothing to do" test in this arm, for the same reason: a record whose bytes
        // were wiped out from under it is not a copy.
        if !resolved.is_empty() && tracked_at(name, &resolved) {
            let landed = slots
                .first()
                .and_then(|s| s.import)
                .map_or_else(|| name.clone(), |i| i.lock.name.clone());
            sweep.push(plain_row(&landed, PullAction::UpToDate, None, &sc.label));
            continue;
        }
        install_or_refresh_repo_skill(env, sc, &sctx, &spec, &targz, name, &slots, sweep);
    }
    // The commit motion this update landed — a DISCLOSURE of what worked, beside the rows that
    // carry the members. It must not ride the channel the summary counts as failures, and it is
    // said only NOW, once every member has had its turn: a member that stood down for a DECISION
    // did not move, so the line does not list it as one that did. With every member standing down
    // there is nothing left for this line to add — the decision rows already say, in words a
    // person can act on, that the source has a newer version.
    let blocked: Vec<String> = sweep.decisions[decisions_before..]
        .iter()
        .map(|d| d.name.clone())
        .collect();
    let all_blocked = !discovered.is_empty() && discovered.iter().all(|d| blocked.contains(d));
    if !recorded.is_empty()
        && !resolved.is_empty()
        && !commit_matches(&recorded, &resolved)
        && !all_blocked
    {
        sweep.disclosures.push(git_updated_line(
            &origin,
            &recorded,
            &resolved,
            &tracked,
            &discovered,
            &blocked,
        ));
    }
    // The pass finished. CONVERGENCE IS RE-READ FROM THE STORE, not inferred from the plan: the
    // slot view above was captured before any of the installs ran, and it only ever asked whether
    // a member was tracked at all. A refresh the engine REFUSED — a member carrying local edits, a
    // lock it could not take — leaves that member tracked at the OLD commit and would otherwise
    // pass, writing a settlement mark over work that never happened and suppressing the retry
    // until upstream moved again. So the mark is written only when every discovered member really
    // is at the resolved head, which is also true, vacuously and correctly, of a head holding none.
    let converged = {
        let landed = tracked_repo_members(&sctx, &origin);
        let at_head = |name: &str| {
            let slots = dest_slots(&sctx, roots.as_ref(), global, &row_dest, &landed, name);
            !slots.is_empty()
                && slots.iter().all(|s| {
                    s.import.is_some_and(|i| {
                        commit_matches(i.origin.commit.as_deref().unwrap_or_default(), &resolved)
                    })
                })
        };
        discovered.iter().all(|d| at_head(d))
    };
    if converged {
        lane.note_settled(&origin, &git_ref, &sc.label, &resolved, &discovered);
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
    let members = tracked_repo_members(&sctx, &origin);
    // Same placement decision as the set arm: the row's `dest` list is one converge slot per
    // destination dir (no field = the one default slot).
    let row_dest = fields.dest.clone().unwrap_or_default();
    let roots = discovery_roots(env.ctx, &sc.scope);
    let global = matches!(sc.scope, ResolvedScope::Person);
    let slots = dest_slots(&sctx, roots.as_ref(), global, &row_dest, &members, skill);
    let tracked = slots.first().and_then(|s| s.import);
    let pin = row.pin();
    let pin_satisfied = pin.as_ref().is_some_and(|p| {
        slots.iter().all(|s| {
            s.import
                .is_some_and(|i| commit_matches(i.origin.commit.as_deref().unwrap_or_default(), p))
        })
    });
    let converge_in_place = |sweep: &mut Sweep| match &tracked {
        Some(import) => sweep.push(plain_row(
            &import.lock.name,
            PullAction::UpToDate,
            None,
            &sc.label,
        )),
        // Nothing to converge: this member has never been fetched here.
        None => sweep.warnings.push(format!(
            "NOT_INSTALLED {}: \"{}\" — an external skill this machine has not fetched yet \
             (network required)",
            sc.label, row.reference
        )),
    };
    let Some(lane) = env.forge.filter(|_| !pin_satisfied) else {
        converge_in_place(sweep);
        return;
    };
    let git_ref = pin.clone().unwrap_or_default();
    if let Some(hold) = lane.hold(&origin, &git_ref, &sc.label) {
        forge_hold_line(sc, &row.reference, &hold, sweep);
        // Only a TRACKED copy has something to converge. The not-installed-yet line would
        // otherwise repeat the fault another row just reported — or, behind a settled verdict,
        // reappear every single round about a row nothing is going to fetch.
        if tracked.is_some() {
            converge_in_place(sweep);
        }
        return;
    }
    let spec = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref: pin.clone(),
        // A literal in-repo path is the escape hatch a row spells explicitly; without one the leaf
        // NAME selects the skill.
        subdir: fields.subdir.clone(),
    };
    // THE CHEAP CHECK, exactly as in the set arm: a floating row that already holds the commit the
    // default branch points at is settled without downloading anything.
    if pin.is_none() {
        let head = match lane.probe(&origin, &git_ref, &spec) {
            Ok(h) => h,
            Err(e) => {
                note_item_failure(
                    env.ctx,
                    &mut sweep.warnings,
                    &mut sweep.failed_bundles,
                    &row.reference,
                    &e,
                );
                // Only a TRACKED copy has something to converge; the not-installed-yet line would
                // be a second, weaker sentence about the failure just reported.
                if tracked.is_some() {
                    converge_in_place(sweep);
                }
                return;
            }
        };
        renamed_line(&origin, &head, sweep);
        let settled = !slots.is_empty()
            && slots.iter().all(|s| {
                s.import.is_some_and(|i| {
                    commit_matches(i.origin.commit.as_deref().unwrap_or_default(), &head.commit)
                })
            });
        if settled && let Some(import) = &tracked {
            sweep.push(plain_row(
                &import.lock.name,
                PullAction::UpToDate,
                None,
                &sc.label,
            ));
            return;
        }
    }
    let targz = match lane.fetch(&origin, &git_ref, &spec) {
        Ok(t) => t,
        Err(e) => {
            note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                &row.reference,
                &e,
            );
            if tracked.is_some() {
                converge_in_place(sweep);
            }
            return;
        }
    };
    // The archive has to decode before any of it counts — including as a CHECK. A 200 carrying an
    // unreadable body is a request that succeeded and an answer that did not.
    let tree = match crate::git_source::extract_tree(&targz) {
        Ok(t) => t,
        Err(e) => {
            lane.note_fault(&origin, &git_ref, &e);
            note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                &row.reference,
                &e,
            );
            if tracked.is_some() {
                converge_in_place(sweep);
            }
            return;
        }
    };
    lane.note_landed(&origin, &git_ref, &tree.commit.clone().unwrap_or_default());
    // The source's motion, said only once the re-import has had its turn (see below).
    let mut motion: Option<String> = None;
    // Every slot's copy at the same commit is settled: nothing moves without a real change.
    if let Some(import) = &tracked {
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
            // failure (see the set arm above). HELD until the re-import has run: if the copy
            // here carries edits, the decision row below says the source moved in words a person
            // can act on, and this line would repeat it in the engine's.
            motion = Some(format!(
                "GIT_UPDATED {origin}: {} → {}; skills: ~{}",
                short_commit(&recorded),
                short_commit(&resolved),
                import.lock.name
            ));
        }
    }
    let before = sweep.decisions.len();
    install_or_refresh_repo_skill(env, sc, &sctx, &spec, &targz, skill, &slots, sweep);
    if let Some(line) = motion.filter(|_| sweep.decisions.len() == before) {
        sweep.disclosures.push(line);
    }
}

/// ONE converge slot of a forge row's member: the destination root the row aimed this copy at
/// (`None` = the row named none, so the default agent dir answers) and the tracked import
/// already sitting there.
struct DestSlot<'a> {
    root: Option<PathBuf>,
    import: Option<&'a ForgeImport>,
}

/// The slots one member of a forge row converges through — the row's `dest` field made
/// operational.
///
/// A `-a` selector import wrote that field precisely so the copy would keep living in the dir
/// it was aimed at; without reading it back, the refresh below re-lands the copy through the
/// DEFAULT root and the selection silently evaporates on the first commit move. One slot per
/// dest entry (its resolved root, paired with the import whose recorded placements sit under
/// it), and with a single slot the by-name import answers even when its placement predates the
/// field — the row says where that copy belongs, and the refresh is what takes it there.
///
/// No field at all → exactly one default slot holding the by-name import: today's behavior,
/// unchanged.
fn dest_slots<'a>(
    sctx: &Ctx<'_>,
    roots: Option<&super::DiscoveryRoots>,
    global: bool,
    dest: &[String],
    tracked: &'a [ForgeImport],
    name: &str,
) -> Vec<DestSlot<'a>> {
    let candidates: Vec<&ForgeImport> = tracked
        .iter()
        .filter(|i| i.lock.name == name || subdir_leaf(&i.origin).as_deref() == Some(name))
        .collect();
    let (Some(roots), false) = (roots, dest.is_empty()) else {
        return vec![DestSlot {
            root: None,
            import: candidates.first().copied(),
        }];
    };
    let single = dest.len() == 1;
    dest.iter()
        .map(|entry| {
            let root = resolve_dest_root(entry, roots, global);
            let placed = root.as_ref().and_then(|root| {
                candidates
                    .iter()
                    .copied()
                    .find(|i| placed_under(sctx, i, root))
            });
            DestSlot {
                root,
                import: placed.or_else(|| single.then(|| candidates.first().copied()).flatten()),
            }
        })
        .collect()
}

/// Resolve one `dest` entry to its root dir: `~/` against the machine home and absolute paths
/// verbatim in the global file; a project entry against the project dir (the discovery cwd —
/// containment was proven at load, and the import's write-boundary belt re-proves it).
fn resolve_dest_root(entry: &str, roots: &super::DiscoveryRoots, global: bool) -> Option<PathBuf> {
    if let Some(rest) = entry.strip_prefix("~/") {
        return Some(roots.home.join(rest));
    }
    if Path::new(entry).is_absolute() {
        return Some(PathBuf::from(entry));
    }
    if global {
        // A relative entry cannot resolve in the machine file (the grammar refuses it at load;
        // an older record meeting this stays unplanned rather than guessed at).
        return None;
    }
    roots
        .cwd
        .as_deref()
        .map(|cwd| cwd.join(entry.trim_start_matches("./")))
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
    slots: &[DestSlot<'_>],
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
        note_item_failure(
            env.ctx,
            &mut sweep.warnings,
            &mut sweep.failed_bundles,
            name,
            &e,
        );
        return;
    }
    let mut landed: Option<String> = None;
    let mut blocked = false;
    for slot in slots {
        let opts = super::AddRemoteOpts {
            // A row that spells a literal `subdir` has already narrowed the archive; otherwise the
            // leaf name picks the skill out of a multi-skill repo.
            skill: spec.subdir.is_none().then(|| name.to_owned()),
            harness: None,
            // The row's own destination root for THIS copy (`None` = the default agent dir).
            dest_root: slot.root.clone(),
            global: matches!(sc.scope, ResolvedScope::Person),
        };
        let outcome = match slot.import {
            Some(import) => refresh_repo_skill(sctx, targz, spec, &opts, &roots, &import.sid),
            None => super::add_remote_fetched(sctx, targz, spec, &roots, &opts)
                .map(|d| RefreshOutcome::Landed(d.name)),
        };
        match outcome {
            // One receipt row per MEMBER, whatever the slot count — the person asked for a skill,
            // not for a copy per agent.
            Ok(RefreshOutcome::Landed(name)) => landed = landed.or(Some(name)),
            // The source moved and this copy carries edits: nothing was overwritten, and the
            // choice is the person's. It is stated ONCE, as a decision row — a second slot
            // blocked on the same edits is the same unanswered question, not a second one.
            Ok(RefreshOutcome::BlockedByEdits) => blocked = true,
            Err(e) => note_item_failure(
                env.ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                name,
                &e,
            ),
        }
    }
    if let Some(name) = landed {
        sweep.push(plain_row(&name, PullAction::FastForwarded, None, &sc.label));
    } else if blocked {
        sweep
            .decisions
            .push(import_blocked_decision(name, &spec.origin(), &sc.scope));
    }
}

/// The decision a moved source hands back when the copy here carries edits: what changed, what
/// stands in the way, and the one runnable answer — drop them.
///
/// Both facts belong in the first sentence: the SOURCE moved (which is why anything is happening
/// at all) and the EDITS are why nothing did. Keeping them needs no command at all — nothing was
/// overwritten, so a person who wants the work simply leaves it where it is; the row's job is to
/// say that the source moved and to hand over the one command that is not obvious.
///
/// A `publish` offer used to ride here too, and it was a command this reader could not run: a
/// skill that came from a repository is the one kind that reaches a person with no workspace at
/// all, and `publish` refuses them at the login step. A way out that dead-ends is worse than no
/// way out, so the row names only what works.
fn import_blocked_decision(
    name: &str,
    origin: &str,
    scope: &ResolvedScope,
) -> super::PendingDecision {
    let argv =
        |tokens: &[&str]| -> Vec<String> { tokens.iter().map(|t| (*t).to_owned()).collect() };
    let reset = match scope {
        ResolvedScope::Person => argv(&["topos", "update", "-g", name, "--reset"]),
        ResolvedScope::Project { .. } => argv(&["topos", "update", name, "--reset"]),
    };
    let ways = vec![("to discard them:", reset)];
    let pad = ways.iter().map(|(label, _)| label.len()).max().unwrap_or(0);
    super::PendingDecision {
        name: name.to_owned(),
        line: format!("{origin} has a newer version, but your edits would be overwritten"),
        detail: ways
            .iter()
            .map(|(label, cmd)| format!("{label:<pad$}   {}", cmd.join(" ")))
            .collect(),
        ways_out: ways.into_iter().map(|(_, cmd)| cmd).collect(),
    }
}

/// What a re-import of a tracked external skill concluded.
enum RefreshOutcome {
    /// The re-import landed — the bundle's tracked name.
    Landed(String),
    /// Local edits stand in the way, so nothing was touched. Not an error: the source moved, the
    /// person's work is intact, and the only thing missing is their answer about which wins.
    BlockedByEdits,
}

/// Re-import a tracked external skill at a NEW commit: local edits stand the refresh down (never
/// overwritten by an import), a clean copy is snapshot-verified, the sidecar record replaced
/// wholesale, and the fresh import lands through the ordinary adopt.
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
) -> Result<RefreshOutcome, ClientError> {
    let _guard = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
    let sp = ctx.layout.published(sid);
    let map: PlacementMap = sync_engine::read_map_required(ctx, &sp)?;
    // Read for its own sake: a record with no readable lock is corrupt, and this refresh is about
    // to park and replace directories — it stops here rather than on a half-moved tree.
    let _lock: Lock = doc::read_doc::<Lock>(ctx.fs, &sp.lock)?
        .ok_or_else(|| ClientError::Corrupt(format!("{}: lock.json missing", sid.as_str())))?;
    let scans = placement::scan_placements(ctx, &map)?;
    if scans
        .iter()
        .any(|s| matches!(s.status, placement::ScanStatus::Modified { .. }))
    {
        return Ok(RefreshOutcome::BlockedByEdits);
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
    let mut stash_all = || -> Result<bool, ClientError> {
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
            // treat a difference exactly as an up-front local edit: stand down, restoring every
            // stash. An import never overwrites work it did not put there, whenever it arrived.
            let still_clean =
                crate::scan::scan(&parked).is_ok_and(|fresh| fresh.bundle_digest == *digest);
            if !still_clean {
                return Ok(false);
            }
        }
        let sidecar_dir = ctx.layout.skill_dir(sid);
        if ctx.fs.exists(&sidecar_dir) {
            // The sidecar record: topos's own engine state, mutated only under the skill lock
            // this fn holds — no content digest to re-prove (`None`).
            stash_dir(ctx.fs, &sidecar_dir, None, &mut stashed)?;
        }
        Ok(true)
    };
    // A stash failure MID-LOOP restores what was already moved — the old import stays coherent,
    // and so does an edit that arrived mid-refresh: that is a decision, not a fault.
    match stash_all() {
        Ok(true) => {}
        Ok(false) => {
            restore(ctx.fs, &stashed);
            return Ok(RefreshOutcome::BlockedByEdits);
        }
        Err(e) => {
            restore(ctx.fs, &stashed);
            return Err(e);
        }
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
            Ok(RefreshOutcome::Landed(d.name))
        }
        Err(e) => {
            restore(ctx.fs, &stashed);
            Err(e)
        }
    }
}

/// The line a source the lane declined to dial earns.
///
/// A GONE source is named ONCE and then left alone: the reference is what has to change, and
/// repeating the same sentence at every session start would train a person to stop reading them.
/// The breaker's short-circuit says nothing at all — the source that actually failed already did,
/// and "we skipped this because something else broke" is noise about someone else's problem.
fn forge_hold_line(sc: &ScopeCtx<'_>, reference: &str, hold: &ForgeHold, sweep: &mut Sweep) {
    if let ForgeHold::Gone { reason, first_time } = hold
        && *first_time
    {
        sweep.warnings.push(format!(
            "REMOTE_FETCH {}: \"{reference}\" — {reason}; the copies here still work, and \
             `topos update` retries once the row names something that resolves",
            sc.label
        ));
    }
}

/// The DISCLOSURE a followed rename earns: the row still works, because the forge redirected and
/// the check followed it. A rename is never a failure, so this never rides the channel the receipt
/// counts as one.
///
/// It deliberately stops at the FACT and does not advise respelling the row. The tracked copies
/// record the origin they were imported under, so a row edited to the new name would find nothing
/// tracked, try a fresh install, and collide with the copy already sitting there — an update that
/// stops updating, caused by following our own advice. Recognizing the old provenance under a new
/// spelling is what would make that advice safe, and until it exists the honest thing to say is
/// that nothing needs doing.
fn renamed_line(origin: &str, head: &RepoHead, sweep: &mut Sweep) {
    if let Some((owner, repo)) = &head.renamed_to {
        let host = origin.split('/').next().unwrap_or_default();
        sweep.disclosures.push(format!(
            "GIT_RENAMED {origin}: the forge now serves this as {host}/{owner}/{repo} and \
             redirects to it — the row keeps working as written"
        ));
    }
}

/// The ONE receipt line a moved git source earns: what it was, what it is, and which members that
/// moved — a source that silently swaps bytes under an agent is exactly what this line prevents.
///
/// `blocked` names the members that stood down for a DECISION. They are dropped from BOTH readings:
/// nothing of theirs moved, so they are neither a member that changed nor — the mistake worth
/// guarding — a member that left.
fn git_updated_line(
    origin: &str,
    old: &str,
    new: &str,
    tracked: &[ForgeImport],
    discovered: &[String],
    blocked: &[String],
) -> String {
    let had: Vec<&str> = tracked
        .iter()
        .map(|i| i.lock.name.as_str())
        .filter(|n| !blocked.iter().any(|b| b == n))
        .collect();
    let mut parts: Vec<String> = Vec::new();
    for name in discovered.iter().filter(|d| !blocked.contains(d)) {
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
            // A RETIRED record holds nothing the machine still reports: its kept files are the
            // person's own now, so its cache rows drop out exactly like a gone placement's.
            if sidecar::record_retired(ctx.fs, layout, &sid) {
                continue;
            }
            let Ok(Some(map)) = doc::read_map(ctx.fs, &layout.published(&sid).map) else {
                continue;
            };
            // A CONFIG-PLACED (mcp) record holds no dirs — its "still held" proof is the
            // config-entry half of its record (the `entries.json` beside the map).
            let held = if map.placements.is_empty() {
                !crate::config_custody::entries_of(ctx.fs, layout, id).is_empty()
            } else {
                map.placements.iter().any(|p| ctx.fs.exists(Path::new(p)))
            };
            if held {
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
///
/// Both are ADVISORIES. They read as warnings and are meant to, but neither describes a fault:
/// a recipe that adopts less than the workspace assigns is a choice someone made, and a declined
/// bundle a local row still delivers is the machine's row winning exactly as designed. Counting
/// either as `failed` — let alone exiting non-zero on it — would call a working machine broken.
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
            sweep.advisories.push(format!(
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
                sweep.advisories.push(format!(
                    "DECLINED_OVERRIDE {name}: declined on the web, delivered here by your manifest"
                ));
            }
        }
    }
}

// =================================================================================================
// MCP convergence (the config-placement half of the sweep).
// =================================================================================================

/// Run [`crate::mcp_engine::converge`] once per DRIVEN, RESOLVED scope, over the demands the
/// fan-out collected, and fold the outcomes back into the sweep: per-agent states onto the
/// receipt rows, removal/drift disclosures, and the merged `skill_id → states` map the applied
/// report and the delivery cache read (person scope wins per slug — the same store the applied
/// pick prefers).
///
/// FREEZE DISCIPLINE mirrors the cleaner's: removals run only on a full (untargeted) sweep of a
/// scope whose manifest RESOLVED, with a readable delivery cache; and every bundle whose demand
/// state is unknowable this run — an unreached workspace's cached deliveries, a failed channel's
/// members, a mentioned-but-unresolved row — is HELD in place, unreported.
fn run_mcp_converge(
    env: &Env<'_>,
    driven: &Driven,
    person: Option<&ScopePlan>,
    project: Option<&(PathBuf, ScopePlan)>,
    project_frozen: bool,
    opts: &ManifestUpdateOpts,
    sweep: &mut Sweep,
) -> HashMap<String, Vec<topos_types::results::McpAgentState>> {
    let mut merged: HashMap<String, Vec<topos_types::results::McpAgentState>> = HashMap::new();
    let Some(roots) = env.ctx.roots.clone() else {
        return merged; // no machine roots: no config surface is resolvable
    };
    // The MACHINE-LEVEL holds every scope shares: cached deliveries of workspaces that produced
    // no fresh snapshot this run (unreached, pending, ended, or logged out), and members of a
    // channel whose expansion failed.
    let mut machine_hold: HashSet<String> = HashSet::new();
    for (ws_id, entry) in &env.prior.workspaces {
        let fresh = env
            .runs
            .iter()
            .any(|r| &r.session.workspace_id == ws_id && r.snapshot.is_some());
        for (skill_id, ds) in &entry.delivered {
            if !fresh
                || ds
                    .via_channels
                    .iter()
                    .any(|c| sweep.failed_channels.contains(&(ws_id.clone(), c.clone())))
            {
                machine_hold.insert(skill_id.clone());
            }
        }
    }

    // Person first (its states win the per-slug merge), then the project fills gaps.
    let mut scopes_to_run: Vec<(String, sidecar::Layout, Option<PathBuf>, bool)> = Vec::new();
    if driven.person && person.is_some() {
        scopes_to_run.push((
            ResolvedScope::Person.label(),
            env.ctx.layout.clone(),
            None,
            true,
        ));
    }
    if driven.project
        && !project_frozen
        && let Some((dir, _)) = project
    {
        let label = ResolvedScope::Project { dir: dir.clone() }.label();
        let has_demands = sweep.mcp_demands.get(&label).is_some_and(|d| !d.is_empty());
        // Demands mint the store through the ordinary shell (self-ignoring `.topos/`); a
        // removal-only converge visits an EXISTING store and never mints one.
        let layout = if has_demands {
            match sidecar::ensure_project_store(env.ctx.fs, dir) {
                Ok(l) => Some(l),
                Err(e) => {
                    sweep
                        .warnings
                        .push(format!("MCP_STORE_FAILED {}: {}", label, e.detail()));
                    None
                }
            }
        } else {
            sidecar::existing_project_store(env.ctx.fs, dir)
        };
        if let Some(layout) = layout {
            scopes_to_run.push((label, layout, Some(dir.clone()), false));
        }
    }

    for (label, layout, project_root, person_scope) in scopes_to_run {
        let rows = sweep.mcp_demands.remove(&label).unwrap_or_default();
        // The common non-mcp machine: no demands and no custody document — nothing to converge,
        // read.
        if rows.is_empty() && !env.ctx.fs.exists(&layout.config_custody_path()) {
            continue;
        }

        let mut hold = machine_hold.clone();
        hold.extend(sweep.mcp_hold.remove(&label).unwrap_or_default());
        // A name a row still MENTIONS whose demand did not land this run (a catalog fetch
        // failure, a refused resolution) must hold its entries too — matched through the cache's
        // name → id rows.
        if let Some(mentioned) = sweep.mentioned.get(&label) {
            let demanded: HashSet<&str> = rows.iter().map(|d| d.bundle_id.as_str()).collect();
            for entry in env.prior.workspaces.values() {
                for (skill_id, ds) in &entry.delivered {
                    if mentioned.contains(&ds.name) && !demanded.contains(skill_id.as_str()) {
                        hold.insert(skill_id.clone());
                    }
                }
            }
        }
        let allow_removals = opts.targets.is_empty() && !sweep.mcp_blind;
        let cwd = project_root.clone().or_else(|| roots.cwd.clone());
        let detected: std::collections::BTreeSet<String> =
            topos_harness::registry::detected_harnesses(&roots.home, cwd.as_deref())
                .iter()
                .map(|h| h.slug.to_owned())
                .collect();
        let io = crate::mcp_engine::ScopeIo {
            fs: env.ctx.fs,
            layout: &layout,
            home: roots.home.clone(),
            project_root: project_root.clone(),
        };
        let descriptors = topos_harness::mcp::descriptor::mcp_harnesses();
        // THE ONE PLANNING CALL for this scope: every resolved row's reach becomes the config
        // files its entries belong in. The converge past this point reads demand from these
        // plans and custody from each bundle's own record — it decides no reach of its own.
        let demands: Vec<crate::mcp_engine::McpDemand> = rows
            .into_iter()
            .map(|row| row.planned(&io, &descriptors, &detected))
            .collect();
        let outcome = crate::mcp_engine::converge(
            &io,
            &demands,
            &descriptors,
            &detected,
            &hold,
            allow_removals,
        );
        sweep.warnings.extend(outcome.warnings);
        // A bundle the converge could not place is a FAILED BUNDLE, not merely a warning line —
        // the summary counts bundles, and a gate refusal that rode only the line channel printed
        // "1 already up to date" on a run that exited non-zero.
        //
        // ONE BUNDLE, ONE BUCKET. The store half of a config-placed bundle can succeed while its
        // config half is refused, which left the same bundle holding a row that said `up to date`
        // AND a place in the failed count — `Checked 2 bundles` over a machine holding one, with
        // the two clauses contradicting each other. The failure is the load-bearing fact (nothing
        // was placed for it, and the warning line says why), so it takes the bundle and the row
        // stands down.
        for name in &outcome.failed_bundles {
            sweep
                .rows
                .retain(|r| &r.skill != name || r.scope.as_deref() != Some(label.as_str()));
        }
        sweep.failed_bundles.extend(outcome.failed_bundles);
        // Work that SUCCEEDED belongs in disclosures: a wholly-owned config file deleted when its
        // last entry left is the removal working, and counted as a fault it makes a clean sweep
        // report itself failed and exit non-zero.
        for notice in outcome.notices {
            if !sweep.disclosures.contains(&notice) {
                sweep.disclosures.push(notice);
            }
        }
        // Config entries that LEFT this run ride the receipt as `removed` rows — one per bundle,
        // counted in the config files the entries lived in; a hand-edited entry stays in place
        // and rides the same row's `kept` list. Named from the delivery cache (the entry's
        // demand is gone, so the cache is the one place its name survives), qualified where the
        // cache still knows the workspace. A STANDING drifted survivor whose sweep removed
        // nothing is a fact, not a byte change: it stays a disclosure line, so the quiet hook
        // never reads a survivor as fresh movement.
        let mut left: BTreeMap<String, (Vec<String>, Vec<String>)> = BTreeMap::new();
        for removed in &outcome.removed {
            let entry = left.entry(removed.bundle_id.clone()).or_default();
            let file = removed
                .state
                .file
                .as_deref()
                .map(|f| super::inventory::pretty(env.ctx, Path::new(f)));
            let list = if removed.state.state == TargetOutcome::Drifted {
                &mut entry.1
            } else {
                &mut entry.0
            };
            if let Some(file) = file
                && !list.contains(&file)
            {
                list.push(file);
            }
        }
        for removed in &outcome.removed {
            if removed.state.state != TargetOutcome::Drifted {
                continue;
            }
            if left
                .get(&removed.bundle_id)
                .is_some_and(|(files, _)| !files.is_empty())
            {
                continue; // the bundle's own `removed` row carries it as a kept file
            }
            let line = format!(
                "MCP_DRIFTED {}: a hand-edited entry in {} is left in place",
                removed.state.agent,
                removed.state.file.as_deref().unwrap_or("its config"),
            );
            if !sweep.disclosures.contains(&line) {
                sweep.disclosures.push(line);
            }
        }
        for (bundle_id, (files, kept)) in left {
            if files.is_empty() {
                continue; // nothing left this run — the drifted survivor was disclosed above
            }
            let named = env.prior.workspaces.iter().find_map(|(ws_id, e)| {
                let ds = e.delivered.get(&bundle_id)?;
                let display = match (e.host.as_deref(), e.workspace_name.as_deref()) {
                    (Some(h), Some(w)) => Some(env.qualified(h, w, &ds.name)),
                    _ => None,
                };
                Some((ds.name.clone(), ws_id.clone(), display))
            });
            let (name, ws_id, display) = match named {
                Some((n, w, d)) => (n, Some(w), d),
                // No delivery cache row describes this bundle (a local row, a workspace whose
                // cache entry is gone). Its OWN record still holds the name it was placed under —
                // ask that before falling back to the opaque id, or a receipt announces the
                // removal of something nobody can recognise.
                None => {
                    let recorded = SkillId::parse(&bundle_id).ok().and_then(|sid| {
                        doc::read_doc::<Lock>(env.ctx.fs, &layout.published(&sid).lock)
                            .ok()
                            .flatten()
                            .map(|l| l.name)
                    });
                    let name = recorded.unwrap_or_else(|| {
                        bundle_id
                            .strip_prefix("local:")
                            .unwrap_or(&bundle_id)
                            .to_owned()
                    });
                    (name, None, None)
                }
            };
            let mut row = plain_row(&name, PullAction::Removed, ws_id, &label);
            row.kind = BundleKind::Mcp.tag();
            row.display = display;
            row.destinations = files;
            row.kept = kept;
            sweep.push(row);
        }
        for bundle in outcome.bundles {
            // The receipt row for this bundle in this scope carries the per-agent outcomes — found
            // by the IDENTITY the demand was filed under, so a workspace `linear` and a local
            // `linear` standing in one scope never trade states.
            let at = sweep
                .mcp_rows
                .get(&(label.clone(), bundle.bundle_id.clone()))
                .copied();
            if let Some(row) = at.and_then(|i| sweep.rows.get_mut(i)) {
                let mut states = bundle.states.clone();
                sync_engine::prettify_state_files(env.ctx, &mut states);
                // A first-install row counts the config FILES its entries landed in — the
                // destination column a config-placed bundle speaks in.
                if row.action == PullAction::Installed {
                    row.destinations = sync_engine::config_destinations(env.ctx, &states);
                }
                // The store held the right bytes all along, so the sync said "up to date" — but
                // this run WROTE a config entry, so disk moved, and a row that still read `up to
                // date` would leave the receipt's own verb saying the run only looked. THE RULE:
                // a bundle whose config entries this scope had never placed before is an INSTALL
                // (the first-ever placement of a manifest line, whose store side is a no-op
                // sync); a write over entries custody already recorded is the ordinary
                // catch-up — a repair, where no version moved and only bytes on disk did.
                else if row.action == PullAction::UpToDate && bundle.wrote {
                    row.action = if bundle.first_placement {
                        PullAction::Installed
                    } else {
                        PullAction::Refreshed
                    };
                    row.destinations = sync_engine::config_destinations(env.ctx, &states);
                }
                row.harnesses = states;
            }
            // The merged map (the applied report + the delivery cache): person wins per slug,
            // the project fills slugs the person scope did not touch.
            //
            // Both of those answer a STANDING question — where this installation's entries live —
            // not "what did this run do", so `placed` settles back to `current` on the way in: a
            // fleet row (and the offline deep dive) would otherwise say a different word about
            // the same unchanged file depending on which sweep last touched it. The RUN's own
            // receipt above keeps the distinction, which is where it means something.
            let entry = merged.entry(bundle.bundle_id).or_default();
            for mut state in bundle.states {
                if state.state.wrote() {
                    state.state = TargetOutcome::Current;
                }
                let replace = person_scope || !entry.iter().any(|s| s.agent == state.agent);
                if replace {
                    entry.retain(|s| s.agent != state.agent);
                    entry.push(state);
                }
            }
        }
    }
    merged
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
///
/// CONFIG-PLACED (mcp) records pass through here UNTOUCHED by construction: they hold no
/// placement dirs, so `clean_written_placements` finds no targets and the withdraw arm cleans
/// nothing — their removal convergence is [`run_mcp_converge`]'s (the config entries leave
/// through the drivers' prior-matched removal, never through a dir sweep).
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
                if !ctx.fs.exists(&ctx.layout.skill_dir(&sid))
                    || sidecar::record_retired(ctx.fs, &ctx.layout, &sid)
                {
                    continue;
                }
                // WHY it left decides how much goes: this machine's own choice keeps the bytes
                // AND every locally-edited copy in place (removing a line is first-class — the
                // person's work is not swept up with it); a feed withdrawal resets to
                // never-received, so a re-delivery installs afresh instead of reading as
                // already-current. No feed row, no feed — file or no file: a machine with no
                // global file demands nothing, so cleanup never depends on the file's existence.
                let switched_off = plan.off_for(&host, &ws, &cached.name).is_some();
                let withheld = !plan.has_feed(&host, &ws);
                let by_choice = switched_off || withheld || cached.via_manifest;
                if by_choice {
                    match clean_by_choice(ctx, &sid, true) {
                        Ok(Some(clean)) => {
                            let mut row = plain_row(
                                &clean.name,
                                PullAction::Removed,
                                Some(run.session.workspace_id.clone()),
                                &label,
                            );
                            row.display = Some(env.qualified(&host, &ws, &cached.name));
                            row.destinations = clean.removed;
                            row.kept = clean.kept;
                            sweep.push(row);
                        }
                        Ok(None) => {}
                        Err(e) => note_item_failure(
                            ctx,
                            &mut sweep.warnings,
                            &mut sweep.failed_bundles,
                            skill_id,
                            &e,
                        ),
                    }
                } else {
                    match withdraw_person_scope(ctx, &sid) {
                        Ok(name) => sweep.push(plain_row(
                            &name,
                            PullAction::Withdrawn,
                            Some(run.session.workspace_id.clone()),
                            &label,
                        )),
                        Err(e) => note_item_failure(
                            ctx,
                            &mut sweep.warnings,
                            &mut sweep.failed_bundles,
                            skill_id,
                            &e,
                        ),
                    }
                }
            }
        }
        // Forge imports the person recipe no longer names ride the SAME undemanded clean: a
        // dropped repo row is this machine's own choice, so its members retire exactly like a
        // dropped workspace row's placements — unedited copies uninstalled, edited copies kept in
        // place, the sidecar bytes kept, in-checkout placements untouched (they are a project
        // scope's business). A row that still names the origin mentioned every tracked member
        // above, so a transient forge failure can never read as a drop.
        for import in forge_imports(ctx) {
            if mentioned.contains(&import.lock.name) || adopted.contains(import.sid.as_str()) {
                continue;
            }
            match clean_by_choice(ctx, &import.sid, true) {
                Ok(Some(clean)) => {
                    let mut row = plain_row(&clean.name, PullAction::Removed, None, &label);
                    row.destinations = clean.removed;
                    row.kept = clean.kept;
                    sweep.push(row);
                }
                Ok(None) => {}
                Err(e) => note_item_failure(
                    ctx,
                    &mut sweep.warnings,
                    &mut sweep.failed_bundles,
                    &import.lock.name,
                    &e,
                ),
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
        if sidecar::record_retired(pctx.fs, &playout, &sid) {
            continue; // a retired record's kept files are the person's own — never swept
        }
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
            .filter(|(i, p)| {
                // A failed set expansion freezes everything under its project dir — a member's dir
                // must survive the sweep that could not see the member list.
                if sweep
                    .unexpanded
                    .iter()
                    .any(|sd| Path::new(p).starts_with(sd))
                {
                    return false;
                }
                // The user's own adopted-in-place source dir is never the sweep's to retire.
                if map
                    .placement_state
                    .get(*i)
                    .is_some_and(|s| s.adopted_source)
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
        // A dropped project row is this scope's own choice too: unedited copies leave, edited (or
        // unreadable) copies stay in place — the same keep rule the person clean applies.
        let cleaned = crate::sidecar::lock_skill(pctx.fs, &pctx.layout, &sid)
            .and_then(|_guard| retire_split(&pctx, &sid, &lock, &map, &stale));
        match cleaned {
            Ok(Some(clean)) => {
                let mut row = plain_row(&clean.name, PullAction::Removed, None, &label);
                row.display = env.prior.workspaces.iter().find_map(|(_, e)| {
                    let ds = e.delivered.get(sid.as_str())?;
                    Some(env.qualified(e.host.as_deref()?, e.workspace_name.as_deref()?, &ds.name))
                });
                row.destinations = clean.removed;
                row.kept = clean.kept;
                sweep.push(row);
            }
            Ok(None) => {}
            Err(e) => note_item_failure(
                ctx,
                &mut sweep.warnings,
                &mut sweep.failed_bundles,
                &lock.name,
                &e,
            ),
        }
    }
}

/// Remove the retirement marker of every record an identity claim touched this run — the
/// re-demand is the explicit act that returns a released record to the surfaces. Driven scopes
/// only (an undriven scope claimed nothing); best-effort, like the revive itself.
fn revive_reclaimed(
    env: &Env<'_>,
    driven: &Driven,
    project: Option<&(PathBuf, ScopePlan)>,
    sweep: &Sweep,
) {
    let mut layouts: Vec<(String, sidecar::Layout)> = Vec::new();
    if driven.person {
        layouts.push((ResolvedScope::Person.label(), env.ctx.layout.clone()));
    }
    if driven.project
        && let Some((dir, _)) = project
        && let Some(playout) = sidecar::existing_project_store(env.ctx.fs, dir)
    {
        layouts.push((ResolvedScope::Project { dir: dir.clone() }.label(), playout));
    }
    for (label, id) in &sweep.synced {
        let Some((_, layout)) = layouts.iter().find(|(l, _)| l == label) else {
            continue;
        };
        let Ok(sid) = SkillId::parse(id) else {
            continue;
        };
        sidecar::revive_record(env.ctx.fs, layout, &sid);
    }
}

/// The ONE-TIME ORPHAN RESOLUTION — records may describe rows, never create them, so a store
/// record that is claimed by NO row and delivered by NOTHING gets one closing statement on this
/// receipt and then RETIRES: the marker written first (a crash between marker and line loses the
/// line, never doubles it), the saved copy kept in the store forever, silently, and every walker
/// skipping the record from here on. Nothing on disk is deleted — placed files stay where they
/// are and belong to the person now; the line says so and where.
///
/// FREEZE DISCIPLINE — resolve only when this sweep can actually KNOW nothing demands the record:
/// the run is untargeted (the caller gates), the scope was driven and its manifest parsed, the
/// delivery cache was readable, and for a record whose cached workspace still holds a LIVE
/// session that workspace produced a fresh snapshot this run — an unreachable workspace freezes
/// its records (an outage must never read as an orphan). A workspace with NO live session at all
/// is no freeze: signed-out is a deliberate state, and its leftovers are exactly what this pass
/// resolves. Failed channel expansions and unexpanded project sets freeze theirs; an MCP record
/// with live custody entries waits for the converge to conclude; a record that already earned a
/// receipt row this run (the cleaner's removed/withdrawn) is fresh news, not a standing orphan.
fn resolve_orphans(
    env: &Env<'_>,
    driven: &Driven,
    all: &sessions::Sessions,
    person_parsed: bool,
    project: Option<&(PathBuf, ScopePlan)>,
    project_frozen: bool,
    sweep: &mut Sweep,
) {
    if sweep.mcp_blind {
        return; // the delivery cache was unreadable: every "nothing delivers it" would be a guess
    }
    let ctx = env.ctx;
    let mut scopes: Vec<(String, sidecar::Layout)> = Vec::new();
    if driven.person && person_parsed {
        scopes.push((ResolvedScope::Person.label(), ctx.layout.clone()));
    }
    if driven.project
        && !project_frozen
        && let Some((dir, _)) = project
        && let Some(playout) = sidecar::existing_project_store(ctx.fs, dir)
    {
        scopes.push((ResolvedScope::Project { dir: dir.clone() }.label(), playout));
    }
    let live: HashSet<&str> = all.live().map(|s| s.workspace_id.as_str()).collect();
    let now_ms = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    for (label, layout) in scopes {
        let mentioned = sweep.mentioned.get(&label).cloned().unwrap_or_default();
        let synced: HashSet<String> = sweep
            .synced_in(&label)
            .into_iter()
            .map(str::to_owned)
            .collect();
        let reported: HashSet<String> = sweep
            .rows
            .iter()
            .filter(|r| r.scope.as_deref() == Some(label.as_str()))
            .map(|r| r.skill.clone())
            .collect();
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
            if super::builtin::is_builtin(id)
                || sidecar::record_retired(ctx.fs, &layout, &sid)
                // A record still carrying config entries is placed, not orphaned — its own
                // document says so.
                || !crate::config_custody::entries_of(ctx.fs, &layout, id).is_empty()
            {
                continue;
            }
            let sp = layout.published(&sid);
            let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &sp.lock) else {
                continue; // an unreadable record is frozen, never judged
            };
            if mentioned.contains(&lock.name)
                || synced.contains(id)
                || reported.contains(&lock.name)
            {
                continue;
            }
            // The delivery answer, workspace by workspace, over the cache: a live-session
            // workspace must have answered fresh THIS run (else freeze), and a fresh answer that
            // still delivers the id keeps the record out of here entirely.
            let mut frozen = false;
            let mut delivered = false;
            let mut ws_known: Option<(String, String, String)> = None;
            for (ws_id, e) in &env.prior.workspaces {
                let Some(ds) = e.delivered.get(id) else {
                    continue;
                };
                if let (Some(h), Some(w)) = (e.host.as_deref(), e.workspace_name.as_deref())
                    && ws_known.is_none()
                {
                    ws_known = Some((ws_id.clone(), h.to_owned(), w.to_owned()));
                }
                if live.contains(ws_id.as_str()) {
                    match env
                        .runs
                        .iter()
                        .find(|r| r.session.workspace_id == *ws_id)
                        .and_then(|r| r.snapshot.as_ref())
                    {
                        None => frozen = true,
                        Some(snap) => {
                            if snap.skills.iter().any(|s| s.skill_id == *id) {
                                delivered = true;
                            }
                        }
                    }
                    if ds
                        .via_channels
                        .iter()
                        .any(|c| sweep.failed_channels.contains(&(ws_id.clone(), c.clone())))
                    {
                        frozen = true;
                    }
                }
            }
            // A fresh snapshot delivering the id keeps it even without a cache row.
            delivered = delivered
                || env.runs.iter().any(|r| {
                    r.snapshot
                        .as_ref()
                        .is_some_and(|s| s.skills.iter().any(|d| d.skill_id == *id))
                });
            if frozen || delivered {
                continue;
            }
            let map: PlacementMap = match doc::read_map(ctx.fs, &sp.map) {
                Ok(Some(m)) => m,
                _ => continue, // no/unreadable map: frozen, never judged
            };
            // A failed set expansion froze everything under its project dir.
            if map.placements.iter().any(|p| {
                sweep
                    .unexpanded
                    .iter()
                    .any(|sd| Path::new(p).starts_with(sd))
            }) {
                continue;
            }
            // RETIRE FIRST, then say it — at-most-once presentation.
            if let Err(e) = sidecar::retire_record(ctx.fs, &layout, &sid, now_ms) {
                note_item_failure(
                    ctx,
                    &mut sweep.warnings,
                    &mut sweep.failed_bundles,
                    &lock.name,
                    &e,
                );
                continue;
            }
            let (recorded, present) = orphan_placements(ctx, &map);
            let fact = orphan_fact(
                ws_known.as_ref().map(|(_, _, w)| w.as_str()),
                &present,
                &recorded,
            );
            let mut row = plain_row(
                &lock.name,
                PullAction::Released,
                ws_known.as_ref().map(|(ws_id, _, _)| ws_id.clone()),
                &label,
            );
            row.display = ws_known
                .as_ref()
                .map(|(_, h, w)| env.qualified(h, w, &lock.name));
            row.note = Some(fact);
            sweep.push(row);
        }
    }
}

/// The record's placements as display paths, in two readings: EVERY placement it listed, and the
/// subset that still EXISTS on disk — the two halves the released line speaks from ("what was
/// recorded" and "what is still there").
fn orphan_placements(ctx: &Ctx<'_>, map: &PlacementMap) -> (Vec<String>, Vec<String>) {
    let mut recorded = Vec::with_capacity(map.placements.len());
    let mut present = Vec::new();
    for p in &map.placements {
        let path = Path::new(p);
        let shown = super::inventory::pretty(ctx, path);
        if ctx.fs.exists(path) {
            present.push(shown.clone());
        }
        recorded.push(shown);
    }
    (recorded, present)
}

/// The concrete fact a released row states — WHY the record resolved, and then WHERE that leaves
/// the files, one line each (the receipt indents the second under the first). `workspace` is the
/// delivering workspace's address name when the delivery cache knows it; `present` the
/// still-existing placement dirs (display paths — exactly one is named inline, several are listed
/// beneath, per this receipt's own convention for naming folders a person can go and look in);
/// `recorded` EVERY placement the record listed (display paths, `present` among them).
///
/// The lines are separate facts, so each stands on its own: with no workspace name to say who
/// stopped sharing, the row states only where the files stand — never a sentence with a hole in it.
///
/// With nothing left on disk the fact states the DISCOVERY, never an act: the copies were already
/// gone when the sweep looked, and this run deleted nothing. So it names what the record knew of
/// (the one copy, or the last of several), says it no longer exists, and closes on the only
/// consequence that matters — nothing left to manage. A phrasing that reads as a removal would
/// claim a deletion nobody performed.
pub(crate) fn orphan_fact(
    workspace: Option<&str>,
    present: &[String],
    recorded: &[String],
) -> String {
    const YOURS: &str = "the files stay where they are, and are yours to keep or delete:";
    let files = match present {
        [] => None,
        [one] => Some(format!("{YOURS} {one}")),
        many => Some(format!(
            "{YOURS}\n{}",
            many.iter()
                .map(|p| format!("  {p}"))
                .collect::<Vec<_>>()
                .join("\n")
        )),
    };
    match (workspace, files) {
        (Some(ws), Some(f)) => {
            format!("{ws} stopped sharing this — topos will not update it any more\n{f}")
        }
        (None, Some(f)) => f,
        (_, None) => match recorded {
            [] => "no copies remain — nothing left to manage".to_owned(),
            [one] => format!("its only copy, {one}, no longer exists — nothing left to manage"),
            [.., last] => format!(
                "its {} copies, last at {last}, no longer exist — nothing left to manage",
                recorded.len()
            ),
        },
    }
}

/// A row-dropped bundle's by-choice clean over ONE store: exactly the placements topos itself
/// WROTE (`materialized_sha` present and NOT the `adopted_source` marker — the user's own
/// adopted-in-place dir carries a baseline sha for drift detection but is never deleted),
/// snapshot-first; the sync doc is NOT reset. With `exclude_project` (the HOME store's posture)
/// placements inside some project checkout are left alone — a project manifest may still demand
/// the bundle there, and that checkout reconciles lazily when visited; a PROJECT store passes
/// `false` (its placements are its own). `Ok(Some(name))` when something was actually cleaned.
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
                && !st.adopted_source
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

/// What one BY-CHOICE clean concluded: the bundle's name, the destinations actually uninstalled,
/// and the edited (or unreadable) copies kept in place — display paths, `~`-abbreviated.
pub(super) struct ByChoiceClean {
    pub(super) name: String,
    pub(super) removed: Vec<String>,
    pub(super) kept: Vec<String>,
}

/// [`clean_written_placements`]'s BY-CHOICE twin — the clean a person's own manifest choice (a
/// dropped feed line, an `"off"` switch, a dropped row) drives: the same written-placement target
/// set, split by [`retire_split`] so an EDITED copy stays in place instead of being
/// snapshotted-and-removed. `Ok(None)` when the bundle has nothing recorded to retire.
///
/// `pub(super)` for ONE caller besides the sweep: a whole-row `remove -g`'s eager cleanup, whose
/// dropped record the cache/forge walks may not enumerate (see [`super::manifest_edit`]) — it
/// reads the record directly (a retired marker does not hide it: the placements are what retire).
pub(super) fn clean_by_choice(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    exclude_project: bool,
) -> Result<Option<ByChoiceClean>, ClientError> {
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
                && !st.adopted_source
                && (!exclude_project || !is_project_placement(ctx, Path::new(p)))
        })
        .map(|(i, _)| i)
        .collect();
    if targets.is_empty() {
        return Ok(None);
    }
    retire_split(ctx, sid, &lock, &map, &targets)
}

/// Retire exactly `indices` of one bundle's placements BY CHOICE: an UNEDITED copy leaves through
/// the ordinary park-then-verify rail; a copy whose bytes differ from its recorded baseline — or
/// one that cannot be read at all, which fails toward keeping — is KEPT IN PLACE (an edited copy
/// is the person's own work; ending delivery must never sweep it up). A kept edited copy is
/// snapshotted into the store first (its bytes stay reachable through `topos log`), then its
/// RECORD is released: the map no longer manages the dir, so a later sweep neither re-reports it
/// nor ever overwrites it — the file is the person's own from here on. Foreign dirs stay exactly
/// as they are, record and all. `Ok(None)` = nothing removed and nothing kept.
///
/// THE CALLER HOLDS THE SKILL's writer flock.
fn retire_split(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    lock: &Lock,
    map: &PlacementMap,
    indices: &[usize],
) -> Result<Option<ByChoiceClean>, ClientError> {
    let scans = placement::scan_placements(ctx, map)?;
    let mut removable: Vec<usize> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    let mut kept: Vec<usize> = Vec::new();
    for &i in indices {
        match &scans[i].status {
            placement::ScanStatus::Clean { .. } => {
                removable.push(i);
                removed.push(super::inventory::pretty(ctx, &scans[i].dir));
            }
            // No dir on disk: the record simply retires, nothing to name on the receipt.
            placement::ScanStatus::Absent => removable.push(i),
            placement::ScanStatus::Modified { .. } | placement::ScanStatus::Unscannable => {
                kept.push(i);
            }
            placement::ScanStatus::Foreign => {}
        }
    }
    for &i in &kept {
        if let placement::ScanStatus::Modified { scanned } = &scans[i].status {
            sync_engine::snapshot_draft(ctx, &ctx.layout.published(sid), lock, scanned)?;
        }
    }
    if !removable.is_empty() {
        clean_placements(ctx, sid, lock, map, &removable)?;
    }
    if !kept.is_empty() {
        let raw: Vec<String> = kept.iter().map(|&i| map.placements[i].clone()).collect();
        release_records(ctx, sid, &raw)?;
    }
    let kept: Vec<String> = kept
        .iter()
        .map(|&i| super::inventory::pretty(ctx, Path::new(&map.placements[i])))
        .collect();
    if removed.is_empty() && kept.is_empty() {
        return Ok(None);
    }
    Ok(Some(ByChoiceClean {
        name: lock.name.clone(),
        removed,
        kept,
    }))
}

/// Release the RECORDS of kept copies (by their recorded raw paths): the bytes stay on disk,
/// untouched; the map just stops managing those dirs. Runs under the caller's skill flock, over
/// the map AS IT IS NOW (a preceding [`clean_placements`] already rewrote it).
fn release_records(ctx: &Ctx<'_>, sid: &SkillId, paths: &[String]) -> Result<(), ClientError> {
    let sp = ctx.layout.published(sid);
    let Some(mut map) = doc::read_map(ctx.fs, &sp.map)? else {
        return Ok(());
    };
    let keep: Vec<bool> = map.placements.iter().map(|p| !paths.contains(p)).collect();
    let mut it = keep.iter();
    map.placements.retain(|_| *it.next().unwrap_or(&true));
    let mut it = keep.iter();
    map.placement_state.retain(|_| *it.next().unwrap_or(&true));
    doc::write_map(ctx.fs, &sp.map, &map)
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
        // A RETIRED record is no import any more — nothing enumerates, updates, or cleans it.
        if sidecar::record_retired(ctx.fs, &ctx.layout, &sid) {
            continue;
        }
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
    // The adopted-in-place SOURCE dir is the user's own — topos never created it, and no sweep
    // may delete or empty it, whatever else retires. Filtered HERE, at the one choke point every
    // undemanded/withdraw clean rides, so the guarantee is structural (callers pre-filter too,
    // for honest receipts).
    let indices: Vec<usize> = indices
        .iter()
        .copied()
        .filter(|&i| !map.placement_state.get(i).is_some_and(|s| s.adopted_source))
        .collect();
    let indices = indices.as_slice();
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
// `--force` (the rebuild repair).
// =================================================================================================

/// Rebuild ONE store: for every bundle it tracks, ABSORB each distinct edited copy into the store,
/// then drop the recorded placement dirs and reset the bundle to never-received, so the ordinary
/// sweep re-projects it pristine. The order is the whole guarantee — a rebuild is a repair, and a
/// repair that can lose an edit is not one. A bundle whose placements cannot be read is left exactly
/// as it is, with a line saying so — and so is one whose merge is still undecided ([`rebuild_skill`]).
///
/// TWO records are skipped outright. A RETIRED one is never re-projected: the sweep would not write
/// it back, so a rebuild that dropped its dirs would be a plain delete of the person's own copies.
/// And the BUILT-IN is not delivered from anywhere — the bare sweep force-syncs it from THIS BINARY,
/// on its own, so it has no drift a rebuild could repair and dropping its folder here would only
/// take away a dir this run never asked about.
fn rebuild_store(ctx: &Ctx<'_>, layout: &crate::sidecar::Layout, sweep: &mut Sweep) {
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
        if super::builtin::is_builtin(sid.as_str()) || sidecar::record_retired(ctx.fs, layout, &sid)
        {
            continue;
        }
        match rebuild_skill(ctx, &sid) {
            Ok(None) => {}
            // Undecided merge: a DECISION, not a fault. Nothing is broken and no retry helps —
            // the folders stand exactly as they were until the person picks a side.
            Ok(Some(decision)) => sweep.decisions.push(decision),
            Err(e) => sweep.warnings.push(format!(
                "REBUILD_SKIPPED {id}: {}",
                crate::render::safe_message(&e)
            )),
        }
    }
}

/// [`rebuild_store`] for one bundle (see its doc for the ordering rule). `Ok(Some(..))` is a bundle
/// this rebuild deliberately did NOT touch, with the decision that says why.
fn rebuild_skill(
    ctx: &Ctx<'_>,
    sid: &SkillId,
) -> Result<Option<super::PendingDecision>, ClientError> {
    let sp = ctx.layout.published(sid);
    {
        let _guard = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
        let lock: Option<Lock> = doc::read_doc(ctx.fs, &sp.lock)?;
        let map: Option<PlacementMap> = doc::read_map(ctx.fs, &sp.map)?;
        let (Some(lock), Some(map)) = (lock, map) else {
            return Ok(None);
        };
        // A bundle whose merge is still undecided has NO pristine version to re-project. Its
        // folders hold the person's own bytes; the lock's base is the team's; and the sweep that
        // would put a rebuilt folder back stops at the block and writes no placement. Dropping the
        // dirs here would therefore empty every agent folder and leave them empty until the merge
        // is settled — a repair that breaks the thing it repairs. So the rebuild stands aside and
        // names the two exits, each of which rewrites every managed placement on its way out.
        // Presence is the test, exactly as it is for the publish gate.
        if ctx.fs.exists(&sp.conflict) {
            return Ok(Some(rebuild_blocked_decision(ctx, &lock.name)));
        }
        if map.placements.is_empty() {
            return Ok(None);
        }
        let all: Vec<usize> = (0..map.placements.len()).collect();
        clean_placements(ctx, sid, &lock, &map, &all)?;
    }
    let sync: Option<SyncState> = doc::read_doc(ctx.fs, &sp.sync)?;
    super::pull::reset_to_never_received(ctx, sid, sync.as_ref())?;
    Ok(None)
}

/// Why a rebuild left a blocked bundle's folders exactly as they were, with both exits spelled for
/// the scope the store lives in (the layout IS the scope) so each is runnable as printed.
///
/// The bundle leads its own line and the way out sits under it, one command per line — the shape
/// every other per-bundle answer takes. No internal code prefixes it: the reader is being told what
/// happened to a folder of theirs, and a code says nothing they can act on.
///
/// It carries NO argv of its own. The blocked bundle is re-disclosed as a conflicted ROW in the
/// same run, and that row already offers the two acts that end the merge as typed
/// `RESOLVE_DIVERGED_DRAFT` actions. Two spellings of one decision on one machine surface is one
/// too many: an agent reading four actions for two choices has to work out that half of them are
/// the same commands.
fn rebuild_blocked_decision(ctx: &Ctx<'_>, name: &str) -> super::PendingDecision {
    let g = crate::error::scope_flag(!ctx.layout.is_project_scope());
    super::PendingDecision {
        name: name.to_owned(),
        line: "waiting on a merge decision, so its folders were left as they are".to_owned(),
        detail: vec![
            "settle it first, then rebuild:".to_owned(),
            format!("  topos update{g} {name} --keep-mine"),
            format!("  topos update{g} {name} --reset"),
        ],
        ways_out: Vec::new(),
    }
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
        merge: None,
        synced_placements: None,
        destinations: Vec::new(),
        kept: Vec::new(),
        display: None,
        note: None,
        scope: Some(scope.to_owned()),
        harnesses: Vec::new(),
        kind: None,
        draft: false,
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
fn note_item_failure(
    ctx: &Ctx<'_>,
    warnings: &mut Vec<String>,
    failed: &mut std::collections::BTreeSet<String>,
    name: &str,
    e: &ClientError,
) {
    failed.insert(name.to_owned());
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
///
/// The two channels are not interchangeable. A PARK that landed is an advisory — it says where
/// the person's old history now lives, and a handover that worked must not make the run report a
/// failure or exit non-zero. A handover that could NOT complete is a real fault and rides
/// `warnings`.
pub(crate) fn handover_legacy_project_rows(
    ctx: &Ctx<'_>,
    project_dirs: &[PathBuf],
    advisories: &mut Vec<String>,
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
        if sidecar::record_retired(ctx.fs, &ctx.layout, &sid) {
            continue; // a retired record left every surface — never handed over
        }
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
                    advisories.push(format!(
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
