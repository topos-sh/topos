//! The MANIFEST reconcile — `update` on the manifest-layer model: **an agent gets demand ∩
//! entitlement**. Resolve every manifest covering the working directory (this folder's
//! `topos.toml` chain → each logged-in session's server-stored profile, delivered as a ready-made
//! layer → the local personal manifest), nearest-first with excludes, then converge the machine on
//! the winning set:
//!
//! - **profile items** ride each session's ONE delivery answer (the server already intersected
//!   demand with entitlement and expanded channels) and land in the HOME harness dirs;
//! - **project items** (a folder's `topos.toml`) resolve through the session the reference's
//!   host/workspace names — the catalog read supplies the current pointer — and materialize
//!   INSIDE the project (its harness dirs, kept out of commits via `.git/info/exclude`);
//! - **external GitHub refs** install at their PINNED commit (lockfile logic — no governance rail
//!   behind them); **local path refs** are adopt-in-place facts (presence is the delivery).
//!
//! Delivery is silent, npm-style — login was the acceptance event, so nothing here asks. The
//! reconcile also maintains the OFFLINE DELIVERY CACHE (`state/sync_status.json`): per session the
//! delivered set with names + protection, which `status`/`list` read without a network call and
//! the cache-backed follow seam ([`CacheFollow`]) is built over.
//!
//! Non-oracle discipline: the server answers uniformly (the same 404 for "doesn't exist" / "not a
//! member"); every honest status line here is phrased from LOCAL knowledge — which manifest asked,
//! which session is missing — never from a server confirmation.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

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
use crate::manifest::file::MANIFEST_FILE;
use crate::manifest::refs::ParsedRef;
use crate::manifest::resolve::{
    Layer, LayerSource, Resolution, ResolvedItem, ResolvedScope, resolve_layers,
};
use crate::manifest::walk;
use crate::plane::{
    DeliverySnapshot, DirectorySource, FollowContext, FollowMode, FollowSource, LinkStatus,
    PlaneError, PlaneSource, ReconcileTransport,
};
use crate::sessions::{self, SESSION_ACTIVE, SESSION_ENDED, SESSION_PENDING, Session};
use crate::sync_status::{self, DeliveredSkill, WorkspaceSync};
use crate::{doc, placement, sidecar};

use super::pull::PullOutcome;
use super::sync_engine::{self, Invocation};

/// The per-session transports the reconcile drives: the byte/delivery lane (one `UreqPlane` under
/// the session's credential) and the directory lane (catalog + channel reads).
pub(crate) struct SessionTransports {
    pub plane: Box<dyn ReconcileTransport>,
    pub directory: Box<dyn DirectorySource>,
    /// The contribute-write lane (publish / propose / revert / review) under the same credential.
    pub contribute: Box<dyn crate::plane::ContributeSource>,
    /// The governance lane (invitations; the session self-revoke). Consumed by the invite fold.
    #[allow(dead_code)]
    pub governance: Box<dyn crate::plane::GovernanceSource>,
}

/// Builds the transports for ONE session (per-workspace credentials — the session model).
pub(crate) type SessionConnect<'a> = dyn Fn(&Session) -> SessionTransports + 'a;

/// How a manifest reconcile behaves.
#[derive(Default)]
pub(crate) struct ManifestUpdateOpts {
    /// Targeted names/references (`topos update <name>…`); empty = the full sweep.
    pub targets: Vec<String>,
    /// Ack the delivered notices (the interactive / `--json` update); the quiet hook fetches
    /// WITHOUT acking, so nothing is marked read that no one narrated.
    pub ack_notices: bool,
}

/// One session's runtime state for this sweep.
struct SessionRun {
    session: Session,
    transports: SessionTransports,
    /// The delivery answer (`None` = unreachable this run — the profile layer is cache-fed and
    /// the engine converges from the local store).
    snapshot: Option<DeliverySnapshot>,
    /// Lazily fetched catalog (project-ref resolution). `Some(None)` = fetch failed this run.
    skills_index: std::cell::RefCell<Option<Option<WireSkillIndex>>>,
    channels_index: std::cell::RefCell<Option<Option<WireChannelIndex>>>,
}

