//! `pull` — the session-start currency entry point + the targeted accept / go-back.
//!
//! The bare `topos pull` (the installed session-start hook) sweeps every followed skill toward its
//! `current`. A targeted `topos pull <skill>` accepts a pending update for one skill (the explicit
//! command supplies the consent a confirm-each offer solicited); `topos pull <skill>@<hash>` goes back to
//! a specific local version. The per-skill engine (check → plan → apply) lives in
//! [`super::sync_engine`]; this module is the scope dispatch + aggregation.
//!
//! In production the follow-state comes from the enrollment docs (`follows.json`, written by `follow`)
//! and the plane reads ride the real HTTP transport — this is exactly what the installed hook runs. With
//! nothing followed the sweep reports an honestly empty state. The tests drive the same engine over
//! fixture sources (no HTTP).
//!
//! **The sweep degrades fast when the plane is down.** The first connect-level failure trips a
//! per-invocation circuit breaker ([`BreakerPlane`]): every remaining plane call in this pull
//! short-circuits to an unreachable error the engine already maps to a local-state-only outcome, so a
//! dead plane costs ONE connect timeout, not one per followed skill — the session-start hook must never
//! hang the harness.

use std::cell::Cell;
use std::collections::HashSet;
use std::path::Path;

use topos_core::digest::to_hex;
use topos_types::persisted::{Lock, PlacementMap, SyncState};
use topos_types::results::{PullAction, PullData, PullSkill};
use topos_types::{CurrentRecord, PointerScope, WIRE_SCHEMA_VERSION, WireCurrentRecord};

use crate::ctx::Ctx;
use crate::enroll::{self, FollowEntry, FollowModeDoc};
use crate::error::ClientError;
use crate::id::SkillId;
use crate::plane::{
    DeliverySkill, DeliverySource, FetchedVersion, FollowContext, FollowMode, KnownCurrent,
    PlaneError, PlaneSource, PointerFetch,
};
use crate::{doc, sidecar};

use super::sync_engine::{self, WorkState};

/// The never-received sentinel the first-receive baseline carries (and an upstream withdrawal
/// restores, so a later re-delivery installs afresh instead of reading as already-current).
const ZERO_GEN: topos_types::Generation = topos_types::Generation { epoch: 0, seq: 0 };
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// What a `pull` invocation targets.
#[derive(Debug)]
pub(crate) enum PullScope {
    /// The bare session-start sweep — every followed skill.
    AllFollowed,
    /// One skill, by name, in a targeted mode.
    One { name: String, mode: TargetMode },
}

/// How a targeted single-skill pull behaves.
#[derive(Debug)]
pub(crate) enum TargetMode {
    /// `topos pull <skill>` — accept a pending update / resume a held skill / resolve a divergence (no `@hash`).
    AcceptPending,
    /// `topos pull <skill> --onto-current` — the disclosed escape: commit MY bytes on top of `current`,
    /// dropping the merge (a 2-way diff of what is dropped is surfaced). Resolves a divergence without merging.
    OntoCurrent,
    /// `topos pull <skill>@<ref>` — install an older version's bytes locally (a deliberate go-back).
    /// The ref is the full 64-hex id or a short prefix, resolved against the skill's recorded history
    /// inside [`sync_engine::go_back`] (where that history is already loaded and validated).
    GoBack(super::VersionRef),
}

/// A `pull` run's typed result: the per-skill rows PLUS the per-skill hard failures the sweep isolated.
/// `data` is the schema-pinned envelope payload; `warnings` ride the envelope's existing `warnings` field
/// (one stable-shape line per failed skill), so an isolated failure is machine-visible under `--json`
/// instead of stderr-only.
pub(crate) struct PullOutcome {
    pub data: PullData,
    pub warnings: Vec<String>,
}

