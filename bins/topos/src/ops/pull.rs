//! `pull` — the session-start auto-update entry point + the targeted accept / go-back.
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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use topos_core::digest::to_hex;
use topos_types::persisted::{Lock, PlacementMap, SyncState};
use topos_types::results::{PullAction, PullData, PullSkill, ResetData, WorkspaceSyncReport};
use topos_types::{CurrentRecord, PointerScope, WIRE_SCHEMA_VERSION, WireCurrentRecord};

use crate::ctx::Ctx;
use crate::enroll::{self, FollowEntry, FollowModeDoc};
use crate::error::ClientError;
use crate::id::SkillId;
use crate::plane::{
    DeliverySkill, DeliverySource, FetchedVersion, FollowContext, FollowMode, FollowSource,
    KnownCurrent, PlaneError, PlaneSource, PointerFetch,
};
use crate::sync_status::{self, DeliveredSkill, WorkspaceSync};
use crate::{doc, sidecar};

use super::sync_engine::{self, Invocation};

/// The never-received sentinel the first-receive baseline carries (and an upstream withdrawal
/// restores, so a later re-delivery installs afresh instead of reading as already-current).
const ZERO_GEN: u64 = 0;
const ZERO_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// What a `pull` invocation targets.
#[derive(Debug)]
pub(crate) enum PullScope {
    /// The bare session-start sweep — every followed skill.
    AllFollowed,
    /// One skill, by name, in a targeted mode. `workspace` pins the resolution to a specific
    /// workspace when a qualified path (`<ws>/skills/<name>`) selected one — so a name shared across
    /// workspaces resolves to exactly the one the user addressed, never a different one or an
    /// over-strict ambiguity refusal.
    One {
        name: String,
        workspace: Option<String>,
        mode: TargetMode,
    },
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
/// instead of stderr-only. `access_gone` / `unreachable` are the STRUCTURED workspace-level signals the
/// hook's quiet posture reads (a freeze line; the staleness warning) — the warnings carry the same facts
/// as prose, but the hook must not parse prose.
pub(crate) struct PullOutcome {
    pub data: PullData,
    pub warnings: Vec<String>,
    /// Workspaces whose whole delivery answered the uniform 404 THIS run (removed / revoked) — every
    /// copy froze in place.
    pub access_gone: Vec<String>,
    /// Workspaces whose delivery could not be fetched THIS run (transport-level) — state kept, retry
    /// next session; the quiet hook warns only once the staleness window is blown.
    pub unreachable: Vec<String>,
}

impl PullOutcome {
    /// Wrap the schema payload with no workspace-level signals (the targeted paths).
    fn plain(data: PullData, warnings: Vec<String>) -> Self {
        Self {
            data,
            warnings,
            access_gone: Vec::new(),
            unreachable: Vec::new(),
        }
    }
}

/// How a delivery-driven reconcile behaves — the `follow --yes` apply and the hook differ only here.
/// The default (the bare enrolled sweep) accepts nothing silently, declines nothing, renames nothing,
/// reconciles every enrolled workspace, and fetches notices WITHOUT acking.
#[derive(Default)]
pub(crate) struct ReconcileOpts {
    /// Accept first-receive offers THIS invocation (the `follow --yes` batch consent: the describe
    /// disclosed the install set, so the never-received arrivals land instead of staying offers).
    /// Already-received skills keep their normal consent path — this never releases a hold or
    /// auto-applies a confirm-each update.
    pub accept_first_receive: bool,
    /// Skill ids NOT to install (declined dirname collisions): a declined NEW arrival is skipped
    /// wholesale — no follow entry, no baseline, no bytes.
    pub decline: HashSet<String>,
    /// skill_id → the dirname to install a NEW arrival under (the `--prefix-dirname` `<ws>.<name>`
    /// choice for a colliding name). Only a fresh install consults it.
    pub rename: HashMap<String, String>,
    /// Reconcile only this workspace (a `follow --yes` targets one); `None` = every enrolled one.
    pub only_workspace: Option<String>,
    /// When `Some`, RESTRICT first-receive acceptance AND new-arrival installation to these skill ids —
    /// the targeted RE-ATTACH installs exactly its subject. Every OTHER pending first-receive stays
    /// undisclosed: a never-received skill this device already follows keeps its offer (never
    /// auto-placed), and a brand-new arrival outside the set is skipped WHOLESALE (no follow entry, no
    /// baseline, no bytes) for the next full describe to disclose. `None` (the default) installs across
    /// the whole delivered set (the bare sweep / `follow --yes`, whose describe already disclosed it all).
    pub install_only: Option<HashSet<String>>,
    /// Ack the delivered notices after collecting them (the interactive / `--json` update); the
    /// quiet hook fetches WITHOUT acking, so nothing is marked read that no one narrated.
    pub ack_notices: bool,
    /// Adopt NEW arrivals as confirm-each followers (`follow --manual`): every later update is an
    /// offer, never an auto-land. `false` = the auto default.
    pub confirm_each: bool,
}

/// Run the update check for `scope`.
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
            // The proposals count runs AFTER the sweep (it is disclosure, not the update itself) and is skipped
            // entirely once the breaker tripped — no point burning more connect timeouts on it.
            let proposals_awaiting = if breaker.tripped() {
                0
            } else {
                sum_open_proposals(&sweep_ctx)
            };
            Ok(PullOutcome::plain(
                PullData {
                    skills,
                    proposals_awaiting,
                    notices: Vec::new(),
                    sync: Vec::new(),
                },
                warnings,
            ))
        }
        PullScope::One {
            name,
            workspace,
            mode,
        } => {
            let (skill_id, _lock) =
                super::resolve_skill_in_workspace(ctx, &name, workspace.as_deref())?;
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
            Ok(PullOutcome::plain(
                PullData {
                    skills: vec![row],
                    proposals_awaiting,
                    notices: Vec::new(),
                    sync: Vec::new(),
                },
                Vec::new(),
            ))
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
/// `update --reset <skill>...` — the loss-led two-phase discard. Refuses without a named skill (a reset
/// throws away local edits; it must never be a blanket "reset everything"). The describe LEADS with the
/// exact draft delta being discarded (the local `diff` — draft vs current); `--yes` discards it, restoring
/// the followed `current` (an imported skill's adopted origin). Resolves ALL-OR-NONE.
///
/// # Errors
/// [`ClientError::InvalidArgument`] with no named skill; name-resolution errors; a store / io failure.
pub(crate) fn reset(
    ctx: &Ctx<'_>,
    targets: &[String],
    yes: bool,
) -> Result<ResetOutcome, ClientError> {
    if targets.is_empty() {
        return Err(ClientError::InvalidArgument(
            "`update --reset` needs a skill name — it discards that skill's local edits; it will not \
             reset every followed skill at once (name the skill: `topos update <skill> --reset`)"
                .into(),
        ));
    }
    // Resolve ALL-OR-NONE, then compute each draft delta (the loss the describe leads with).
    let mut resolved = Vec::with_capacity(targets.len());
    for token in targets {
        resolved.push(super::resolve_skill(ctx, token)?);
    }
    let mut items = Vec::with_capacity(resolved.len());
    for (id, lock) in &resolved {
        // The draft delta vs current — the exact bytes a reset drops. DIVERGENT copies cannot render
        // one diff (that freeze is exactly what `--reset` is the named way out of), so the loss is
        // disclosed as the frozen set instead of failing the reset. UNCAPPED deliberately: a loss
        // disclosure must never truncate what would be discarded.
        let drop_diff = match super::diff(ctx, &lock.name, None, super::DiffBudget::unlimited()) {
            Ok(d) => d.diff,
            Err(e @ ClientError::PlacementsDiverged { .. }) => {
                format!("{e}\n(each copy is snapshotted into the local store before the reset)")
            }
            Err(e) => return Err(e),
        };
        items.push(ResetData {
            skill: lock.name.clone(),
            workspace_id: super::followed_workspace(ctx, id.as_str()),
            to_version: lock.base_commit.clone(),
            drop_diff,
            applied: false,
        });
    }

    if !yes {
        let mut yes_argv = vec!["topos".to_owned(), "update".to_owned()];
        yes_argv.extend(targets.iter().cloned());
        yes_argv.push("--reset".to_owned());
        yes_argv.push("--yes".to_owned());
        return Ok(ResetOutcome::Described { items, yes_argv });
    }

    // ---- APPLY (`--yes`) ---- discard each draft back to its base (the draft is snapshotted first).
    for (id, _lock) in &resolved {
        sync_engine::reset_to_base(ctx, id)?;
    }
    for item in &mut items {
        item.applied = true;
    }
    Ok(ResetOutcome::Applied(items))
}

/// The two-phase outcome of `update --reset`.
#[derive(Debug)]
pub(crate) enum ResetOutcome {
    Described {
        items: Vec<ResetData>,
        yes_argv: Vec<String>,
    },
    Applied(Vec<ResetData>),
}

/// The reconcile with explicit [`ReconcileOpts`] — the `follow --yes` apply (one workspace,
/// batch-accepted first receives, declined/renamed collisions) and the interactive `update` (acked
/// notices) drive the non-default forms; `&ReconcileOpts::default()` is the bare sweep.
pub(crate) fn pull_reconcile_with(
    ctx: &Ctx<'_>,
    delivery: &dyn DeliverySource,
    opts: &ReconcileOpts,
) -> Result<PullOutcome, ClientError> {
    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    let mut proposals_awaiting: u32 = 0;
    let mut notices = Vec::new();
    let mut access_gone = Vec::new();
    let mut unreachable = Vec::new();
    // The prior freshness doc (best-effort: a corrupt one degrades to empty with a warning — the
    // hook must never wedge on an advisory file) — a failed report keeps its last-report time.
    let prior_sync = match sync_status::read(ctx.fs, &ctx.layout) {
        Ok(s) => s,
        Err(e) => {
            warnings.push(format!("SYNC_STATUS_UNREADABLE: {}", e.detail()));
            crate::sync_status::SyncStatus::default()
        }
    };
    let mut sync_updates: Vec<(String, WorkspaceSync)> = Vec::new();
    let now_millis = i64::try_from(ctx.clock.now_unix_millis()).unwrap_or(i64::MAX);

    // The follow-state snapshot this sweep classifies against (per-skill mutations below re-read
    // under the identity lock, so a stale entry here costs at most one extra row next sweep).
    let followed = ctx.follow.followed();

    for ws in delivery.workspaces() {
        if opts
            .only_workspace
            .as_deref()
            .is_some_and(|only| only != ws)
        {
            continue;
        }
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
                access_gone.push(ws.clone());
                continue;
            }
            Err(PlaneError::Unreachable(m) | PlaneError::Unavailable(m)) => {
                // Transient: keep state, retry next session — the hook must never block a session.
                warnings.push(format!("PLANE_UNAVAILABLE {ws}: {m}"));
                unreachable.push(ws.clone());
                continue;
            }
            Err(PlaneError::Malformed(m)) => {
                warnings.push(format!("WIRE_INVALID {ws}: {m}"));
                continue;
            }
        };
        proposals_awaiting = proposals_awaiting
            .saturating_add(u32::try_from(snapshot.proposals_awaiting).unwrap_or(u32::MAX));

        // The offline delivery cache this reconcile rebuilds for `list` (the served version + `via`
        // channels per delivered skill; a withdrawn skill is flagged below). REPLACES the prior map,
        // so a re-delivered skill self-heals and a since-detached one drops out.
        let mut delivered_cache: BTreeMap<String, DeliveredSkill> = BTreeMap::new();
        let mut delivered_ids: HashSet<&str> = HashSet::with_capacity(snapshot.skills.len());
        for ds in &snapshot.skills {
            // A DECLINED new arrival (a dirname collision the follow describe listed and `--yes`
            // did not opt into) is skipped WHOLESALE — no follow entry, no baseline, no bytes, and
            // no delivered-id mark (the undelivered classifier below must not treat it as detached:
            // it is not followed, so the filter never selects it anyway).
            if opts.decline.contains(&ds.skill_id) {
                continue;
            }
            delivered_ids.insert(ds.skill_id.as_str());
            delivered_cache.insert(
                ds.skill_id.clone(),
                DeliveredSkill {
                    served_version: to_hex(&ds.version_id),
                    withdrawn: false,
                    via_channels: ds.via_channels.clone(),
                },
            );
            // Teach the READ transport this skill's credential before anything fetches its bytes.
            // The per-skill credential map is derived from `follows.json`, which cannot yet name a
            // brand-new arrival — without this, the arrival's first version fetch answers
            // "not served" and the offer is lost for a session.
            delivery.bind_skill(&ws, &ds.skill_id);
            let entry = followed.iter().find(|(id, _)| *id == ds.skill_id);
            let row = match entry {
                // An entry the LOCAL `unfollow` verb paused, which the plane still delivers: respect
                // the local pause (that verb is byte-inert and offline-capable; its server row is
                // written by the unfollow apply, but a plane that still delivers is honoured locally
                // and `follow <skill>` resumes it, exactly as before). Nothing this sweep does ever
                // writes `following = false`, so this arm only ever sees a deliberate local pause —
                // a server-side detach freezes by touching nothing.
                Some((_, f)) if !f.following => continue,
                Some((_, f)) => sync_delivered(ctx, &ws, ds, f, accept_for(ctx, ds, opts)),
                // A brand-new arrival OUTSIDE a targeted install (the re-attach) stays undisclosed —
                // no follow entry, no baseline, no bytes. Skipped wholesale (like a declined collision)
                // so the next full describe is the first to disclose it.
                None if opts
                    .install_only
                    .as_ref()
                    .is_some_and(|only| !only.contains(&ds.skill_id)) =>
                {
                    continue;
                }
                None => install_new_arrival(ctx, &ws, ds, opts),
            };
            match row {
                Ok(mut row) => {
                    row.workspace_id = Some(ws.clone());
                    skills.push(row);
                }
                Err(e) => note_skill_failure(ctx, &mut warnings, &ds.skill_id, &e),
            }
        }

        // The undelivered remainder: WHO ACTED decides the on-disk consequence. Three actors, three
        // outcomes — and absence alone cannot tell them apart, which is why the plane names two of
        // them explicitly and upstream is the remainder.
        let detached: HashSet<&str> = snapshot.detached.iter().map(String::as_str).collect();
        let excluded: HashSet<&str> = snapshot.excluded.iter().map(String::as_str).collect();
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
                // The PERSON detached it (unfollow / a channel leave that lapsed it): freeze in
                // place on every device — bytes untouched, `follow` re-attaches.
                freeze_detached(ctx, &sid)
            } else if excluded.contains(skill_id.as_str()) {
                // THIS DEVICE excludes it ("not on this device"). The `remove` verb already cleared
                // the agent dirs here; the person keeps receiving it elsewhere. Report the true
                // cause and touch nothing — mistaking this for an upstream withdrawal would narrate
                // a lie (and re-run a clean that already happened). The sync read is fallible (a
                // downgrade past a bumped doc schema can make it unreadable); keep it INSIDE the
                // per-skill Result so a failure is isolated to this skill, never aborting the sweep
                // (the quiet hook must not die on one poisoned doc).
                read_sync(ctx, &sid).map(|s| {
                    undelivered_row(
                        &skill_name_or_id(ctx, &sid),
                        s.as_ref(),
                        PullAction::Excluded,
                    )
                })
            } else {
                // UPSTREAM withdrew it (archived, or its last delivering channel dropped it):
                // managed distribution cleans what it managed. Flag it in the offline cache so
                // `list` reports `removed-upstream` without a network call.
                delivered_cache.insert(
                    skill_id.to_string(),
                    DeliveredSkill {
                        withdrawn: true,
                        ..DeliveredSkill::default()
                    },
                );
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
        let mut report_ok = false;
        match applied_snapshot(ctx, &delivered_ids) {
            Ok(applied) => match delivery.report_applied(&ws, &applied) {
                Ok(()) => report_ok = true,
                Err(e) => {
                    let m = match e {
                        PlaneError::NotFound => "access gone".to_owned(),
                        PlaneError::Unreachable(m)
                        | PlaneError::Unavailable(m)
                        | PlaneError::Malformed(m) => m,
                    };
                    warnings.push(format!("REPORT_FAILED {ws}: {m}"));
                }
            },
            Err(e) => warnings.push(format!("REPORT_FAILED {ws}: {}", e.detail())),
        }

        // The freshness record this delivery earns: the delivery time always advances (the fetch
        // succeeded to get here); the report time advances only when the report landed (a failed
        // one keeps its prior stamp, so `auth status` stays honest about the fleet's view).
        sync_updates.push((
            ws.clone(),
            WorkspaceSync {
                last_delivery_at: Some(now_millis),
                last_report_at: if report_ok {
                    Some(now_millis)
                } else {
                    prior_sync
                        .workspaces
                        .get(&ws)
                        .and_then(|e| e.last_report_at)
                },
                staleness_window_ms: snapshot.staleness_window_ms,
                delivered: delivered_cache,
            },
        ));

        // The notices feed, LAST for this workspace (an ack marks person-scoped read-state, so it
        // fires only once the reconcile that surfaces them has actually run). The quiet hook fetches
        // without acking; the interactive/`--json` update acks exactly the ids it returns.
        if !snapshot.notices.is_empty() {
            if opts.ack_notices {
                let ids: Vec<String> = snapshot.notices.iter().map(|n| n.id.clone()).collect();
                if let Err(e) = delivery.ack_notices(&ws, &ids) {
                    let m = match e {
                        PlaneError::NotFound => "access gone".to_owned(),
                        PlaneError::Unreachable(m)
                        | PlaneError::Unavailable(m)
                        | PlaneError::Malformed(m) => m,
                    };
                    warnings.push(format!("ACK_FAILED {ws}: {m}"));
                }
            }
            notices.extend(snapshot.notices);
        }
    }

    // Persist the freshness doc (best-effort — advisory state must never fail a sync) and mirror it
    // onto the payload for the `--json` surface.
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
            skills,
            proposals_awaiting,
            notices,
            sync,
        },
        warnings,
        access_gone,
        unreachable,
    })
}