impl SessionRun {
    /// The catalog, fetched once per run (a failure caches as `None` — one warning, not N).
    fn catalog(&self, warnings: &mut Vec<String>) -> Option<WireSkillIndex> {
        let mut slot = self.skills_index.borrow_mut();
        if slot.is_none() {
            let fetched = match self
                .transports
                .directory
                .skills_index(&self.session.workspace_id)
            {
                Ok(ix) => Some(ix),
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

    fn channels(&self, warnings: &mut Vec<String>) -> Option<WireChannelIndex> {
        let mut slot = self.channels_index.borrow_mut();
        if slot.is_none() {
            let fetched = match self
                .transports
                .directory
                .channels_index(&self.session.workspace_id)
            {
                Ok(ix) => Some(ix),
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
/// person-scope plan reads for workspace provenance. Replaces the retired subscription file: the
/// delivered set IS the standing state, and demand lives in manifests.
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

/// The SESSION-ROUTED plane — the app ctx's `PlaneSource` when the installation runs on
/// sessions: each per-skill read routes to the session lane of the workspace the skill belongs to
/// (the offline delivery cache supplies the map; `bind_skill` teaches new pairs mid-run). A skill
/// no session covers answers "not served", exactly like the retired inert source.
pub(crate) struct SessionRoutedPlane {
    lanes: Vec<(String, Box<dyn ReconcileTransport>)>,
    skill_ws: std::cell::RefCell<std::collections::HashMap<String, String>>,
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
        let mut skill_ws = std::collections::HashMap::new();
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

/// The manifest reconcile (see the module doc). Returns the same [`PullOutcome`] shape the hook
/// and the `update` finishers already consume — `access_gone` carries sessions that answered the
/// uniform 404 (ended server-side), `unreachable` the transport failures.
pub(crate) fn manifest_update(
    ctx: &Ctx<'_>,
    connect: &SessionConnect<'_>,
    git: Option<&dyn crate::git_source::GitTarballSource>,
    opts: &ManifestUpdateOpts,
) -> Result<PullOutcome, ClientError> {
    let mut warnings: Vec<String> = Vec::new();
    let mut access_gone: Vec<String> = Vec::new();
    let mut unreachable: Vec<String> = Vec::new();
    let mut rows: Vec<PullSkill> = Vec::new();
    let mut notices = Vec::new();
    let mut proposals_awaiting: u32 = 0;

    let prior_sync = match sync_status::read(ctx.fs, &ctx.layout) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!("SYNC_STATUS_UNREADABLE: {}", e.detail()));
            sync_status::SyncStatus::default()
        }
    };

    // ---- 1. Dial each live session's delivery (the person layers). ----
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
                warnings.push(format!(
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
            Err(PlaneError::Unreachable(m) | PlaneError::Unavailable(m)) => {
                warnings.push(format!("PLANE_UNAVAILABLE {}: {m}", s.workspace_name));
                unreachable.push(s.workspace_name.clone());
                // The profile layer degrades to the OFFLINE CACHE below — a dead network keeps
                // the local converge working (and the hook never wedges a session start).
                runs.push(SessionRun {
                    session: s.clone(),
                    transports,
                    snapshot: None,
                    skills_index: std::cell::RefCell::new(None),
                    channels_index: std::cell::RefCell::new(None),
                });
            }
            Err(PlaneError::Malformed(m)) => {
                warnings.push(format!("WIRE_INVALID {}: {m}", s.workspace_name));
            }
        }
    }

    // ---- 2. Build the layer chain and resolve. ----
    let mut layers: Vec<Layer> = Vec::new();
    let mut project_dirs: Vec<PathBuf> = Vec::new();
    let mut project_manifests: Vec<(PathBuf, crate::manifest::file::Manifest)> = Vec::new();
    if let Some(roots) = &ctx.roots
        && let Some(cwd) = roots.cwd.as_deref()
    {
        for l in walk::project_layers(ctx.fs, cwd, Some(&roots.home))? {
            project_dirs.push(l.dir.clone());
            project_manifests.push((l.dir.clone(), l.manifest.clone()));
            layers.push(Layer::project(l.dir, l.manifest));
        }
    }
    for run in &runs {
        let delivered: Vec<(String, String, Option<String>)> = match &run.snapshot {
            Some(snap) => snap
                .skills
                .iter()
                .map(|ds| {
                    (
                        ds.name.clone(),
                        format!(
                            "{}/{}/{}",
                            run.session.host, run.session.workspace_name, ds.name
                        ),
                        None,
                    )
                })
                .collect(),
            // Unreachable: the cached delivered set stands in, so resolution (and the local
            // converge) keep working offline.
            None => prior_sync
                .workspaces
                .get(&run.session.workspace_id)
                .map(|e| {
                    e.delivered
                        .iter()
                        .filter(|(_, ds)| !ds.withdrawn && !ds.name.is_empty())
                        .map(|(_, ds)| {
                            (
                                ds.name.clone(),
                                format!(
                                    "{}/{}/{}",
                                    run.session.host, run.session.workspace_name, ds.name
                                ),
                                None,
                            )
                        })
                        .collect()
                })
                .unwrap_or_default(),
        };
        layers.push(Layer::profile(
            run.session.host.clone(),
            run.session.workspace_name.clone(),
            delivered,
        ));
    }
    if let Some(personal) =
        crate::manifest::file::read_manifest(ctx.fs, &ctx.layout.home().join(MANIFEST_FILE))?
    {
        layers.push(Layer::personal(personal));
    }
    let resolution = resolve_layers(&layers);
    for issue in &resolution.issues {
        warnings.push(format!(
            "MANIFEST_ISSUE {}: \"{}\" — {}",
            issue.source.label(),
            issue.reference,
            issue.message
        ));
    }

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

    // ---- 3. Reconcile each resolved item. ----
    let mut synced_ids: HashSet<String> = HashSet::new();
    let mut synced_names: HashSet<String> = HashSet::new();
    let target_names: Vec<String> = opts
        .targets
        .iter()
        .map(|t| {
            crate::manifest::refs::parse_ref(t)
                .map(|p| p.item_name().to_owned())
                .unwrap_or_else(|_| t.clone())
        })
        .collect();
    let mut matched_targets: HashSet<String> = HashSet::new();
    for item in &resolution.items {
        if !target_names.is_empty() {
            if !target_names.iter().any(|t| t == &item.name) {
                continue;
            }
            matched_targets.insert(item.name.clone());
        }
        reconcile_item(
            ctx,
            &runs,
            &follow,
            &project_manifests,
            git,
            item,
            &mut rows,
            &mut warnings,
            &mut synced_ids,
            &mut synced_names,
        );
    }
    if !target_names.is_empty() {
        for t in &target_names {
            if !matched_targets.contains(t) {
                return Err(ClientError::InvalidArgument(format!(
                    "'{t}' is not in any manifest covering this directory or your profiles — \
                     `topos status` shows the resolved set; `topos add` records new demand"
                )));
            }
        }
    }

    // ---- 4. Clean what is no longer demanded (the targeted form never cleans). ----
    if target_names.is_empty() {
        clean_undemanded(
            ctx,
            &runs,
            &prior_sync,
            &resolution,
            &project_dirs,
            &synced_ids,
            &mut rows,
            &mut warnings,
        );
    }

    // ---- 5. Report applied state + refresh the delivery cache, per session. ----
    let now_millis = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);
    let mut sync_updates: Vec<(String, WorkspaceSync)> = Vec::new();
    for run in &runs {
        let Some(snap) = &run.snapshot else {
            continue; // unreachable this run: the prior cache entry stands
        };
        let delivered_ids: HashSet<&str> =
            snap.skills.iter().map(|s| s.skill_id.as_str()).collect();
        let mut report_ok = false;
        match super::pull::applied_snapshot(ctx, &delivered_ids) {
            Ok(applied) => match run
                .transports
                .plane
                .report_applied(&run.session.workspace_id, &applied)
            {
                Ok(()) => report_ok = true,
                Err(e) => {
                    let m = match e {
                        PlaneError::NotFound => "access gone".to_owned(),
                        PlaneError::Unreachable(m)
                        | PlaneError::Unavailable(m)
                        | PlaneError::Malformed(m) => m,
                    };
                    warnings.push(format!("REPORT_FAILED {}: {m}", run.session.workspace_name));
                }
            },
            Err(e) => warnings.push(format!(
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
                },
            );
        }
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
                    warnings.push(format!("ACK_FAILED {}: {m}", run.session.workspace_name));
                }
            }
            notices.extend(snap.notices.iter().cloned());
        }
    }
    if let Err(e) = sync_status::record(ctx.fs, &ctx.layout, &sync_updates) {
        warnings.push(format!("SYNC_STATUS_WRITE_FAILED: {}", e.detail()));
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
            skills: rows,
            proposals_awaiting,
            notices,
            sync,
        },
        warnings,
        access_gone,
        unreachable,
    })
}