/// Run the currency check for `scope`.
///
/// # Errors
/// A hard failure resolving a targeted skill, or (for a targeted pull) a plane-read failure; the bare
/// sweep isolates per-skill failures instead of erroring (each becomes a warning + a stderr line).
pub(crate) fn pull(ctx: &Ctx<'_>, scope: PullScope) -> Result<PullOutcome, ClientError> {
    match scope {
        PullScope::AllFollowed => {
            // The sweep runs through the circuit breaker: the first connect-level failure marks the
            // plane down for the REST of this invocation (including the proposals count below).
            let breaker = BreakerPlane::new(ctx.plane);
            let sweep_ctx = ctx_with_plane(ctx, &breaker);
            let mut skills = Vec::new();
            let mut warnings = Vec::new();
            for (skill_id, follow) in ctx.follow.followed() {
                if !follow.following {
                    continue;
                }
                // The followed id enters path joins below — parse it like any other boundary id. The
                // enrollment loader already refused a corrupt follows.json, so this only fires for an
                // id that bypassed that load (a fixture / a future source); it is isolated like any
                // other per-skill failure, never a landed path escape.
                let sid = match SkillId::parse(&skill_id) {
                    Ok(sid) => sid,
                    Err(e) => {
                        note_skill_failure(ctx, &mut warnings, &skill_id, &e);
                        continue;
                    }
                };
                match sync_engine::sync_one(
                    &sweep_ctx,
                    &sid,
                    &follow,
                    sync_engine::Invocation::Sweep,
                ) {
                    // Stamp the row's workspace provenance from the skill's OWN follow entry — the sweep
                    // spans skills across every followed workspace, so two same-named skills stay
                    // distinguishable in the `--json` rows.
                    Ok(mut row) => {
                        row.workspace_id = Some(follow.workspace_id.clone());
                        skills.push(row);
                    }
                    // A hard per-skill failure (corrupt docs, store/io) must not abort the whole sweep —
                    // disclose it (stderr + a typed warning; never stdout, which the hook injects) and
                    // leave that skill put.
                    Err(e) => note_skill_failure(ctx, &mut warnings, &skill_id, &e),
                }
            }
            // The proposals count runs AFTER the sweep (it is disclosure, not currency) and is skipped
            // entirely once the breaker tripped — no point burning more connect timeouts on it.
            let proposals_awaiting = if breaker.tripped() {
                0
            } else {
                sum_open_proposals(&sweep_ctx)
            };
            Ok(PullOutcome {
                data: PullData {
                    skills,
                    proposals_awaiting,
                },
                warnings,
            })
        }
        PullScope::One { name, mode } => {
            let (skill_id, _lock) = super::resolve_skill(ctx, &name)?;
            // The go-back and the `--onto-current` escape are documented plane-independent (the escape is
            // the offline no-deadlock guarantee) — neither spends a network call on the proposals count.
            let plane_independent = matches!(mode, TargetMode::GoBack(_) | TargetMode::OntoCurrent);
            let mut row = match mode {
                TargetMode::GoBack(vref) => sync_engine::go_back(ctx, &skill_id, &vref)?,
                TargetMode::AcceptPending | TargetMode::OntoCurrent => {
                    let inv = match mode {
                        TargetMode::OntoCurrent => sync_engine::Invocation::Escape,
                        _ => sync_engine::Invocation::Accept,
                    };
                    match ctx
                        .follow
                        .followed()
                        .into_iter()
                        .find(|(id, _)| *id == *skill_id.as_str())
                    {
                        Some((_, follow)) if follow.following => {
                            sync_engine::sync_one(ctx, &skill_id, &follow, inv)?
                        }
                        // Tracked but not followed → there is no `current` to pull; report the local state.
                        _ => sync_engine::current_state(ctx, &skill_id)?,
                    }
                }
            };
            // Stamp the row's workspace provenance from the follow-state (a retained-but-paused entry still
            // resolves; a purely local go-back / tracked-only skill is honestly `None`).
            row.workspace_id = super::followed_workspace(ctx, skill_id.as_str());
            let proposals_awaiting = if plane_independent {
                0
            } else {
                sum_open_proposals(ctx)
            };
            Ok(PullOutcome {
                data: PullData {
                    skills: vec![row],
                    proposals_awaiting,
                },
                warnings: Vec::new(),
            })
        }
    }
}