/// `update [<skill>...] [--skill <s>...] [--channel <c>...]` — the SELECTOR / MULTI-TARGET update. Resolves
/// every positional + `--skill` + `--channel` name through the ONE grammar ALL-OR-NONE (an unresolvable
/// name refuses the whole invocation, exactly like `follow`), then:
///  - a resolved SKILL runs the targeted per-skill path (accept its pending update);
///  - a resolved CHANNEL runs a channel-filtered sync over the live delivery's `via` attribution —
///    installing new arrivals in that channel and landing pending updates for the ones already followed.
///
/// # Errors
/// The grammar's resolution errors (`AMBIGUOUS_NAME`, the uniform not-found, a kind mismatch);
/// [`ClientError::Enrollment`] when a `--channel` selector is used without a delivery transport; a
/// store/io/transport failure.
pub(crate) fn update_selective(
    ctx: &Ctx<'_>,
    directory_connect: &super::follow::DirectoryConnect<'_>,
    delivery: Option<&dyn DeliverySource>,
    positionals: &[String],
    channels: &[String],
    skills: &[String],
    only_workspace: Option<&str>,
) -> Result<PullOutcome, ClientError> {
    use crate::resolve::{self, KindScope, Resolution, ResourceKind, TargetSpec};

    // Build the enrolled universe, then resolve every target ALL-OR-NONE (channels + skills only).
    let (_, universe) = super::follow::build_universe_via(ctx, directory_connect)?;
    let mut specs: Vec<TargetSpec> = Vec::new();
    for t in positionals {
        specs.push(TargetSpec::free(t));
    }
    for s in skills {
        specs.push(TargetSpec::kinded(s, ResourceKind::Skill));
    }
    for c in channels {
        specs.push(TargetSpec::kinded(c, ResourceKind::Channel));
    }
    let resolutions = resolve::resolve_all(&universe, &specs, KindScope::SUBSCRIBABLE)?;

    // Partition into skill names (targeted) and channel names (delivery-filtered).
    let mut skill_targets: Vec<(String, String)> = Vec::new();
    let mut channel_targets: Vec<(String, String)> = Vec::new();
    for r in resolutions {
        match r {
            Resolution::Resource {
                kind: ResourceKind::Skill,
                name,
                workspace_id,
                ..
            } => skill_targets.push((name, workspace_id)),
            Resolution::Resource {
                kind: ResourceKind::Channel,
                name,
                workspace_id,
                ..
            } => channel_targets.push((name, workspace_id)),
            // SUBSCRIBABLE excludes workspaces, so a workspace resolution cannot occur here.
            Resolution::Workspace { .. } => {}
        }
    }

    let mut acc = PullAccumulator::default();
    // Skill targets → the targeted per-skill engine (accept a pending update / resume a hold),
    // pinned to the workspace the qualified path selected.
    for (name, ws) in &skill_targets {
        acc.merge(pull(
            ctx,
            PullScope::One {
                name: name.clone(),
                workspace: Some(ws.clone()),
                mode: TargetMode::AcceptPending,
            },
        )?);
    }
    // Channel targets → the channel-filtered delivery sync.
    if !channel_targets.is_empty() {
        let delivery = delivery.ok_or_else(|| {
            ClientError::Enrollment("not enrolled; nothing to update from a channel".into())
        })?;
        let _ = only_workspace;
        acc.merge(pull_channel_filtered(ctx, delivery, &channel_targets)?);
    }
    Ok(acc.into_outcome())
}