/// Reconcile ONE resolved manifest line (per kind + scope). Failures are isolated per item —
/// warnings, never an aborted sweep.
#[allow(clippy::too_many_arguments)]
fn reconcile_item(
    ctx: &Ctx<'_>,
    runs: &[SessionRun],
    follow: &CacheFollow,
    project_manifests: &[(PathBuf, crate::manifest::file::Manifest)],
    git: Option<&dyn crate::git_source::GitTarballSource>,
    item: &ResolvedItem,
    rows: &mut Vec<PullSkill>,
    warnings: &mut Vec<String>,
    synced_ids: &mut HashSet<String>,
    synced_names: &mut HashSet<String>,
) {
    if !synced_names.insert(item.name.clone()) {
        return; // a channel expansion already claimed the name this run
    }
    match (&item.source, &item.parsed) {
        // A profile item: the session's delivery already resolved it.
        (LayerSource::Profile { host, workspace }, _) => {
            let Some(run) = runs
                .iter()
                .find(|r| &r.session.host == host && &r.session.workspace_name == workspace)
            else {
                return; // the layer came from the cache of an unreachable session — nothing to dial
            };
            match &run.snapshot {
                Some(snap) => {
                    if let Some(ds) = snap.skills.iter().find(|s| s.name == item.name) {
                        sync_workspace_skill(
                            ctx,
                            run,
                            follow,
                            &CatalogTarget {
                                skill_id: ds.skill_id.clone(),
                                name: ds.name.clone(),
                                version_id: to_hex(&ds.version_id),
                                generation: ds.generation,
                                bundle_digest: Some(ds.bundle_digest),
                                review_required: ds.review_required,
                            },
                            item.pin.as_deref(),
                            &item.scope,
                            None,
                            rows,
                            warnings,
                            synced_ids,
                        );
                    }
                }
                None => {
                    // Unreachable: converge the cached skill from the LOCAL store (the engine's
                    // offline arm) — the id comes from the cache-fed follow seam.
                    if let Some((skill_id, fc)) = follow
                        .entries
                        .iter()
                        .find(|(id, fc)| {
                            fc.workspace_id == run.session.workspace_id
                                && skill_name_of(ctx, id).as_deref() == Some(&item.name)
                        })
                        .map(|(id, fc)| (id.clone(), fc.clone()))
                        && let Ok(sid) = SkillId::parse(&skill_id)
                    {
                        let run_ctx = super::pull::ctx_with_plane_and_follow(
                            ctx,
                            run.transports.plane.as_plane(),
                            follow,
                        );
                        match sync_engine::sync_one_with(
                            &run_ctx,
                            &sid,
                            &fc,
                            Invocation::Sweep,
                            None,
                        ) {
                            Ok(mut row) => {
                                row.workspace_id = Some(run.session.workspace_id.clone());
                                synced_ids.insert(skill_id);
                                rows.push(row);
                            }
                            Err(e) => note_item_failure(ctx, warnings, &item.name, &e),
                        }
                    }
                }
            }
        }
        // A workspace skill reference in a project / personal manifest.
        (
            _,
            ParsedRef::Skill {
                host,
                workspace,
                name,
                ..
            },
        ) => {
            let Some(run) = find_run(runs, host.as_deref(), workspace) else {
                warnings.push(not_connected_line(item, host.as_deref(), workspace));
                return;
            };
            let Some(catalog) = run.catalog(warnings) else {
                return;
            };
            let Some(entry) = catalog.skills.iter().find(|e| &e.name == name) else {
                warnings.push(format!(
                    "NOT_AVAILABLE {}: \"{}\" — not in {}'s catalog, or not visible with your \
                     current access",
                    item.source.label(),
                    item.reference,
                    run.session.workspace_name
                ));
                return;
            };
            sync_workspace_skill(
                ctx,
                run,
                follow,
                &CatalogTarget::from_entry(entry),
                item.pin.as_deref(),
                &item.scope,
                placement_override(project_manifests, item, warnings),
                rows,
                warnings,
                synced_ids,
            );
        }
        // A channel reference: expand against the session's channel index.
        (
            _,
            ParsedRef::Channel {
                host,
                workspace,
                name,
            },
        ) => {
            let Some(run) = find_run(runs, host.as_deref(), workspace) else {
                warnings.push(not_connected_line(item, host.as_deref(), workspace));
                return;
            };
            let Some(channels) = run.channels(warnings) else {
                return;
            };
            let Some(ch) = channels.channels.iter().find(|c| &c.name == name) else {
                warnings.push(format!(
                    "NOT_AVAILABLE {}: \"{}\" — no such channel in {}, or not visible with your \
                     current access",
                    item.source.label(),
                    item.reference,
                    run.session.workspace_name
                ));
                return;
            };
            let Some(catalog) = run.catalog(warnings) else {
                return;
            };
            for cs in &ch.skills {
                if !synced_names.insert(cs.name.clone()) {
                    continue; // a nearer line already claimed this name
                }
                let Some(entry) = catalog.skills.iter().find(|e| e.skill_id == cs.skill_id) else {
                    continue; // archived / no current — skipped per the resolution rule
                };
                sync_workspace_skill(
                    ctx,
                    run,
                    follow,
                    &CatalogTarget::from_entry(entry),
                    None,
                    &item.scope,
                    placement_override(project_manifests, item, warnings),
                    rows,
                    warnings,
                    synced_ids,
                );
            }
        }
        // A bare catalog name (hand-written): unique across the connected workspaces or an error.
        (_, ParsedRef::Bare { name, .. }) => {
            let mut hits: Vec<(&SessionRun, WireSkillIndexEntry)> = Vec::new();
            for run in runs {
                if let Some(catalog) = run.catalog(warnings)
                    && let Some(e) = catalog.skills.iter().find(|e| &e.name == name)
                {
                    hits.push((run, e.clone()));
                }
            }
            match hits.as_slice() {
                [] => warnings.push(format!(
                    "NOT_AVAILABLE {}: \"{name}\" — not in any connected workspace's catalog, or \
                     not visible with your current access",
                    item.source.label()
                )),
                [(run, entry)] => sync_workspace_skill(
                    ctx,
                    run,
                    follow,
                    &CatalogTarget::from_entry(entry),
                    item.pin.as_deref(),
                    &item.scope,
                    placement_override(project_manifests, item, warnings),
                    rows,
                    warnings,
                    synced_ids,
                ),
                several => {
                    let candidates: Vec<String> = several
                        .iter()
                        .map(|(r, _)| {
                            format!("{}/{}/{name}", r.session.host, r.session.workspace_name)
                        })
                        .collect();
                    warnings.push(format!(
                        "AMBIGUOUS {}: \"{name}\" is in several workspaces — spell one of: {}",
                        item.source.label(),
                        candidates.join(", ")
                    ));
                }
            }
        }
        // An external GitHub origin: install at the pinned commit; verify a tracked one's pin.
        (
            _,
            ParsedRef::GitHub {
                owner,
                repo,
                subdir,
                ..
            },
        ) => {
            reconcile_github(ctx, git, item, owner, repo, subdir, rows, warnings);
        }
        // A local folder: presence IS the delivery (adopted in place; `add`/`publish` manage it).
        (_, ParsedRef::LocalPath { raw }) => {
            let base = match &item.source {
                LayerSource::Project { dir } => dir.clone(),
                _ => ctx.layout.home().to_path_buf(),
            };
            let dir = if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                base.join(raw.trim_start_matches("./"))
            };
            if ctx.fs.exists(&dir) {
                rows.push(PullSkill {
                    skill: item.name.clone(),
                    workspace_id: None,
                    observed: 0,
                    applied: 0,
                    action: PullAction::UpToDate,
                    offer: None,
                    conflict: None,
                    merge: None,
                    merge_preview: None,
                });
            } else {
                warnings.push(format!(
                    "PATH_MISSING {}: \"{raw}\" — the folder is gone; `topos remove {raw}` drops \
                     the line",
                    item.source.label()
                ));
            }
        }
    }
}