/// The DELIVERY-DRIVEN sweep — the reconcile engine: one delivery call per enrolled workspace
/// answers "what should this device have", and the client converges on it. Replaces the bare
/// sweep's per-skill pointer fan-out when a delivery transport is available (an enrolled install);
/// the targeted single-skill paths keep [`pull`].
///
/// Per workspace: **install** delivered skills this device has never followed (a first-receive
/// baseline + the kernel's I-TOFU offer — following a channel is standing consent, but a brand-new
/// skill's first bytes are still disclosed, never silently landed), **update** the known ones
/// (feeding the engine the already-resolved target — no second pointer GET), and classify every
/// followed-but-undelivered skill by WHO ACTED: in the snapshot's `detached` set → the PERSON
/// detached it (an unfollow / a lapsed channel leave) → freeze in place, bytes untouched; absent
/// otherwise → UPSTREAM withdrew it (archived / its last delivering channel dropped it) → snapshot
/// any draft, CLEAN the agent dirs, keep the sidecar bytes ("keep it as yours" is one narration
/// away). A whole-workspace miss (removed from the roster / a revoked device) freezes EVERYTHING
/// with a warning — never a clean (the copies are yours; re-adding re-enables). Afterwards each
/// workspace gets this device's applied-state report — best-effort fleet visibility, never a sync
/// blocker.
pub(crate) fn pull_reconcile(
    ctx: &Ctx<'_>,
    delivery: &dyn DeliverySource,
) -> Result<PullOutcome, ClientError> {
    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    let mut proposals_awaiting: u32 = 0;

    // The follow-state snapshot this sweep classifies against (per-skill mutations below re-read
    // under the identity lock, so a stale entry here costs at most one extra row next sweep).
    let followed = ctx.follow.followed();

    for ws in delivery.workspaces() {
        let snapshot = match delivery.fetch_delivery(&ws) {
            Ok(s) => s,
            Err(PlaneError::NotFound) => {
                // The whole workspace is gone for THIS device (removed from the roster, revoked,
                // or the workspace itself is no more). The who-acts principle: an upstream person
                // acted on YOU — every copy stays, frozen; nothing is cleaned.
                warnings.push(format!(
                    "ACCESS_GONE {ws}: this device no longer has access; its skills stay frozen in \
                     place (re-adding the member re-enables them)"
                ));
                continue;
            }
            Err(PlaneError::Unreachable(m) | PlaneError::Unavailable(m)) => {
                // Transient: keep state, retry next session — the hook must never block a session.
                warnings.push(format!("PLANE_UNAVAILABLE {ws}: {m}"));
                continue;
            }
            Err(PlaneError::Malformed(m)) => {
                warnings.push(format!("WIRE_INVALID {ws}: {m}"));
                continue;
            }
        };
        proposals_awaiting = proposals_awaiting
            .saturating_add(u32::try_from(snapshot.proposals_awaiting).unwrap_or(u32::MAX));

        let mut delivered_ids: HashSet<&str> = HashSet::with_capacity(snapshot.skills.len());
        for ds in &snapshot.skills {
            delivered_ids.insert(ds.skill_id.as_str());
            // Teach the READ transport this skill's credential before anything fetches its bytes.
            // The per-skill credential map is derived from `follows.json`, which cannot yet name a
            // brand-new arrival — without this, the arrival's first version fetch answers
            // "not served" and the offer is lost for a session.
            delivery.bind_skill(&ws, &ds.skill_id);
            let entry = followed.iter().find(|(id, _)| *id == ds.skill_id);
            let row = match entry {
                // An entry the LOCAL `unfollow` verb paused, which the plane still delivers: respect
                // the local pause (that verb is byte-inert and offline-capable, and its server half
                // — the person-scoped unfollow row — is the verb-reshape increment's; until then a
                // local pause is honoured here and `follow <skill>` resumes it, exactly as before).
                // Nothing this sweep does ever writes `following = false`, so this arm only ever
                // sees a deliberate local pause — a server-side detach freezes by touching nothing.
                Some((_, f)) if !f.following => continue,
                Some((_, f)) => sync_delivered(ctx, &ws, ds, f),
                None => install_new_arrival(ctx, &ws, ds),
            };
            match row {
                Ok(mut row) => {
                    row.workspace_id = Some(ws.clone());
                    skills.push(row);
                }
                Err(e) => note_skill_failure(ctx, &mut warnings, &ds.skill_id, &e),
            }
        }

        // The undelivered remainder: who acted decides the on-disk consequence.
        let detached: HashSet<&str> = snapshot.detached.iter().map(String::as_str).collect();
        for (skill_id, follow) in followed.iter().filter(|(id, f)| {
            f.workspace_id == ws && f.following && !delivered_ids.contains(id.as_str())
        }) {
            let _ = follow;
            let sid = match SkillId::parse(skill_id) {
                Ok(sid) => sid,
                Err(e) => {
                    note_skill_failure(ctx, &mut warnings, skill_id, &e);
                    continue;
                }
            };
            let row = if detached.contains(skill_id.as_str()) {
                freeze_detached(ctx, &sid)
            } else {
                withdraw_upstream(ctx, &sid)
            };
            match row {
                Ok(mut row) => {
                    row.workspace_id = Some(ws.clone());
                    skills.push(row);
                }
                Err(e) => note_skill_failure(ctx, &mut warnings, skill_id, &e),
            }
        }

        // The applied-state report — the fleet page's truth, POST-reconcile (the acceptance bar:
        // after an update, the fleet reflects applied versions). Scoped to the DELIVERED set: a
        // skill this sweep just withdrew (or froze) must not be reported as held — reporting it
        // would re-create a live fleet row for bytes the device no longer serves, and would revive
        // a detach record the plane is deliberately holding. Best-effort: a failure warns.
        match applied_snapshot(ctx, &delivered_ids) {
            Ok(applied) => {
                if let Err(e) = delivery.report_applied(&ws, &applied) {
                    let m = match e {
                        PlaneError::NotFound => "access gone".to_owned(),
                        PlaneError::Unreachable(m)
                        | PlaneError::Unavailable(m)
                        | PlaneError::Malformed(m) => m,
                    };
                    warnings.push(format!("REPORT_FAILED {ws}: {m}"));
                }
            }
            Err(e) => warnings.push(format!("REPORT_FAILED {ws}: {}", e.detail())),
        }
    }

    Ok(PullOutcome {
        data: PullData {
            skills,
            proposals_awaiting,
        },
        warnings,
    })
}