/// A channel-scoped update: sync every DELIVERED skill whose `via` channels include one of the
/// requested `(channel_name, workspace_id)` pairs — installing a new arrival, landing a pending update
/// for a known one — over the live delivery. The pair is SCOPED: a channel name shared across two
/// workspaces syncs only the one the qualified path selected, never both. It is a PARTIAL, targeted
/// operation (like `pull <skill>`): it never withdraws, freezes, reports, acks notices, or writes the
/// freshness cache — those belong to the full bare reconcile. A whole-workspace access loss still
/// surfaces as a warning + the structured `access_gone` signal.
fn pull_channel_filtered(
    ctx: &Ctx<'_>,
    delivery: &dyn DeliverySource,
    channels: &[(String, String)],
) -> Result<PullOutcome, ClientError> {
    // (workspace_id, channel_name) — matched as a PAIR, so a same-named channel in another workspace
    // is never swept in.
    let want: HashSet<(&str, &str)> = channels
        .iter()
        .map(|(name, ws)| (ws.as_str(), name.as_str()))
        .collect();
    let wanted_ws: HashSet<&str> = channels.iter().map(|(_, ws)| ws.as_str()).collect();
    let followed = ctx.follow.followed();
    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    let mut proposals_awaiting: u32 = 0;
    let mut access_gone = Vec::new();
    let mut unreachable = Vec::new();

    for ws in delivery.workspaces() {
        if !wanted_ws.contains(ws.as_str()) {
            continue;
        }
        let snapshot = match delivery.fetch_delivery(&ws) {
            Ok(s) => s,
            Err(PlaneError::NotFound) => {
                warnings.push(format!(
                    "ACCESS_GONE {ws}: this device no longer has access; its skills stay frozen in \
                     place (re-adding the member re-enables them)"
                ));
                access_gone.push(ws.clone());
                continue;
            }
            Err(PlaneError::Unreachable(m) | PlaneError::Unavailable(m)) => {
                warnings.push(format!("PLANE_UNAVAILABLE {ws}: {m}"));
                unreachable.push(ws.clone());
                continue;
            }
            Err(PlaneError::Malformed(m)) => {
                warnings.push(format!("WIRE_INVALID {ws}: {m}"));
                continue;
            }
        };
        proposals_awaiting = proposals_awaiting
            .saturating_add(u32::try_from(snapshot.proposals_awaiting).unwrap_or(u32::MAX));
        for ds in &snapshot.skills {
            if !ds
                .via_channels
                .iter()
                .any(|c| want.contains(&(ws.as_str(), c.as_str())))
            {
                continue;
            }
            // Teach the read transport this skill's credential before its first byte fetch (a new
            // arrival is not yet named in `follows.json`).
            delivery.bind_skill(&ws, &ds.skill_id);
            let entry = followed.iter().find(|(id, _)| *id == ds.skill_id);
            let row = match entry {
                // A locally-paused (unfollowed) entry the plane still delivers: respect the pause.
                Some((_, f)) if !f.following => continue,
                // Naming the channel is explicit consent to re-sync a skill you ALREADY hold — accept
                // the pending update. But a NEVER-SEEN arrival in the channel was not named and its
                // bytes/digest were never disclosed, so it stays a first-receive OFFER (the I-TOFU the
                // bare sweep gives too) — `follow --yes` is the path that batch-accepts after a describe.
                Some((_, f)) => sync_delivered(ctx, &ws, ds, f, Invocation::Accept),
                None => install_new_arrival(ctx, &ws, ds, &ReconcileOpts::default()),
            };
            match row {
                Ok(mut row) => {
                    row.workspace_id = Some(ws.clone());
                    skills.push(row);
                }
                Err(e) => note_skill_failure(ctx, &mut warnings, &ds.skill_id, &e),
            }
        }
    }
    Ok(PullOutcome {
        data: PullData {
            skills,
            proposals_awaiting,
            notices: Vec::new(),
            sync: Vec::new(),
        },
        warnings,
        access_gone,
        unreachable,
    })
}