/// The one target shape both the delivery and the catalog resolve to.
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

/// Sync ONE workspace bundle toward its served (or pinned) version, at the resolved scope.
#[allow(clippy::too_many_arguments)]
fn sync_workspace_skill(
    ctx: &Ctx<'_>,
    run: &SessionRun,
    follow: &CacheFollow,
    target: &CatalogTarget,
    pin: Option<&str>,
    scope: &ResolvedScope,
    override_dir: Option<String>,
    rows: &mut Vec<PullSkill>,
    warnings: &mut Vec<String>,
    synced_ids: &mut HashSet<String>,
) {
    let Ok(sid) = SkillId::parse(&target.skill_id) else {
        warnings.push(format!(
            "BAD_ID {}: served an invalid skill id",
            target.name
        ));
        return;
    };
    if !synced_ids.insert(target.skill_id.clone()) {
        return; // already reconciled this run under another line
    }
    // The manifest pin overrides the served version (the engine fetches by version id, so an
    // older pin resolves as long as the plane still serves its bytes).
    let version_id = pin
        .filter(|p| *p != target.version_id)
        .map_or_else(|| target.version_id.clone(), |p| p.to_owned());
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
    // The scope decides the placement plan: person → the home engine; project → in-checkout dirs.
    let project_dir = match scope {
        ResolvedScope::Project { dir } => Some(dir.clone()),
        ResolvedScope::Person => None,
    };
    let naming_slug = run.session.workspace_name.clone();
    let plan_fn =
        |ctx: &Ctx<'_>, skill_id: &str, lock: &Lock, map: &PlacementMap| match &project_dir {
            Some(dir) => placement::project_plan(
                ctx,
                dir,
                skill_id,
                topos_harness::PlacementNaming {
                    name: Some(&lock.name),
                    workspace_slug: Some(&naming_slug),
                },
                override_dir.as_deref(),
                Some(map),
                None,
            ),
            None => placement::plan_for_skill(ctx, skill_id, lock, map),
        };
    // A brand-new arrival lays the never-received baseline first (scope-planned).
    if !ctx.fs.exists(&ctx.layout.skill_dir(&sid)) {
        let baseline_lock = Lock {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            skill_id: target.skill_id.clone(),
            name: target.name.clone(),
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
            swap_capability: topos_types::persisted::SwapCapability::Unsupported,
            placement_state: Vec::new(),
            harness: None,
            harness_layer: None,
            harness_slug: None,
        };
        let plan = plan_fn(ctx, &target.skill_id, &baseline_lock, &empty);
        if let Err(e) = lay_baseline_with_plan(
            ctx,
            &sid,
            target.name.clone(),
            &plan,
            target.bundle_digest.as_ref(),
        ) {
            note_item_failure(ctx, warnings, &target.name, &e);
            return;
        }
    }
    run.transports
        .plane
        .as_delivery()
        .bind_skill(&run.session.workspace_id, &target.skill_id);
    let run_ctx =
        super::pull::ctx_with_plane_and_follow(ctx, run.transports.plane.as_plane(), follow);
    match sync_engine::sync_one_planned(
        &run_ctx,
        &sid,
        &fc,
        Invocation::Accept,
        Some(&record),
        Some(&plan_fn),
    ) {
        Ok(mut row) => {
            row.workspace_id = Some(run.session.workspace_id.clone());
            // Project placements stay out of commits: one `.git/info/exclude` line per landed dir.
            if let ResolvedScope::Project { dir } = scope {
                exclude_project_placements(ctx, dir, &sid, warnings);
            }
            rows.push(row);
        }
        Err(e) => note_item_failure(ctx, warnings, &target.name, &e),
    }
}