/// Sync one already-followed delivered skill, feeding the engine the delivery's resolved target
/// (no second pointer GET) and the FRESH per-bundle protection posture (the stored doc's flag may
/// lag; the runtime context never does).
fn sync_delivered(
    ctx: &Ctx<'_>,
    ws: &str,
    ds: &DeliverySkill,
    follow: &FollowContext,
) -> Result<PullSkill, ClientError> {
    let sid = SkillId::parse(&ds.skill_id)?;
    let follow = FollowContext {
        review_required: ds.review_required,
        ..follow.clone()
    };
    let rec = delivered_record(ws, ds);
    sync_engine::sync_one_with(
        ctx,
        &sid,
        &follow,
        sync_engine::Invocation::Sweep,
        Some(&rec),
    )
}

/// A brand-new delivered skill this device has never held: record the follow entry (person-scoped
/// truth lives on the plane; the local entry is install-state), lay the first-receive baseline
/// under the skill's CATALOG name, and run the engine — whose kernel consent OFFERS the first
/// bytes (I-TOFU), never silently lands them.
fn install_new_arrival(
    ctx: &Ctx<'_>,
    ws: &str,
    ds: &DeliverySkill,
) -> Result<PullSkill, ClientError> {
    let sid = SkillId::parse(&ds.skill_id)?;
    // The BASELINE FIRST, the follow entry second — the same ordering `follow`'s promote uses. A
    // crash between them leaves a baseline with no entry, which the NEXT reconcile treats as a
    // fresh arrival again (the baseline layer is idempotent: it returns early if the skill dir
    // exists). The reverse order would leave an entry whose sidecar docs do not exist, and every
    // later sweep would fail that skill on a missing-doc read — a permanent wedge.
    super::follow::lay_first_receive_baseline(ctx, &sid, ds.name.clone(), ws)?;
    enroll::write_follows_merged(
        ctx.fs,
        &ctx.layout,
        &[FollowEntry {
            skill_id: ds.skill_id.clone(),
            workspace_id: ws.to_owned(),
            mode: FollowModeDoc::Auto,
            review_required: ds.review_required,
            following: true,
        }],
    )?;
    let follow = FollowContext {
        workspace_id: ws.to_owned(),
        mode: FollowMode::Auto,
        review_required: ds.review_required,
        following: true,
    };
    let rec = delivered_record(ws, ds);
    sync_engine::sync_one_with(
        ctx,
        &sid,
        &follow,
        sync_engine::Invocation::Sweep,
        Some(&rec),
    )
}