/// Accumulates several [`PullOutcome`]s from a multi-target update into one — concatenating the per-skill
/// rows / warnings / structured signals, and keeping the MAX proposals gauge (each targeted pull computes
/// the same workspace-wide count, so summing would multiply it).
#[derive(Default)]
struct PullAccumulator {
    skills: Vec<PullSkill>,
    warnings: Vec<String>,
    access_gone: Vec<String>,
    unreachable: Vec<String>,
    proposals_awaiting: u32,
}

impl PullAccumulator {
    fn merge(&mut self, out: PullOutcome) {
        self.skills.extend(out.data.skills);
        self.warnings.extend(out.warnings);
        self.access_gone.extend(out.access_gone);
        self.unreachable.extend(out.unreachable);
        self.proposals_awaiting = self.proposals_awaiting.max(out.data.proposals_awaiting);
    }
    fn into_outcome(self) -> PullOutcome {
        PullOutcome {
            data: PullData {
                skills: self.skills,
                proposals_awaiting: self.proposals_awaiting,
                notices: Vec::new(),
                sync: Vec::new(),
            },
            warnings: self.warnings,
            access_gone: self.access_gone,
            unreachable: self.unreachable,
        }
    }
}

/// The invocation an already-followed DELIVERED skill syncs under: the `follow --yes` batch consent
/// accepts a still-pending FIRST RECEIVE (the baseline exists, nothing ever landed), and nothing
/// else — an already-received skill keeps its normal consent path (a hold stays held, a confirm-each
/// update stays an offer).
fn accept_for(ctx: &Ctx<'_>, ds: &DeliverySkill, opts: &ReconcileOpts) -> Invocation {
    if !opts.accept_first_receive {
        return Invocation::Sweep;
    }
    // A targeted install (the re-attach) accepts ONLY its subject's first receive — any OTHER
    // never-received skill stays an offer, never silently placed under a describe that named just one.
    if opts
        .install_only
        .as_ref()
        .is_some_and(|only| !only.contains(&ds.skill_id))
    {
        return Invocation::Sweep;
    }
    let never_received = SkillId::parse(&ds.skill_id)
        .ok()
        .and_then(|sid| read_sync(ctx, &sid).ok().flatten())
        .as_ref()
        .is_some_and(sync_engine::is_never_received);
    if never_received {
        Invocation::Accept
    } else {
        Invocation::Sweep
    }
}