/// Reconcile a GitHub reference: absent → fetch + adopt at the pinned commit; tracked + same pin
/// → verified no-op; tracked + a DIFFERENT pin → refuse to touch local edits, else re-import at
/// the new pin (external bytes have no governance rail — the pin IS the version surface).
#[allow(clippy::too_many_arguments)]
fn reconcile_github(
    ctx: &Ctx<'_>,
    git: Option<&dyn crate::git_source::GitTarballSource>,
    item: &ResolvedItem,
    owner: &str,
    repo: &str,
    subdir: &str,
    rows: &mut Vec<PullSkill>,
    warnings: &mut Vec<String>,
) {
    let origin_source = format!("github.com/{owner}/{repo}");
    // A tracked skill with this origin (name-first, then any-origin-match).
    let tracked = find_tracked_github(ctx, &origin_source, subdir);
    match tracked {
        Some((sid, lock, origin)) => {
            let recorded = origin.commit.as_deref().unwrap_or_default();
            let pin_ok = item
                .pin
                .as_deref()
                .is_none_or(|p| commit_matches(recorded, p));
            if pin_ok {
                rows.push(PullSkill {
                    skill: lock.name,
                    workspace_id: None,
                    observed: 0,
                    applied: 0,
                    action: PullAction::UpToDate,
                    offer: None,
                    conflict: None,
                    merge: None,
                    merge_preview: None,
                });
                return;
            }
            // The pin moved (a teammate bumped topos.toml): re-import at the pinned commit.
            let Some(git) = git else {
                return; // offline caller — the stale copy stands; the next online sweep refreshes
            };
            match refresh_github(ctx, git, &sid, item, owner, repo, subdir) {
                Ok(row) => rows.push(row),
                Err(e) => note_item_failure(ctx, warnings, &item.name, &e),
            }
        }
        None => {
            let Some(git) = git else {
                warnings.push(format!(
                    "NOT_INSTALLED {}: \"{}\" — an external skill this machine has not fetched \
                     yet (network required)",
                    item.source.label(),
                    item.reference
                ));
                return;
            };
            let Some(roots) = discovery_roots(ctx, item) else {
                return;
            };
            let spec = crate::source::RemoteSpec {
                host: crate::source::GitHost::GitHub,
                owner: owner.to_owned(),
                repo: repo.to_owned(),
                git_ref: item.pin.clone(),
                subdir: (!subdir.is_empty()).then(|| subdir.to_owned()),
            };
            let global = matches!(item.scope, ResolvedScope::Person);
            match super::add_remote(
                ctx,
                git,
                &spec,
                &roots,
                &super::AddRemoteOpts {
                    skill: None,
                    harness: None,
                    global,
                },
            ) {
                Ok(_) => rows.push(PullSkill {
                    skill: item.name.clone(),
                    workspace_id: None,
                    observed: 0,
                    applied: 0,
                    action: PullAction::FastForwarded,
                    offer: None,
                    conflict: None,
                    merge: None,
                    merge_preview: None,
                }),
                Err(e) => note_item_failure(ctx, warnings, &item.name, &e),
            }
        }
    }
}