/// The wire pointer record a delivery row resolves to — the engine's sync target (scope-checked
/// there like any served pointer; integrity stays the content-addressed version id, re-verified by
/// digest on apply).
fn delivered_record(ws: &str, ds: &DeliverySkill) -> WireCurrentRecord {
    WireCurrentRecord {
        schema_version: WIRE_SCHEMA_VERSION,
        scope: PointerScope {
            workspace_id: ws.to_owned(),
            skill_id: ds.skill_id.clone(),
        },
        record: CurrentRecord {
            version_id: to_hex(&ds.version_id),
            generation: ds.generation,
        },
    }
}

/// The PERSON detached this skill (an unfollow, or a channel leave that lapsed it) — freeze in
/// place: delivery ends, every byte stays exactly where it is, `follow` re-attaches.
fn freeze_detached(ctx: &Ctx<'_>, sid: &SkillId) -> Result<PullSkill, ClientError> {
    let sync = read_sync(ctx, sid)?;
    // NOTE: no follow-state flip either. The person's subscription rows on the plane ARE the truth
    // (that is the whole point of person-scoped subscriptions); flipping the local flag would make
    // the freeze STICKY — a later re-follow (from any device, or the web) re-delivers the skill,
    // and this device must resume. Freezing is simply: touch nothing. The plane keeps the skill out
    // of `skills[]` for as long as the detachment stands, so every subsequent sweep re-freezes it
    // (a no-op), and the sweep that finally sees it delivered again syncs it.
    Ok(undelivered_row(sid, sync.as_ref(), PullAction::Detached))
}

/// UPSTREAM withdrew this skill (archived, or its last delivering channel dropped it) — managed
/// distribution cleans what it managed: snapshot any draft into the sidecar store first (nothing
/// is ever lost), remove the agent dirs, keep every sidecar byte, and freeze the entry. Idempotent
/// across a crash at any step (a re-run re-snapshots nothing, re-removes nothing, re-flips
/// nothing).
fn withdraw_upstream(ctx: &Ctx<'_>, sid: &SkillId) -> Result<PullSkill, ClientError> {
    let sp = ctx.layout.published(sid);
    let sync: Option<SyncState>;
    {
        let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
        sync = doc::read_doc(ctx.fs, &sp.sync)?;
        let lock: Option<Lock> = doc::read_doc(ctx.fs, &sp.lock)?;
        let map: Option<PlacementMap> = doc::read_doc(ctx.fs, &sp.map)?;
        if let (Some(lock), Some(map)) = (lock.as_ref(), map.as_ref()) {
            match sync_engine::compute_work(ctx, map, lock)? {
                // A draft delta is retained in the sidecar store BEFORE any byte leaves the agent
                // dir — nothing is ever lost.
                WorkState::Present {
                    eq_base: false,
                    scanned,
                    ..
                } => {
                    sync_engine::snapshot_draft(ctx, &sp, lock, &scanned)?;
                }
                // Pristine (or absent) — nothing local to retain beyond what the store already has.
                WorkState::Present { .. } | WorkState::Absent => {}
                // UNSCANNABLE — the placement exists but cannot be read safely (a symlink/device
                // node/non-UTF-8 name someone put there). We CANNOT snapshot what we cannot scan, so
                // we must not delete it: FAIL CLOSED, exactly as the sync engine refuses to
                // fast-forward over an unreadable placement. The skill freezes with a typed refusal
                // instead of a silent, unrecoverable delete.
                WorkState::Unscannable => {
                    return Err(ClientError::PlacementUnsupported {
                        reason: "upstream withdrew this skill, but its placement cannot be read; \
                                 refusing to remove it — inspect or move the directory by hand"
                            .into(),
                    });
                }
            }
            for placement in &map.placements {
                let p = Path::new(placement);
                if ctx.fs.exists(p) {
                    ctx.fs.remove_dir_all(p)?;
                }
            }
        }
    }
    // NOTE: no follow-state flip. The entry stays LIVE (a withdrawal is a delivery change, not a
    // subscription change): the sidecar keeps every byte + any draft, and the sync state is reset to
    // the NEVER-RECEIVED baseline — the same all-zero sentinel `follow` lays. That reset is what
    // makes a later re-delivery (a curator re-places the skill, an owner unarchives it) reinstall:
    // without it, `applied == observed` and an absent placement read as "already current", and the
    // skill would never come back. Re-arrival then passes the kernel's I-TOFU first-receive consent
    // — an offer, disclosed, exactly as the original arrival was.
    if let Some(prior) = sync.as_ref() {
        let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
        doc::write_doc(
            ctx.fs,
            &sp.sync,
            &SyncState {
                schema_version: prior.schema_version,
                observed: ZERO_GEN,
                observed_version_id: ZERO_HEX.to_owned(),
                applied: ZERO_GEN,
                base_commit: ZERO_HEX.to_owned(),
                work_hash: ZERO_HEX.to_owned(),
                held: false,
            },
        )?;
    }
    Ok(undelivered_row(sid, sync.as_ref(), PullAction::Withdrawn))
}