/// Sync one already-followed delivered skill, feeding the engine the delivery's resolved target
/// (no second pointer GET) and the FRESH per-bundle protection posture (the stored doc's flag may
/// lag; the runtime context never does). `inv` is [`Invocation::Accept`] only for the `follow
/// --yes` batch consent on a still-pending first receive (see [`accept_for`]).
fn sync_delivered(
    ctx: &Ctx<'_>,
    ws: &str,
    ds: &DeliverySkill,
    follow: &FollowContext,
    inv: Invocation,
) -> Result<PullSkill, ClientError> {
    let sid = SkillId::parse(&ds.skill_id)?;
    let follow = FollowContext {
        review_required: ds.review_required,
        ..follow.clone()
    };
    let rec = delivered_record(ws, ds);
    sync_engine::sync_one_with(ctx, &sid, &follow, inv, Some(&rec))
}

/// A brand-new delivered skill this device has never held: record the follow entry (person-scoped
/// truth lives on the plane; the local entry is install-state), lay the first-receive baseline
/// under the skill's CATALOG name (or the caller's `--prefix-dirname` rename), and run the engine —
/// whose kernel consent OFFERS the first bytes (I-TOFU) on a bare sweep, and PLACES them under the
/// `follow --yes` batch consent ([`ReconcileOpts::accept_first_receive`], where the describe already
/// disclosed the install set).
fn install_new_arrival(
    ctx: &Ctx<'_>,
    ws: &str,
    ds: &DeliverySkill,
    opts: &ReconcileOpts,
) -> Result<PullSkill, ClientError> {
    let sid = SkillId::parse(&ds.skill_id)?;
    let dirname = opts
        .rename
        .get(&ds.skill_id)
        .cloned()
        .unwrap_or_else(|| ds.name.clone());
    // The BASELINE FIRST, the follow entry second — the same ordering `follow`'s promote uses. A
    // crash between them leaves a baseline with no entry, which the NEXT reconcile treats as a
    // fresh arrival again (the baseline layer is idempotent: it returns early if the skill dir
    // exists). The reverse order would leave an entry whose sidecar docs do not exist, and every
    // later sweep would fail that skill on a missing-doc read — a permanent wedge.
    super::follow::lay_first_receive_baseline(ctx, &sid, dirname, ws)?;
    enroll::write_follows_merged(
        ctx.fs,
        &ctx.layout,
        &[FollowEntry {
            skill_id: ds.skill_id.clone(),
            workspace_id: ws.to_owned(),
            mode: if opts.confirm_each {
                FollowModeDoc::ConfirmEach
            } else {
                FollowModeDoc::Auto
            },
            review_required: ds.review_required,
            following: true,
            // A fresh delivery-driven arrival clears any prior per-device exclusion of this id.
            excluded_here: false,
            agents: Vec::new(),
            excluded_agents: Vec::new(),
        }],
    )?;
    let follow = FollowContext {
        workspace_id: ws.to_owned(),
        mode: if opts.confirm_each {
            FollowMode::ConfirmEach
        } else {
            FollowMode::Auto
        },
        review_required: ds.review_required,
        following: true,
        agents: Vec::new(),
        excluded_agents: Vec::new(),
    };
    let rec = delivered_record(ws, ds);
    let inv = if opts.accept_first_receive {
        Invocation::Accept
    } else {
        Invocation::Sweep
    };
    sync_engine::sync_one_with(ctx, &sid, &follow, inv, Some(&rec))
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
    Ok(undelivered_row(
        &skill_name_or_id(ctx, sid),
        sync.as_ref(),
        PullAction::Detached,
    ))
}