/// Re-import a tracked external skill at a NEW pinned commit: local edits refuse (never
/// overwritten by an import), a clean copy is snapshot-verified, the sidecar record replaced
/// wholesale, and the fresh import lands through the ordinary adopt.
fn refresh_github(
    ctx: &Ctx<'_>,
    git: &dyn crate::git_source::GitTarballSource,
    sid: &SkillId,
    item: &ResolvedItem,
    owner: &str,
    repo: &str,
    subdir: &str,
) -> Result<PullSkill, ClientError> {
    let sp = ctx.layout.published(sid);
    let map: PlacementMap = sync_engine::read_map_required(ctx, &sp)?;
    let scans = placement::scan_placements(ctx, &map)?;
    if scans
        .iter()
        .any(|s| matches!(s.status, placement::ScanStatus::Modified { .. }))
    {
        return Err(ClientError::InvalidArgument(format!(
            "'{}' has local edits ahead of its pinned import — publish them (or `topos update {} \
             --reset`) before the pin refresh",
            item.name, item.name
        )));
    }
    if scans
        .iter()
        .any(|s| matches!(s.status, placement::ScanStatus::Unscannable))
    {
        return Err(ClientError::PlacementUnsupported {
            reason: "a placement of this external skill cannot be read; refusing the pin refresh"
                .into(),
        });
    }
    let Some(roots) = discovery_roots(ctx, item) else {
        return Err(ClientError::InvalidArgument(
            "cannot re-import without $HOME set".into(),
        ));
    };
    let spec = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
        git_ref: item.pin.clone(),
        subdir: (!subdir.is_empty()).then(|| subdir.to_owned()),
    };
    // PREFETCH the new pin's archive and prove it extracts + selects BEFORE any old byte is
    // deleted — a transient fetch failure or a bad archive must leave the old install whole.
    let targz = git.fetch(&spec)?;
    {
        let repo_tree = crate::git_source::extract_tree(&targz)?;
        repo_tree.select(spec.subdir.as_deref(), None, &spec.repo, &spec.label())?;
    }
    // Clean re-import: STASH the recorded placements (clean copies of the OLD pin) and the
    // sidecar record aside — sibling renames, same filesystem — then adopt afresh at the new pin.
    // The install can still fail past the prefetch (an occupied destination, an io fault); a
    // failure RESTORES the stashes, so the valid old import is never lost to a refused new one.
    // External origins carry no local history worth preserving past their pin (the lockfile
    // model: bytes follow the pin) — a SUCCESSFUL swap deletes the stashes.
    let mut stashed: Vec<(std::path::PathBuf, std::path::PathBuf)> = Vec::new();
    let stash_dir = |fs: &dyn crate::fs_seam::FsOps,
                     from: &Path,
                     stashed: &mut Vec<(std::path::PathBuf, std::path::PathBuf)>|
     -> Result<(), ClientError> {
        let name = from
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dir".to_owned());
        let to = from.with_file_name(format!(".topos-refresh-old-{name}"));
        if fs.exists(&to) {
            fs.remove_dir_all(&to)?;
        }
        fs.rename(from, &to)
            .map_err(|e| ClientError::Io(format!("stash {}: {e}", from.display())))?;
        stashed.push((from.to_path_buf(), to));
        Ok(())
    };
    for scan in &scans {
        if matches!(scan.status, placement::ScanStatus::Clean { .. }) && ctx.fs.exists(&scan.dir) {
            stash_dir(ctx.fs, &scan.dir, &mut stashed)?;
        }
    }
    let sidecar_dir = ctx.layout.skill_dir(sid);
    if ctx.fs.exists(&sidecar_dir) {
        stash_dir(ctx.fs, &sidecar_dir, &mut stashed)?;
    }
    let global = matches!(item.scope, ResolvedScope::Person);
    let installed = super::add_remote_fetched(
        ctx,
        &targz,
        &spec,
        &roots,
        &super::AddRemoteOpts {
            skill: None,
            harness: None,
            global,
        },
    );
    let data = match installed {
        Ok(d) => {
            for (_, stash) in &stashed {
                let _ = ctx.fs.remove_dir_all(stash);
            }
            d
        }
        Err(e) => {
            // Restore the old install (best-effort; a restore failure leaves the stash sibling
            // on disk rather than deleting anything).
            for (orig, stash) in stashed.iter().rev() {
                if !ctx.fs.exists(orig) {
                    let _ = ctx.fs.rename(stash, orig);
                }
            }
            return Err(e);
        }
    };
    Ok(PullSkill {
        skill: data.name,
        workspace_id: None,
        observed: 0,
        applied: 0,
        action: PullAction::FastForwarded,
        offer: None,
        conflict: None,
        merge: None,
        merge_preview: None,
    })
}

/// The discovery roots an external install resolves its destination against — project scope roots
/// at the demanding checkout (the import lands in-project), person scope at the machine cwd.
fn discovery_roots(ctx: &Ctx<'_>, item: &ResolvedItem) -> Option<super::DiscoveryRoots> {
    let roots = ctx.roots.as_ref()?;
    let cwd = match &item.scope {
        ResolvedScope::Project { dir } => Some(dir.clone()),
        ResolvedScope::Person => roots.cwd.clone(),
    };
    Some(super::DiscoveryRoots {
        home: roots.home.clone(),
        cwd,
    })
}

/// Find a tracked skill imported from `origin_source` (+ subdir), by walking the sidecar's origin
/// docs. Best-effort: unreadable entries are skipped.
fn find_tracked_github(
    ctx: &Ctx<'_>,
    origin_source: &str,
    subdir: &str,
) -> Option<(SkillId, Lock, topos_types::results::SkillOrigin)> {
    let entries = ctx.fs.read_dir(&ctx.layout.skills_dir()).ok()?;
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
        let subdir_matches = match &origin.origin.subdir {
            Some(s) => s == subdir,
            None => subdir.is_empty(),
        };
        if origin.origin.source == origin_source && subdir_matches {
            let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &sp.lock) else {
                continue;
            };
            return Some((sid, lock, origin.origin));
        }
    }
    None
}

/// Whether a recorded commit satisfies a manifest pin (git-style prefix match, either direction).
fn commit_matches(recorded: &str, pin: &str) -> bool {
    !recorded.is_empty() && (recorded.starts_with(pin) || pin.starts_with(recorded))
}