/// A row for a skill the sweep did NOT sync (frozen / withdrawn): last-known generations, no offer.
fn undelivered_row(sid: &SkillId, sync: Option<&SyncState>, action: PullAction) -> PullSkill {
    let (observed, applied) = sync.map_or(
        (
            topos_types::Generation { epoch: 0, seq: 0 },
            topos_types::Generation { epoch: 0, seq: 0 },
        ),
        |s| (s.observed, s.applied),
    );
    PullSkill {
        skill: sid.as_str().to_owned(),
        workspace_id: None,
        observed,
        applied,
        action,
        offer: None,
        conflict: None,
        merge: None,
    }
}

fn read_sync(ctx: &Ctx<'_>, sid: &SkillId) -> Result<Option<SyncState>, ClientError> {
    let sp = ctx.layout.published(sid);
    doc::read_doc(ctx.fs, &sp.sync)
}

/// What this device HOLDS after the reconcile, over the skills the delivery actually DELIVERED:
/// the materialized version from `map.json` (the honest "applied" — an offered-but-unaccepted first
/// receive has none and is skipped, as is any skill whose placement this sweep removed). Read-only.
///
/// Scoping to the delivered set is load-bearing: reporting a withdrawn or frozen skill would tell
/// the fleet page this device still serves bytes it does not, and would revive the very detach
/// record the plane wrote.
fn applied_snapshot(
    ctx: &Ctx<'_>,
    delivered: &HashSet<&str>,
) -> Result<Vec<(String, [u8; 32])>, ClientError> {
    let mut out = Vec::new();
    for skill_id in delivered {
        let Ok(sid) = SkillId::parse(skill_id) else {
            continue;
        };
        let sp = ctx.layout.published(&sid);
        let Some(map) = doc::read_doc::<PlacementMap>(ctx.fs, &sp.map)? else {
            continue;
        };
        // A placement the sweep removed (or never laid) is not held, whatever the doc says.
        if !map.placements.iter().any(|p| ctx.fs.exists(Path::new(p))) {
            continue;
        }
        if let Ok(commit) = super::parse_hex32(&map.applied_commit)
            && commit != [0u8; 32]
        {
            out.push(((*skill_id).to_owned(), commit));
        }
    }
    Ok(out)
}

/// One isolated per-skill failure as a stable, machine-parseable envelope warning:
/// `<CODE> <skill_id>: <safe message>` (the same code/safe-message pair the error envelope would carry;
/// the skill id here came from the follow-state, never a secret).
fn skill_warning(skill_id: &str, e: &ClientError) -> String {
    format!(
        "{} {skill_id}: {}",
        e.code(),
        crate::render::safe_message(e)
    )
}