/// UPSTREAM withdrew this skill (archived, or its last delivering channel dropped it) — managed
/// distribution cleans what it managed: snapshot any draft into the sidecar store first (nothing
/// is ever lost), remove the agent dirs, keep every sidecar byte, and freeze the entry. Idempotent
/// across a crash at any step (a re-run re-snapshots nothing, re-removes nothing, re-flips
/// nothing).
/// Which caller drives [`snapshot_and_clean`] — only the fail-closed refusal wording differs (an
/// unreadable placement must never be silently deleted, and the message names the actual cause).
#[derive(Debug, Clone, Copy)]
pub(crate) enum WithdrawReason {
    /// The sweep's upstream withdrawal (archived / last channel dropped).
    Upstream,
    /// The `remove` verb's per-device exclusion.
    RemoveExclusion,
}

/// Snapshot any local draft into the sidecar store, then CLEAN the agent dirs — keeping every sidecar
/// byte. The shared half of managed-distribution takedown: the sweep's upstream withdrawal and the
/// `remove` verb's per-device exclusion both run it. Returns the prior [`SyncState`] (for the caller's
/// row/reset). Idempotent across a crash (a re-run re-snapshots nothing, re-removes nothing). FAILS
/// CLOSED on an unscannable placement — a symlink/device/non-UTF-8 name we cannot snapshot must never
/// be silently deleted.
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] on an unscannable placement; a store/io failure otherwise.
pub(crate) fn snapshot_and_clean(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    reason: WithdrawReason,
) -> Result<Option<SyncState>, ClientError> {
    let sp = ctx.layout.published(sid);
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, sid)?;
    let sync: Option<SyncState> = doc::read_doc(ctx.fs, &sp.sync)?;
    let lock: Option<Lock> = doc::read_doc(ctx.fs, &sp.lock)?;
    let map: Option<PlacementMap> = doc::read_map(ctx.fs, &sp.map)?;
    if let (Some(lock), Some(map)) = (lock.as_ref(), map.as_ref()) {
        // EVERY distinct edited copy is retained in the sidecar store BEFORE any byte leaves an
        // agent dir — nothing is ever lost, whichever placement carried the edit. UNSCANNABLE
        // placements FAIL CLOSED: we cannot snapshot what we cannot scan, so we must not delete it
        // — the skill freezes with a typed refusal instead of a silent, unrecoverable delete.
        let scans = crate::placement::scan_placements(ctx, map)?;
        if scans
            .iter()
            .any(|s| matches!(s.status, crate::placement::ScanStatus::Unscannable))
        {
            return Err(ClientError::PlacementUnsupported {
                reason: match reason {
                    WithdrawReason::Upstream => {
                        "upstream withdrew this skill, but its placement cannot be read; \
                         refusing to remove it — inspect or move the directory by hand"
                    }
                    WithdrawReason::RemoveExclusion => {
                        "this skill's placement cannot be read; refusing to remove it — \
                         inspect or move the directory by hand"
                    }
                }
                .into(),
            });
        }
        for (idx, _) in crate::placement::distinct_modified(&scans) {
            if let crate::placement::ScanStatus::Modified { scanned } = &scans[idx].status {
                sync_engine::snapshot_draft(ctx, &sp, lock, scanned)?;
            }
        }
        for (placement, scan) in map.placements.iter().zip(&scans) {
            // A FOREIGN placement — recorded as a target but never materialized by topos, and
            // since occupied by someone else's bytes (the user, or the harness itself) — is not
            // ours to delete: it was never snapshotted and never ours. Leave it in place (the
            // same rule the scope-change cleanup applies).
            if matches!(scan.status, crate::placement::ScanStatus::Foreign) {
                continue;
            }
            let p = Path::new(placement);
            if ctx.fs.exists(p) {
                ctx.fs.remove_dir_all(p)?;
            }
        }
    }
    Ok(sync)
}