/// The explicit `[placement]` override for `item` from its OWN project manifest: a per-reference
/// pin first, else the per-kind (`skill`) pin. Project layers only. The value must be a RELATIVE
/// path that stays inside the project (no `..`, not absolute) — a committed manifest must never be
/// able to aim managed bytes outside its own checkout; a hostile/mistaken value is ignored with a
/// warning and the default placement engages.
fn placement_override(
    project_manifests: &[(PathBuf, crate::manifest::file::Manifest)],
    item: &ResolvedItem,
    warnings: &mut Vec<String>,
) -> Option<String> {
    let LayerSource::Project { dir } = &item.source else {
        return None;
    };
    let (_, manifest) = project_manifests.iter().find(|(d, _)| d == dir)?;
    let raw = manifest
        .placement
        .iter()
        .find(|(r, _)| {
            r == &item.reference
                || crate::manifest::refs::parse_ref(r)
                    .map(|p| p.item_name() == item.name)
                    .unwrap_or(false)
        })
        .map(|(_, d)| d.clone())
        .or_else(|| {
            manifest
                .placement_kind
                .iter()
                .find(|(k, _)| k == "skill")
                .map(|(_, d)| d.clone())
        })?;
    if crate::placement::safe_project_rel(&raw) {
        Some(raw)
    } else {
        warnings.push(format!(
            "PLACEMENT_OVERRIDE_IGNORED {}: the [placement] value {raw:?} must be a relative              path inside the project (no `..`, not absolute) — using the default placement",
            item.name
        ));
        None
    }
}

/// Keep a project-scope skill's landed dirs out of commits: one `.git/info/exclude` line per
/// placement under the project root (committed ignore files are NEVER touched). Best-effort — a
/// project without `.git` simply has nothing to exclude from.
fn exclude_project_placements(
    ctx: &Ctx<'_>,
    project_dir: &Path,
    sid: &SkillId,
    warnings: &mut Vec<String>,
) {
    let sp = ctx.layout.published(sid);
    let Ok(Some(map)) = doc::read_map(ctx.fs, &sp.map) else {
        return;
    };
    let rels: Vec<String> = map
        .placements
        .iter()
        .filter_map(|p| {
            Path::new(p)
                .strip_prefix(project_dir)
                .ok()
                .map(|rel| format!("/{}/", rel.display()))
        })
        .collect();
    if rels.is_empty() {
        return;
    }
    if let Err(e) = ensure_git_exclude(ctx, project_dir, &rels) {
        warnings.push(format!(
            "GIT_EXCLUDE_FAILED {}: {}",
            project_dir.display(),
            e.detail()
        ));
    }
}

/// Append missing lines to the repo's `.git/info/exclude` (creating it if needed). Resolves a
/// worktree/submodule `.git` FILE through its `gitdir:` pointer (and a worktree's `commondir`),
/// so the exclude lands where git actually reads it. Idempotent.
pub(crate) fn ensure_git_exclude(
    ctx: &Ctx<'_>,
    project_dir: &Path,
    lines: &[String],
) -> Result<(), ClientError> {
    let Some(git_dir) = resolve_git_dir(ctx, project_dir) else {
        return Ok(()); // not a git repo — nothing travels, nothing to exclude
    };
    let exclude = git_dir.join("info/exclude");
    let existing = ctx
        .fs
        .read_opt(&exclude)?
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_default();
    let present: HashSet<&str> = existing.lines().map(str::trim).collect();
    let missing: Vec<&String> = lines
        .iter()
        .filter(|l| !present.contains(l.trim()))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    let mut next = existing;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    if !next.contains("# topos-managed skills") {
        next.push_str("# topos-managed skills (placed by `topos update`; not committed)\n");
    }
    for l in missing {
        next.push_str(l);
        next.push('\n');
    }
    ctx.fs.create_dir_all(&git_dir.join("info"))?;
    crate::atomic::atomic_write(ctx.fs, &exclude, next.as_bytes())
}

/// The git dir whose `info/exclude` covers `project_dir`: `.git` as a dir directly; a `.git` FILE
/// (worktree / submodule) through `gitdir:`, then a worktree's `commondir` indirection.
fn resolve_git_dir(ctx: &Ctx<'_>, project_dir: &Path) -> Option<PathBuf> {
    let dot_git = project_dir.join(".git");
    match ctx.fs.path_kind(&dot_git).ok()? {
        Some(crate::fs_seam::PathKind::Dir) => Some(dot_git),
        Some(_) => {
            let content = ctx.fs.read_opt(&dot_git).ok()??;
            let text = String::from_utf8_lossy(&content);
            let gitdir = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("gitdir:"))?
                .trim();
            let gitdir = if Path::new(gitdir).is_absolute() {
                PathBuf::from(gitdir)
            } else {
                project_dir.join(gitdir)
            };
            // A linked worktree's exclude lives in the COMMON dir.
            if let Ok(Some(common)) = ctx.fs.read_opt(&gitdir.join("commondir")) {
                let common = String::from_utf8_lossy(&common).trim().to_owned();
                let common_path = if Path::new(&common).is_absolute() {
                    PathBuf::from(common)
                } else {
                    gitdir.join(common)
                };
                return Some(common_path);
            }
            Some(gitdir)
        }
        None => None,
    }
}