/// Disclose one isolated per-skill sweep failure under the same redaction policy as the top-level error
/// path: the SAFE message on stderr (the hook surface — never stdout), the FULL `Display` chain to the
/// append-only diagnostics log (best-effort), and a stable-shape envelope warning.
fn note_skill_failure(ctx: &Ctx<'_>, warnings: &mut Vec<String>, skill_id: &str, e: &ClientError) {
    let _ = crate::logfile::append_error_event(
        ctx.fs,
        &ctx.layout.log_path(),
        "pull",
        e.code(),
        &format!("skill {skill_id}: {}", e.detail()),
        // First-class, so `topos log <skill>`'s skill_id filter surfaces the wedged skill's failures.
        Some(skill_id),
        ctx.clock.now_unix_millis(),
    );
    eprintln!(
        "topos pull: skill {skill_id}: {}",
        crate::render::safe_message(e)
    );
    warnings.push(skill_warning(skill_id, e));
}

/// A shallow copy of `ctx` with the plane source swapped (the breaker wraps the real transport for the
/// duration of one sweep; every other seam is shared).
fn ctx_with_plane<'a>(ctx: &'a Ctx<'a>, plane: &'a dyn PlaneSource) -> Ctx<'a> {
    Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: ctx.layout.clone(),
        harness: ctx.harness,
        plane,
        follow: ctx.follow,
    }
}

/// The per-invocation plane circuit breaker. Delegates to the real source until the first
/// **connect-level** failure ([`PlaneError::Unreachable`] — the plane could not be dialed at all), then
/// answers every remaining call with an immediate `Unreachable` so a sweep over N skills costs one
/// connect timeout, not N (and the proposals count is skipped entirely). An HTTP-level failure
/// ([`PlaneError::Unavailable`], e.g. a 500 on one skill) never trips it — the plane answered.
struct BreakerPlane<'a> {
    inner: &'a dyn PlaneSource,
    down: Cell<bool>,
}

impl<'a> BreakerPlane<'a> {
    fn new(inner: &'a dyn PlaneSource) -> Self {
        Self {
            inner,
            down: Cell::new(false),
        }
    }

    fn tripped(&self) -> bool {
        self.down.get()
    }

    fn short_circuit(&self) -> PlaneError {
        PlaneError::Unreachable(
            "the plane was unreachable earlier in this pull; skipping the remaining calls".into(),
        )
    }

    fn note<T>(&self, r: Result<T, PlaneError>) -> Result<T, PlaneError> {
        if matches!(r, Err(PlaneError::Unreachable(_))) {
            self.down.set(true);
        }
        r
    }
}

impl PlaneSource for BreakerPlane<'_> {
    fn get_current(
        &self,
        skill_id: &str,
        known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        if self.down.get() {
            return Err(self.short_circuit());
        }
        self.note(self.inner.get_current(skill_id, known))
    }

    fn fetch_version(
        &self,
        skill_id: &str,
        version_id: [u8; 32],
    ) -> Result<FetchedVersion, PlaneError> {
        if self.down.get() {
            return Err(self.short_circuit());
        }
        self.note(self.inner.fetch_version(skill_id, version_id))
    }

    fn list_open_proposals(&self, skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        if self.down.get() {
            return Err(self.short_circuit());
        }
        self.note(self.inner.list_open_proposals(skill_id))
    }
}

/// The count of OPEN proposals across the FOLLOWED skills (the `proposals_awaiting` figure) — sourced from
/// the plane's proposals read route, one GET per followed skill. **Best-effort:** a per-skill read failure
/// contributes `0` and never aborts the pull (and never writes to stdout — the session-start hook injects
/// stdout). Runs after the sweep, through the same breaker, so a down plane costs it nothing.
fn sum_open_proposals(ctx: &Ctx<'_>) -> u32 {
    ctx.follow
        .followed()
        .into_iter()
        .filter(|(_, f)| f.following)
        .map(|(id, _)| {
            ctx.plane
                .list_open_proposals(&id)
                .map(|p| u32::try_from(p.len()).unwrap_or(u32::MAX))
                .unwrap_or(0)
        })
        .fold(0u32, u32::saturating_add)
}