fn withdraw_upstream(ctx: &Ctx<'_>, sid: &SkillId) -> Result<PullSkill, ClientError> {
    // Snapshot any draft into the sidecar store, then clean the agent dirs (keeping every sidecar
    // byte) — the shared machinery `remove` reuses for a per-device exclusion.
    let sync = snapshot_and_clean(ctx, sid, WithdrawReason::Upstream)?;
    // NOTE: no follow-state flip. The entry stays LIVE (a withdrawal is a delivery change, not a
    // subscription change): the sidecar keeps every byte + any draft, and the sync state is reset to
    // the NEVER-RECEIVED baseline — the same all-zero sentinel `follow` lays.
    reset_to_never_received(ctx, sid, sync.as_ref())?;
    Ok(undelivered_row(
        &skill_name_or_id(ctx, sid),
        sync.as_ref(),
        PullAction::Withdrawn,
    ))
}

/// Reset a skill's sync state to the NEVER-RECEIVED baseline — the same all-zero sentinel `follow`
/// lays. The reset is what makes a later re-delivery (a curator re-places the skill, an owner
/// unarchives it, a `follow` lifts this device's exclusion) REINSTALL: without it,
/// `applied == observed` and an absent placement read as "already current", and the skill would
/// never come back. Re-arrival then passes the kernel's I-TOFU first-receive consent — an offer,
/// disclosed, exactly as the original arrival was. A skill with no prior sync state needs no reset
/// (it already sits at the baseline).
///
/// # Errors
/// A store/io failure writing the sync doc.
pub(crate) fn reset_to_never_received(
    ctx: &Ctx<'_>,
    sid: &SkillId,
    prior: Option<&SyncState>,
) -> Result<(), ClientError> {
    let sp = ctx.layout.published(sid);
    if let Some(prior) = prior {
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
    Ok(())
}

/// A row for a skill the sweep did NOT sync (frozen / withdrawn): last-known generations, no offer.
/// `name` is the CATALOG name (what a synced row carries and what `add`/`resolve_skill` key on) — a
/// withdrawn row's `topos add <name>` "keep it as yours" salvage command must resolve, so this can
/// never be the raw id.
fn undelivered_row(name: &str, sync: Option<&SyncState>, action: PullAction) -> PullSkill {
    let (observed, applied) = sync.map_or((0, 0), |s| (s.observed, s.applied));
    PullSkill {
        skill: name.to_owned(),
        workspace_id: None,
        observed,
        applied,
        action,
        offer: None,
        conflict: None,
        merge: None,
        merge_preview: None,
    }
}

/// The skill's catalog NAME from its sidecar lock (what synced rows and `resolve_skill` use), falling
/// back to the id when no lock is readable.
fn skill_name_or_id(ctx: &Ctx<'_>, sid: &SkillId) -> String {
    let sp = ctx.layout.published(sid);
    doc::read_doc::<Lock>(ctx.fs, &sp.lock)
        .ok()
        .flatten()
        .map_or_else(|| sid.as_str().to_owned(), |l| l.name)
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
        let Some(map) = doc::read_map(ctx.fs, &sp.map)? else {
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

/// The quiet hook's stdout lines — the ONLY bytes `update --quiet` may emit on stdout (which the
/// session-start hook injects into the session), and only for the two facts a person must not miss:
///
/// - a workspace whose access is GONE this run (removed / revoked) — one line naming the freeze;
/// - a workspace that was UNREACHABLE this run AND whose last successful delivery is older than its
///   staleness window — one line with the age (a fresh miss stays silent: transient blips must not
///   spam every session).
///
/// Reads the freshness doc best-effort (an unreadable one warns nowhere — the hook stays silent
/// rather than noisy).
pub(crate) fn quiet_hook_lines(
    fs: &dyn crate::fs_seam::FsOps,
    layout: &crate::sidecar::Layout,
    now_millis: i64,
    out: &PullOutcome,
) -> Vec<String> {
    let mut lines = Vec::new();
    for ws in &out.access_gone {
        lines.push(format!(
            "topos: {ws} — this device no longer has access; its skills are frozen in place"
        ));
    }
    if out.unreachable.is_empty() {
        return lines;
    }
    let status = sync_status::read(fs, layout).unwrap_or_default();
    for ws in &out.unreachable {
        let entry = status.workspaces.get(ws);
        if sync_status::is_stale(entry, now_millis) {
            let last = entry.and_then(|e| e.last_delivery_at).unwrap_or(now_millis);
            lines.push(format!(
                "topos: {ws} last synced {} ago — server unreachable",
                sync_status::human_duration(now_millis.saturating_sub(last))
            ));
        }
    }
    lines
}

/// Whether a failed `update --quiet` exits 0 with a one-line warning instead of nonzero: the hook
/// posture — an AUTH or TRANSPORT failure must never fail the session start (the harness would
/// surface a scary error for a network blip), while a genuinely local failure (corrupt sidecar,
/// io) still exits nonzero so it is not silently swallowed forever.
pub(crate) fn quiet_soft_failure(e: &ClientError) -> bool {
    matches!(
        e,
        ClientError::Plane(_)
            | ClientError::Enrollment(_)
            | ClientError::PlaneRejected(_)
            | ClientError::PlaneTerminal { .. }
            | ClientError::Denied(_)
            | ClientError::EnrollDenied
            | ClientError::TargetNotFound { .. }
    )
}

/// A shallow copy of `ctx` with the plane source swapped (the breaker wraps the real transport for
/// the duration of one sweep; the `follow --yes` reconcile swaps its own delivery transport in; every
/// other seam is shared).
pub(super) fn ctx_with_plane<'a>(ctx: &'a Ctx<'a>, plane: &'a dyn PlaneSource) -> Ctx<'a> {
    Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: ctx.layout.clone(),
        harness: ctx.harness,
        plane,
        follow: ctx.follow,
        roots: ctx.roots.clone(),
    }
}

/// A shallow copy of `ctx` with BOTH the plane source AND the follow seam swapped — the re-attach
/// reconcile drives the delivery transport for its byte fetches (`bind_skill` must land on the object
/// the fetches use) AND a follow seam re-read from disk (the startup seam predates the re-attach's
/// `set_following` / `set_excluded` writes, so a just-re-affirmed skill would otherwise read as paused).
pub(super) fn ctx_with_plane_and_follow<'a>(
    ctx: &'a Ctx<'a>,
    plane: &'a dyn PlaneSource,
    follow: &'a dyn FollowSource,
) -> Ctx<'a> {
    Ctx {
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        layout: ctx.layout.clone(),
        harness: ctx.harness,
        plane,
        follow,
        roots: ctx.roots.clone(),
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