/// Clean what nothing demands any more:
/// - a PROFILE-dropped skill (in the prior cache, absent from today's delivery, resolved by no
///   manifest): snapshot any draft, clean its NON-project placements, reset to never-received;
/// - a PROJECT-dropped skill (placements under this cwd's project chain that today's resolution
///   did not manage): snapshot-first clean of exactly those dirs.
#[allow(clippy::too_many_arguments)]
fn clean_undemanded(
    ctx: &Ctx<'_>,
    runs: &[SessionRun],
    prior_sync: &sync_status::SyncStatus,
    resolution: &Resolution,
    project_dirs: &[PathBuf],
    synced_ids: &HashSet<String>,
    rows: &mut Vec<PullSkill>,
    warnings: &mut Vec<String>,
) {
    // Profile-dropped: prior cache vs today's delivery.
    for run in runs {
        let Some(snap) = &run.snapshot else { continue };
        let now: HashSet<&str> = snap.skills.iter().map(|s| s.skill_id.as_str()).collect();
        let Some(prior) = prior_sync.workspaces.get(&run.session.workspace_id) else {
            continue;
        };
        for (skill_id, cached) in &prior.delivered {
            if cached.withdrawn || now.contains(skill_id.as_str()) || synced_ids.contains(skill_id)
            {
                continue;
            }
            let Ok(sid) = SkillId::parse(skill_id) else {
                continue;
            };
            if !ctx.fs.exists(&ctx.layout.skill_dir(&sid)) {
                continue;
            }
            match withdraw_person_scope(ctx, &sid) {
                Ok(name) => rows.push(PullSkill {
                    skill: name,
                    workspace_id: Some(run.session.workspace_id.clone()),
                    observed: 0,
                    applied: 0,
                    action: PullAction::Withdrawn,
                    offer: None,
                    conflict: None,
                    merge: None,
                    merge_preview: None,
                }),
                Err(e) => note_item_failure(ctx, warnings, skill_id, &e),
            }
        }
    }

    // Project-dropped: recorded placements under this cwd's chain the resolution didn't manage.
    if project_dirs.is_empty() {
        return;
    }
    let resolved_project_names: HashSet<(&Path, &str)> = resolution
        .items
        .iter()
        .filter_map(|i| match &i.scope {
            ResolvedScope::Project { dir } => Some((dir.as_path(), i.name.as_str())),
            ResolvedScope::Person => None,
        })
        .collect();
    let Ok(entries) = ctx.fs.read_dir(&ctx.layout.skills_dir()) else {
        return;
    };
    for entry in entries {
        let Some(id) = entry.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        // A skill THIS run reconciled is demanded by construction (incl. the channel-expanded
        // items, which carry the channel's name in the resolution, not their own).
        if synced_ids.contains(id) {
            continue;
        }
        let Ok(sid) = SkillId::parse(id) else {
            continue;
        };
        let sp = ctx.layout.published(&sid);
        let Ok(Some(lock)) = doc::read_doc::<Lock>(ctx.fs, &sp.lock) else {
            continue;
        };
        let Ok(Some(map)) = doc::read_map(ctx.fs, &sp.map) else {
            continue;
        };
        let stale: Vec<usize> = map
            .placements
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                project_dirs.iter().any(|pd| {
                    Path::new(p).starts_with(pd)
                        && !resolved_project_names.contains(&(pd.as_path(), lock.name.as_str()))
                })
            })
            .map(|(i, _)| i)
            .collect();
        if stale.is_empty() {
            continue;
        }
        let cleaned = crate::sidecar::lock_skill(ctx.fs, &ctx.layout, &sid)
            .and_then(|_guard| clean_placements(ctx, &sid, &lock, &map, &stale));
        if let Err(e) = cleaned {
            note_item_failure(ctx, warnings, &lock.name, &e);
        }
    }
}

/// A profile-dropped skill leaves the PERSON scope: snapshot every edited copy, clean the
/// placements that are NOT inside some project checkout (a project manifest may still demand it
/// there — that checkout reconciles lazily when visited), keep every sidecar byte, and reset the
/// sync doc to never-received so a later re-delivery reinstalls. Returns the catalog name.
fn withdraw_person_scope(ctx: &Ctx<'_>, sid: &SkillId) -> Result<String, ClientError> {
    let sp = ctx.layout.published(sid);
    let name;
    {
        // The guard is scoped: `reset_to_never_received` below takes the SAME per-skill flock on
        // a fresh fd, which would deadlock against a still-held one.
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

/// Whether a placement dir belongs to some PROJECT checkout — an ancestor holds a `topos.toml`
/// (the manifest travels with the repo; its placements are that scope's business). The ONE
/// heuristic, shared with the person plan's prior-stability rule.
fn is_project_placement(ctx: &Ctx<'_>, dir: &Path) -> bool {
    crate::placement::under_project_manifest(ctx, dir)
}

/// Snapshot-first clean of exactly `indices` placements: every distinct edited copy is committed
/// into the sidecar store BEFORE any dir is removed; Foreign dirs are never touched; the cleaned
/// dirs leave the placement record (demand ended — the explicit act was the manifest/profile
/// edit). Fails closed on an unscannable placement.
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
            ctx.fs.remove_dir_all(p)?;
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

/// The session a `(host, workspace)` reference resolves through: an exact `(host, ws)` match when
/// the host is spelled; a host-less `@ws/…` matches by workspace name alone.
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

/// The honest "not available" line for a workspace ref with no session — phrased from LOCAL
/// knowledge (which manifest asked; which login is missing), never a server answer.
fn not_connected_line(item: &ResolvedItem, host: Option<&str>, workspace: &str) -> String {
    let address = match host {
        Some(h) => format!("{h}/{workspace}"),
        None => workspace.to_owned(),
    };
    format!(
        "NOT_AVAILABLE {}: \"{}\" — referenced here, but this installation is not logged into \
         {address} (run `topos login {address}`)",
        item.source.label(),
        item.reference
    )
}

/// A skill's catalog name from its sidecar lock (offline).
fn skill_name_of(ctx: &Ctx<'_>, skill_id: &str) -> Option<String> {
    let sid = SkillId::parse(skill_id).ok()?;
    doc::read_doc::<Lock>(ctx.fs, &ctx.layout.published(&sid).lock)
        .ok()
        .flatten()
        .map(|l| l.name)
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
// The never-received baseline (moved here from the retired follow verb — the reconcile's scaffold
// for a brand-new arrival's first receive).
// =================================================================================================

/// The all-zero sentinel a first-receive baseline carries.
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// The genesis generation sentinel.
const GENESIS: u64 = 0;

// =================================================================================================
// The never-received baseline — the sidecar scaffold a brand-new arrival's first receive lands into.
// =================================================================================================

/// [`lay_first_receive_baseline`] with the placement plan already computed — the manifest
/// reconcile's entry for PROJECT-scope arrivals (their targets root at the demanding checkout,
/// not the home harness dirs).
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
