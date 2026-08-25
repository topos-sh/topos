//! The per-skill sync engine: `checkForUpdates → plan → apply`, crash-safe.
//!
//! For one followed skill, under its writer flock, the engine:
//! 1. **checkForUpdates** — conditional-GET the served `current` pointer, scope-check it (workspace/skill),
//!    and adopt it as the sync target: whenever the served `(generation, version_id)` differs from the
//!    stored `observed`/`observed_version_id` IN ANY DIRECTION (a team rollback after a server restore is a
//!    legitimate backward move now), update `observed` + `observed_version_id` and drive toward it.
//! 2. **plan** — drive toward `observed`: classify the working tree (clean / draft / absent / unscannable),
//!    snapshot a draft FIRST, fetch the target's bytes, re-verify them (digest == tree == `commit_id`),
//!    record them durably in the sidecar store, then refine (a crash-after-swap heals, never a false
//!    divergence), and map the situation to a `consent::Situation`.
//! 3. **apply** — act on `consent::decide()`: materialize + advance `applied`, or resolve a diverged
//!    draft with the author-merge (never clobber).
//!
//! `applied` advances only after a successful swap. The served record IS the sync target; its integrity is
//! the content-addressed `version_id`, re-verified byte-for-byte by digest on apply — a digest mismatch is
//! a loud integrity ERROR. The consent decision is the kernel's one policy — the engine only chooses which
//! row to feed it.

use std::path::Path;

use topos_core::digest::{self, to_hex};
use topos_core::identity::{self, Commit};
use topos_core::sync::{self, ApplyClass};
use topos_gitstore::{ImportFile, Store, WriteBatch};
use topos_types::persisted::{Lock, LockedFile, PlacementMap, SyncState};
use topos_types::results::{PullAction, PullSkill};

use crate::bundle_kind::{BundleKind, RecordKind};
use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::materialize::{self, MaterializeReport, MaterializeReq};
use crate::placement::{self, ScanStatus};
use crate::plane::{FollowContext, KnownCurrent, PlaneError, PointerFetch};
use crate::scan::ScannedBundle;
use crate::{doc, logfile, sidecar};

/// The fixed commit message for a draft snapshot (folded into its `version_id`; must stay constant).
/// `pub(crate)` so `keep-as-yours` (`ops::add`) can recognise a retained draft snapshot in the store.
pub(crate) const DRAFT_SNAPSHOT_MESSAGE: &str = "topos: draft snapshot";
/// A bound on ancestor backfill — far beyond any real lineage gap; stops a forged cyclic store.
const MAX_BACKFILL: usize = 256;
/// The `applied` generation a deliberate local decision leaves behind when this machine is knowingly NOT
/// holding the served `current`: the genesis sentinel `(0,0)`, which is strictly below any real served
/// `observed`, so a later pull sees `applied != observed` (behind) and drives toward the team's version
/// again. ONE decision leaves it: the go-back, which installs an OLD version whose true generation is no
/// longer tracked locally (and pins `held` until an explicit pull).
///
/// It never reads as never-received: [`is_never_received`] also requires the all-zero `base_commit`
/// sentinel, and the go-back leaves a real commit there.
pub(crate) const APPLIED_BEHIND_CURRENT: u64 = 0;

/// A capability token proving the author-merge code was reached from a divergence. Its field is private to
/// this module, so NO other module can mint one; [`super::merge_resolve::resolve_diverged`] takes it by
/// value, so the merge is unreachable from a current/behind/clean-follower state **by construction** — a
/// structural gate, not a role check. Every mint site is guarded by one of two facts: [`sync_one`]'s
/// post-fetch `Diverged` arm holds `work != base`, and every other site stands on a recorded
/// `conflict.json` (the escape, and both recorded-conflict finisher dispatches) — a record only an
/// author's divergence ever writes. A clean follower hits none of them.
pub(crate) struct DivergedWitness(());

/// What a per-skill `sync_one` invocation is — the bare sweep or an explicit accept.
/// Replaces the old `explicit: bool`. The `--keep-mine` escape is NOT a member: it never syncs
/// anything, so it has its own entry ([`escape_one`]) rather than a mode this function must
/// remember to refuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Invocation {
    /// The bare session-start sweep (`topos pull`).
    Sweep,
    /// A targeted accept / resume (`topos pull <skill>`).
    Accept,
}

impl Invocation {
    /// Whether the user's command itself supplies consent (a targeted accept) vs the bare sweep.
    fn is_explicit(self) -> bool {
        matches!(self, Invocation::Accept)
    }
}

/// `topos update <skill> --keep-mine` — FINISH a merge that has STOPPED, by committing the author's
/// chosen tree on top of `current`. Plane-independent by construction (it reads the local record and
/// the local store and nothing else), which is the offline no-deadlock guarantee.
///
/// It is its OWN entry, and deliberately not a mode of [`sync_one`]: `sync_one` is reached only for
/// a FOLLOWED bundle, so an escape routed through it silently fell through to a read-only
/// "up to date" for every other kind of row — a local path, a forge import, an unfollowed workspace
/// bundle — which is the one answer an explicit `--keep-mine` must never give. Here the refusal is
/// structural: the only success is a recorded conflict, and every other state (whatever the row's
/// kind, followed or not) refuses toward the merge.
///
/// The states that reach the refusal each had their own silent wrong answer: an up-to-date copy and
/// a plain draft reported success and did nothing, a clean copy that was merely behind APPLIED the
/// team's version (the opposite of the flag), and a live divergence committed the draft over changes
/// the merge was about to land.
pub(crate) fn escape_one(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
) -> Result<PullSkill, ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    let sp = ctx.layout.published(skill_id);
    let skill_id = skill_id.as_str();
    let sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    // The placement map is read only where an escape is actually about to write — the refusal
    // needs nothing but the name, and must not turn into a different error for a row whose map
    // cannot be read.
    if let Some(cs) = doc::read_doc::<topos_types::persisted::ConflictState>(ctx.fs, &sp.conflict)?
    {
        let map: PlacementMap = read_map_required(ctx, &sp)?;
        // A MARKED record is a merge an exit already CONCLUDED, interrupted before its clear
        // landed — never a stopped merge this command may act on afresh. A marked Escape IS this
        // command's own crashed run: finish it and return its row. A marked Reset/Merge is
        // finished too, and then there is no stopped merge left — refuse like any other resolved
        // state, because committing the original draft over a landed conclusion is exactly the
        // loss the mark exists to prevent.
        if cs.concluded.is_some() {
            if let Some(row) = super::merge_resolve::finish_concluded(
                DivergedWitness(()),
                ctx,
                skill_id,
                &sp,
                &sync,
                &lock,
                &map,
                &cs,
            )? {
                return Ok(row);
            }
        } else {
            // An UNMARKED record is a live stopped merge: this exit acts on it as it stands.
            // A witness mint site — guarded: a `conflict.json` only ever exists for an author
            // who diverged (a follower never reaches merge code, so never records one).
            return super::merge_resolve::escape_recorded(
                DivergedWitness(()),
                ctx,
                skill_id,
                &sp,
                &sync,
                &lock,
                &map,
                &cs,
            );
        }
    }
    Err(ClientError::NoStoppedMerge {
        skill: lock.name,
        global: !ctx.layout.is_project_scope(),
    })
}

/// Bring one followed skill current (the sweep and the explicit-accept path).
///
/// `inv` is [`Invocation::Sweep`] for the bare session-start sweep or [`Invocation::Accept`] for a
/// targeted `topos pull <skill>` (which also releases a `held` pin).
pub(crate) fn sync_one(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    follow: &FollowContext,
    inv: Invocation,
) -> Result<PullSkill, ClientError> {
    sync_one_with(ctx, skill_id, follow, inv, None)
}

/// [`sync_one`] with an already-resolved sync target — the delivery-driven reconcile's entry: the
/// per-workspace delivery answered "what should this device have" in ONE call, so the per-skill
/// pointer GET is skipped and the served `(generation, version_id)` is adopted directly. `None`
/// keeps the conditional per-skill GET (the targeted-pull path). Everything downstream — the scope
/// check, the four-state plan, fetch + re-verify, consent, materialization — is identical: the
/// target's integrity story is the content-addressed version id re-verified by digest on apply,
/// however the pointer arrived.
pub(crate) fn sync_one_with(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    follow: &FollowContext,
    inv: Invocation,
    target: Option<&topos_types::WireCurrentRecord>,
) -> Result<PullSkill, ClientError> {
    sync_one_planned(ctx, skill_id, follow, inv, target, None)
}

/// How a sync derives its placement plan — the scope seam: `None` keeps the person-scope default
/// ([`placement::plan_for_skill`] — home harness dirs); the manifest reconcile passes a
/// project-scope planner for items a project `topos.toml` demands (the bytes belong INSIDE that
/// checkout). The fn is re-invoked whenever the engine re-plans (the adoption-lapse correction),
/// so both plan sites stay scope-consistent.
pub(crate) type PlanFn<'a> =
    dyn Fn(&Ctx<'_>, &str, &Lock, &PlacementMap) -> crate::placement::PlacementPlan + 'a;

/// [`sync_one_with`] with an explicit placement-plan source (see [`PlanFn`]).
pub(crate) fn sync_one_planned(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    follow: &FollowContext,
    inv: Invocation,
    target: Option<&topos_types::WireCurrentRecord>,
    plan_fn: Option<&PlanFn<'_>>,
) -> Result<PullSkill, ClientError> {
    let explicit = inv.is_explicit();
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    let sp = ctx.layout.published(skill_id);
    let skill_id = skill_id.as_str();
    let mut sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    let mut map: PlacementMap = read_map_required(ctx, &sp)?;
    let name = lock.name.clone();

    // An unresolved conflict is on record. A sync heals a crashed materialization and re-discloses
    // the block WITHOUT re-merging (the conflict draft already consumed `current`). The escape that
    // RESOLVES it is [`escape_one`], which never enters here.
    if let Some(cs) = doc::read_doc::<topos_types::persisted::ConflictState>(ctx.fs, &sp.conflict)?
    {
        // FIRST: a record an exit already MARKED concluded is not a block at all — finish that
        // exit and go on. Before the re-disclosure, which would otherwise re-settle the docs to a
        // work tree the landed conclusion has already replaced. A marked Escape finishes to its
        // own `Merged` row (this sweep's answer for the bundle); a marked Reset/Merge finishes to
        // an unblocked bundle and the sweep proceeds over it normally. The mark is the ONLY thing
        // read here: an unmarked record is a live block whatever the other documents say.
        if cs.concluded.is_some() {
            if let Some(row) = super::merge_resolve::finish_concluded(
                DivergedWitness(()),
                ctx,
                skill_id,
                &sp,
                &sync,
                &lock,
                &map,
                &cs,
            )? {
                return Ok(row);
            }
            // The finisher moved the docs (a reset converges placements and rewrites sync/map) —
            // re-read what the rest of this run stands on. The lock is untouched by both
            // finishing arms that reach here.
            sync = read_required(ctx, &sp.sync, "sync.json")?;
            map = read_map_required(ctx, &sp)?;
        } else {
            // An UNMARKED record is a live block: heal whatever the recording crash left and
            // re-disclose it as it stands.
            super::merge_resolve::recover_resolution(ctx, &sp, &sync, &lock, &map, &cs)?;
            return super::merge_resolve::conflicted_row_from_state(ctx, &name, &sync, &map, &cs);
        }
    }

    // A never-received followed skill (the first-receive baseline the reconcile lays: nothing observed
    // yet, no placement). It applies like any other pending version — the recipe row IS the consent —
    // so this only decides how the run READS: an `installed` row rather than a fast-forward, plus the
    // adoption-lapse correction below. Captured BEFORE checkForUpdates mutates `observed`.
    let first_receive = is_never_received(&sync);

    // The conditional-GET validator: what the client currently holds (its observed generation AND the commit
    // it names) — so a record reusing `(epoch,seq)` for a different commit is returned, not 304'd. `None`
    // for the never-received baseline (no observed commit yet) → an unconditional first GET.
    let known = known_current(&sync)?;

    // ---- checkForUpdates ----
    let fetched = match target {
        // The delivery already answered for this workspace — no per-skill GET, no conditional
        // validator needed (the snapshot is fresher than any cache header).
        Some(rec) => Ok(PointerFetch::Record(rec.clone())),
        None => ctx.plane.get_current(skill_id, known),
    };
    match fetched {
        Ok(PointerFetch::NotModified) => {}
        Ok(PointerFetch::Record(rec)) => {
            // Scope-check the served record (a mis-scoped record is a malformed response, not the target).
            let Some(version_id) = scoped_version_id(&rec, skill_id, &follow.workspace_id) else {
                return Err(ClientError::WireInvalid(format!(
                    "the current pointer for {skill_id} is scoped to a different workspace/skill"
                )));
            };
            // The served record IS the sync target. Adopt it whenever it differs from what we hold — in ANY
            // direction (a server restore is a legitimate team rollback). The move is durable NOW (it must
            // survive a failed apply as the retry target), independent of whether the apply succeeds.
            let version_hex = to_hex(&version_id);
            if sync.observed != rec.record.generation || sync.observed_version_id != version_hex {
                sync.observed = rec.record.generation;
                sync.observed_version_id = version_hex;
                doc::write_doc(ctx.fs, &sp.sync, &sync)?;
            }
        }
        Err(PlaneError::NotFound) => return Ok(state_row(&name, &sync, PullAction::UpToDate)),
        // The server refuses this build outright. A targeted update says so (nothing about this
        // skill will move until the binary is replaced); a sweep keeps converging from the local
        // store, exactly as it does for a plane that cannot be read — the delivery lane's own
        // warning is what carries the fix.
        Err(PlaneError::UpdateRequired { min }) => {
            if explicit {
                return Err(ClientError::UpdateRequired { min });
            }
        }
        Err(PlaneError::Unavailable(m) | PlaneError::Unreachable(m)) => {
            // Targeted accept: surface the failure. A bare sweep falls through to drive `applied`
            // toward `observed` from the LOCAL store — a pending apply whose target is already local
            // still completes when the plane is unreachable; one that needs a fetch fails per-skill
            // below, never a false UpToDate. (The escape's own offline-capable no-deadlock path is
            // the recorded-conflict arm above, which returns before any of this.)
            if explicit {
                return Err(ClientError::Plane(m));
            }
        }
        // A structurally malformed served response is a wire-validation error (content addressing is the
        // integrity story; a garbled body cannot be the target).
        Err(PlaneError::Malformed(m)) => return Err(ClientError::WireInvalid(m)),
    }

    // ---- plan: classify via the kernel's four-state transition, driving toward `observed` ----
    // The placement TARGET SET is recomputed each sync (a newly detected harness / newly true
    // coverage adds a target; a record only ever leaves through an explicit verb). The reconciled
    // map carries every recorded placement — appended targets are never-materialized until an apply.
    // Generations tell WHEN; versions tell WHAT. A record whose version differs from the
    // applied base is a PIN that moved (the row's, or the project lock's) — the transition must
    // read it as behind, or a changed pin would wait for the SERVER to move before landing.
    let version_diverged = target
        .is_some_and(|t| !lock.base_commit.is_empty() && t.record.version_id != lock.base_commit);
    let applied_eq_observed = sync.applied == sync.observed && !version_diverged;
    // A CONNECTED SERVER never reaches this engine. It holds no version to fetch and no object
    // store to fetch into — its whole delivery is the document the reconcile records — so an
    // arrival here is a caller that believed a record was something its own kind marker denies.
    // Fail closed: refuse, rather than plan a placement for bytes that do not exist.
    if plan_fn.is_none() && planning_kind(ctx, skill_id, &map, &name)?.is_mcp() {
        return Err(server_has_no_versions(&name));
    }
    let make_plan = |map: &PlacementMap| match plan_fn {
        Some(f) => f(ctx, skill_id, &lock, map),
        None => placement::plan_for_skill(ctx, skill_id, &lock, map),
    };
    let plan = make_plan(&map);
    let map = placement::reconcile_map(&map, &plan);
    let managed = placement::managed_indices(&map, &plan);
    let work = compute_work(ctx, &map, &lock, &name)?;
    let work_eq_base = match &work.state {
        // Nothing on disk to clobber (a clean install) / the work tree matches the locked base.
        WorkState::Absent | WorkState::CleanAtBase => true,
        WorkState::Draft { .. } => false,
        WorkState::Unscannable => {
            // An unreadable placement matters only if there is a pending update; never silently
            // fast-forward over it (fail closed), but if already current there is nothing to do.
            if applied_eq_observed {
                return Ok(state_row(&name, &sync, PullAction::UpToDate));
            }
            return Err(ClientError::PlacementUnsupported {
                reason: "the placement cannot be read; refusing to fast-forward over it".into(),
            });
        }
    };
    match sync::decide_state(work_eq_base, applied_eq_observed) {
        // ① CURRENT / ③ DRAFT — no pending remote update (a draft is surfaced by `list`/`diff`, never
        // nagged). A missing or stale MANAGED placement (a newly added target, a copy behind the
        // applied version) is still converged from the LOCAL store — this is where a new agent's dir
        // (or a scope change) gets its bytes without waiting for the next served version.
        sync::SyncStatus::Current | sync::SyncStatus::Draft => {
            // The converge COMMITS an updated map — every later committing step this run must see
            // it (a stale map handed to the fan-out's materialize would erase the baselines the
            // converge just recorded, and the next scan would read the installed dir as foreign).
            let converged =
                converge_placements(ctx, &sp, skill_id, &lock, &sync, &map, &managed, &work)?;
            let map = converged.map;
            // A LOCAL DRAFT rides EVERY row this arm produces, and the flag is computed HERE —
            // above the arm's early returns — because it used to be computed below them. A settled
            // draft that fanned out, and a bundle whose folder this run re-created, both returned
            // `draft: false` while the person's edits stood: `list` called them drafts, `status`
            // counted them, and the update receipt said otherwise about the same machine. The
            // three surfaces read one machine and now say one thing about it.
            let drafted = matches!(work.state, WorkState::Draft { .. });
            // The SETTLED-DRAFT fan-out: a draft whose content is unchanged since the previous
            // run's observation is copied onto the bundle's other placements in this scope (their
            // baselines advance with it); an unsettled draft only updates the observation — a
            // mid-edit file never spreads. Runs only here — no pending remote update, no freeze
            // (true competitors already errored in compute_work).
            if let WorkState::Draft { scanned } = &work.state {
                let synced = settle_or_spread(
                    ctx, &sp, skill_id, &lock, &sync, &map, &managed, &work, scanned,
                )?;
                if !synced.is_empty() {
                    let mut row = synced_row(
                        &name,
                        &sync,
                        u32::try_from(synced.len()).unwrap_or(u32::MAX),
                    );
                    row.destinations = synced;
                    // A folder the converge CREATED in the SAME run (a hand-deleted dir healed
                    // from the local store) stood absent when the fan-out chose its targets, so
                    // the fan-out skipped it and the `synced` column cannot name it. The row
                    // states it as its second fact — nothing this run wrote goes unnamed.
                    if !converged.created.is_empty() {
                        row.note = Some(also_line("installed", &converged.created));
                    }
                    row.draft = drafted;
                    return Ok(row);
                }
            } else if sync.draft_observed.is_some() {
                // The draft resolved outside an apply (reverted by hand): the stale observation
                // must not let a FUTURE identical edit spread on its first sighting.
                let cleared = SyncState {
                    draft_observed: None,
                    ..sync.clone()
                };
                doc::write_doc(ctx.fs, &sp.sync, &cleared)?;
            }
            // A placement the converge CREATED (a folder materialized where none stood — a
            // hand-deleted dir healed, a re-added row re-placed) is an INSTALL and the row says
            // so; a stale copy it REWROTE to the version this machine holds is a REFRESH, which
            // is not a first materialization and never borrows that word. Both are bytes that
            // moved: "up to date" may only claim itself when nothing changed on disk.
            if !converged.created.is_empty() {
                let mut row = state_row(&name, &sync, PullAction::Installed);
                row.destinations = converged.created;
                // A refresh alongside a creation rides the same row: the install leads (a folder
                // appeared), the caught-up copies are named as the second fact rather than
                // relabelled `installed` or dropped.
                if !converged.refreshed.is_empty() {
                    row.note = Some(also_line("updated", &converged.refreshed));
                }
                row.draft = drafted;
                return Ok(row);
            }
            if !converged.refreshed.is_empty() {
                let mut row = state_row(&name, &sync, PullAction::Refreshed);
                // Every folder holding the applied version, not only the ones rewritten — a
                // bundle in two folders that needed one rewrite still lives in two folders.
                row.destinations = converged.at_version;
                row.draft = drafted;
                return Ok(row);
            }
            let mut row = state_row(&name, &sync, PullAction::UpToDate);
            row.draft = drafted;
            return Ok(row);
        }
        // ② BEHIND / ④ DIVERGED — an update is pending; fall through to fetch + apply.
        sync::SyncStatus::Behind | sync::SyncStatus::Diverged => {}
    }

    // A held skill (a deliberate go-back pin) suppresses exactly one auto fast-forward; an explicit
    // `topos pull <skill>` falls through and applies, and the successful apply clears `held` — so a FAILED
    // explicit resume (an error before the apply) leaves the hold intact.
    if sync.held && !explicit {
        return Ok(state_row(&name, &sync, PullAction::Held));
    }

    // Fetch + record the target durably (the integrity gate: write_bundle + commit re-derive the id and
    // refuse a lying ref; render-on-read re-hashes). Backfill any missing ancestors so `commit` has parents.
    // A failed integrity check is a loud per-skill integrity ERROR, not a silent skip.
    let target_commit = super::parse_hex32(&sync.observed_version_id)?;
    let store = Store::open(&sp.store)?;
    let mut written = WriteBatch::default();
    // The ONE place a sync reaches the network for BYTES — every state above returned without
    // fetching, so the label is honest wherever it prints. `_if_idle` because the reconcile that
    // usually drives this already named the item (`updating <name> (2 of 3)`), which says more.
    let fetching = crate::progress::phase_if_idle(ctx.progress, &format!("downloading {name}"));
    let target_digest = ensure_local(ctx, &store, skill_id, target_commit, 0, &mut written)?
        .unwrap_or_else(|| unreachable!("depth-0 ensure_local errors instead of shallow-stopping"));
    // Once, after the whole backfill — exactly the versions THIS op wrote (plus the target's own set when
    // already local), durable before any JSON records the target. Never the whole store: the per-pull
    // fsync cost is bounded by the fetched bytes, not lifetime history.
    fsync_batch(ctx, &written)?;
    // The bytes are local and durable — the rest of this sync is disk work.
    drop(fetching);
    // A digest mismatch on the rendered bytes is a loud integrity ERROR (content addressing is the integrity
    // story) — corruption evidence, never a transient skip.
    let bundle = store.render_verified(target_commit, target_digest)?;
    let target_digest_hex = to_hex(&target_digest);
    // A first-receive ADOPTION recorded for an EARLIER served version LAPSES here — the plan runs
    // before the fetch, so only now can the reservation's digest be checked against the resolved
    // target. The reservation meant "adopt the byte-identical occupant in place"; a target that
    // resolved to DIFFERENT bytes no longer describes it, and proceeding would wedge the apply on
    // the materializer's never-clobber refusal. Clear the stale record and re-plan: the re-choose
    // suffixes past the occupied dir, the reservation swap puts the record right, and the apply
    // proceeds with the fresh state (the correction only touches never-materialized reservations,
    // which scan Foreign/Absent either way — the base classification above is unchanged).
    let (map, managed, work) = if first_receive
        && managed.iter().any(|&i| {
            map.placement_state[i].materialized_sha.is_none()
                && map.placement_state[i]
                    .pre_existing_sha
                    .as_deref()
                    .is_some_and(|sha| sha != target_digest_hex)
        }) {
        let mut corrected = map.clone();
        for &i in &managed {
            let st = &mut corrected.placement_state[i];
            if st.materialized_sha.is_none()
                && st
                    .pre_existing_sha
                    .as_deref()
                    .is_some_and(|sha| sha != target_digest_hex)
            {
                st.pre_existing_sha = None;
            }
        }
        let plan = make_plan(&corrected);
        let map = placement::reconcile_map(&corrected, &plan);
        let managed = placement::managed_indices(&map, &plan);
        let work = compute_work(ctx, &map, &lock, &name)?;
        (map, managed, work)
    } else {
        (map, managed, work)
    };
    // "At target" is now an EVERYWHERE fact: every managed placement must already hold the target's
    // bytes for the no-swap heal — a partial landing (a crash mid-loop, a newly added dir) stays a
    // CleanForward, whose apply loop skips the landed dirs and swaps only the rest.
    let work_eq_target = all_managed_at_target(&work.scans, &managed, &target_digest_hex);

    // ---- apply ----
    let t = ApplyTarget {
        commit: target_commit,
        digest_hex: &target_digest_hex,
        bundle: &bundle,
    };
    match sync::refine_after_fetch(work_eq_base, work_eq_target) {
        ApplyClass::AlreadyAtTarget => {
            // The bytes already equal the target (a crash after a prior swap, or an idempotent re-pull):
            // advance `applied` with NO swap, never a false DIVERGED — and no spurious draft snapshot.
            heal_forward(ctx, &sp, &map, &managed, &lock, &sync, &t)?;
            // Base AND bytes were already at the target: the advance was generation BOOKKEEPING
            // (a served current moving past an unchanged pin), not an update — a receipt that
            // announced it again every run would count standing still as work.
            if !first_receive && lock.base_commit == to_hex(&target_commit) {
                let mut row = applied_row(&name, &sync, target_commit);
                row.action = PullAction::UpToDate;
                return Ok(row);
            }
            let mut row = applied_row(&name, &sync, target_commit);
            mark_installed(ctx, &mut row, first_receive, &map, &managed);
            Ok(row)
        }
        ApplyClass::CleanForward => {
            // A swap happens only here, and only when `work_eq_base` (a clean follower) — so a swap never
            // overwrites a local draft. Every situation a clean forward can reach APPLIES: the recipe row
            // is the standing pre-authorization, an explicit accept is the direct local command, and a
            // review-required workspace only ever serves an already-approved `current`. The kernel stays
            // the authority for that claim, ASKED here rather than re-decided — and asked in every
            // build, not just a debug one. It is the last coupling between this engine and the
            // consent kernel on the apply path, it costs one table lookup per skill, and the thing
            // it guards is bytes landing in an agent's folder: a decision table that ever stopped
            // agreeing must stop the apply, not be compiled out of the release the apply ships in.
            if !topos_core::consent::decide(situation_for(follow, explicit)).applies_bytes() {
                return Err(ClientError::Corrupt(format!(
                    "the consent kernel refused a clean forward apply for {name}"
                )));
            }
            // What this fast-forward REPLACES, when the target undoes it — decided while the lock
            // still names the replaced version (the apply below rewrites it).
            let undone = (!first_receive)
                .then(|| {
                    undone_version_note(&store, &lock, &name, target_commit, &target_digest_hex)
                })
                .flatten();
            apply_forward(ctx, &sp, &map, &managed, &lock, &sync, skill_id, &t)?;
            let mut row = applied_row(&name, &sync, target_commit);
            mark_installed(ctx, &mut row, first_receive, &map, &managed);
            if undone.is_some() {
                row.note = undone;
            }
            Ok(row)
        }
        ApplyClass::Diverged => {
            // ④ a GENUINE local draft vs a newer remote — resolve it (author-side three-way merge / escape).
            // DIVERGED implies `work != base`, which can only hold for a Draft work tree (the ONE
            // edited copy — several divergent copies froze typed back in compute_work).
            let WorkState::Draft { scanned } = &work.state else {
                // Unreachable by construction (Absent / CleanAtBase are equal to base; Unscannable
                // returned above) — fail closed rather than resolve a divergence whose bytes this
                // run cannot name.
                return Err(ClientError::Corrupt(format!(
                    "{name} reads as diverged with no draft working tree"
                )));
            };
            // The structural author-only gate: the witness is minted ONLY here.
            super::merge_resolve::resolve_diverged(
                DivergedWitness(()),
                ctx,
                skill_id,
                &sp,
                &sync,
                &lock,
                &map,
                scanned,
                &bundle,
                target_commit,
            )
        }
    }
}

/// Abbreviate the config-file paths a receipt row's per-agent states carry (`~` under the home) —
/// receipt rows only; the wire report and the delivery cache keep the absolute paths.
pub(crate) fn prettify_state_files(
    ctx: &Ctx<'_>,
    states: &mut [topos_types::results::McpAgentState],
) {
    for s in states {
        if let Some(f) = s.file.take() {
            s.file = Some(super::inventory::pretty(ctx, Path::new(&f)));
        }
    }
}

/// The record's kind for every path that PLANS PLACEMENT — the durable chain
/// ([`crate::bundle_kind::classify`]) with this engine's fail-closed rule applied ONCE, here, so
/// the three planning sites cannot drift apart on the arm that matters.
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] when nothing answers over an EMPTY placement map (see
/// [`kind_indeterminate`]). Over a map that already holds placements the record is provably
/// dir-placed, so an unanswered chain reads as an ordinary skill.
fn planning_kind(
    ctx: &Ctx<'_>,
    skill_id: &str,
    map: &PlacementMap,
    name: &str,
) -> Result<BundleKind, ClientError> {
    match crate::bundle_kind::classify(ctx, skill_id, &map.placements) {
        RecordKind::Known(kind) => Ok(kind),
        RecordKind::Indeterminate if map.placements.is_empty() => Err(kind_indeterminate(name)),
        RecordKind::Indeterminate => Ok(BundleKind::Skill),
    }
}

/// The typed refusal every version-shaped path answers over a CONNECTED SERVER. It is a belt, not
/// a door: every verb that could name one refuses in its own words first, and reaching here means
/// a record's kind marker and its caller disagree.
fn server_has_no_versions(name: &str) -> ClientError {
    ClientError::PlacementUnsupported {
        reason: format!(
            "'{name}' is an MCP server bundle — this machine holds the one revision it was given \
             and no version history behind it, so there is nothing here to apply. `topos update` \
             converges its config entries."
        ),
    }
}

/// The typed refusal every empty-map + unknowable-kind path answers: the record may be a
/// config-placed (mcp) bundle whose kind evidence was lost, and materializing its bytes into
/// skill dirs on a guess is exactly the corruption the marker exists to prevent.
fn kind_indeterminate(name: &str) -> ClientError {
    ClientError::PlacementUnsupported {
        reason: format!(
            "{name}'s store records no placements and its bundle kind cannot be determined (no \
             kind marker, and no manifest row declares one) — refusing to plan skill placement \
             for what may be a config-placed MCP bundle; a full `topos update` sweep restores \
             the record"
        ),
    }
}

/// `topos pull <skill>@<ref>` — install an older version's exact bytes locally (a deliberate go-back),
/// set `held` to suppress the next auto fast-forward, and **do NOT change the `observed` target** (the
/// team's `current` is untouched; the go-back is a local pin). The target must be present in this skill's
/// LOCAL store (the versions this client has fetched/committed); a short prefix resolves against that same
/// local set, so a no-match prefix reports the same typed go-back error a full unknown id does.
pub(crate) fn go_back(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    vref: &super::VersionRef,
) -> Result<PullSkill, ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    let sp = ctx.layout.published(skill_id);
    let skill_id = skill_id.as_str();
    let sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    let map: PlacementMap = read_map_required(ctx, &sp)?;
    let name = lock.name.clone();
    // A CONNECTED SERVER has no version history here to go back through (see the sweep's own
    // arm above); the doors that could name one refuse first, and this is the belt.
    if planning_kind(ctx, skill_id, &map, &name)?.is_mcp() {
        return Err(server_has_no_versions(&name));
    }
    let plan = placement::plan_for_skill(ctx, skill_id, &lock, &map);
    let map = placement::reconcile_map(&map, &plan);
    let managed = placement::managed_indices(&map, &plan);

    // Resolve the ref against the versions this client holds LOCALLY (the go-back can only install bytes it
    // already has). A prefix that matches no local version reports the same typed error a full unknown id does.
    let store = Store::open(&sp.store)?;
    let known: Vec<String> = store.list_versions()?.iter().map(|v| to_hex(v)).collect();
    let target = super::resolve_version_ref(&known, vref)?.ok_or_else(|| {
        ClientError::UnknownGoBackVersion {
            version: vref.shown(),
        }
    })?;
    let target_hex = to_hex(&target);

    // Snapshot-on-touch FIRST. A go-back is an explicit OVERWRITE of the placements, so it must never
    // silently lose an unsaved local edit (the never-clobber rail applies here exactly as in the
    // sweep) — EVERY distinct edited copy is committed to the sidecar store before any swap (a
    // go-back deliberately converges divergent copies, so it snapshots them all rather than freezing).
    // An unreadable placement fails closed.
    snapshot_all_modified(ctx, &sp, &lock, &map, "a go-back")?;

    // The target's bytes must be readable from the local store (a previously-applied version); a
    // present-but-unreadable version (e.g. a dangling ref) is refused with the typed go-back error
    // rather than surfacing a raw integrity error.
    let target_digest = store_bundle_digest_opt(&store, target)?.ok_or_else(|| {
        ClientError::UnknownGoBackVersion {
            version: target_hex.clone(),
        }
    })?;
    let bundle = store.render_verified(target, target_digest)?;
    // The go-back writes nothing new into the store (the draft snapshot above synced its own set), but the
    // docs below re-record `target` as applied — make ITS objects + ref durable first (a version fetched
    // by a pull that crashed before its fsync can be present-and-renderable yet not durable). Bounded by
    // one version's tree, never the whole store.
    fsync_batch(ctx, &store.version_durability(&target)?)?;
    let target_digest_hex = to_hex(&target_digest);

    // `ExplicitLocalPull` → `MaterializeLocal`: a direct local command authorizes installing these bytes;
    // the digest is re-bound on materialize. The `observed` target is untouched (the team's current does
    // not change); `applied` drops to the genesis sentinel so a later bare `pull` sees `applied != observed`
    // (② behind), which — while `held` — reports Held and, on an explicit `pull`, fast-forwards to current.
    debug_assert!(
        topos_core::consent::decide(topos_core::consent::Situation::ExplicitLocalPull)
            .applies_bytes()
    );
    let next_sync = SyncState {
        schema_version: sync.schema_version,
        observed: sync.observed,
        observed_version_id: sync.observed_version_id.clone(),
        applied: APPLIED_BEHIND_CURRENT,
        base_commit: target_hex.clone(),
        work_hash: target_digest_hex.clone(),
        held: true,
        // A go-back installs an OLD version whole — no draft stands.
        draft_observed: None,
    };
    let next_lock = lock_from_bundle(&lock, target, &bundle);
    let report = materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id,
            target_indices: &managed,
            bundle: &bundle,
            next_map: next_map(&map, target, &target_digest_hex),
            next_lock: &next_lock,
            next_sync: &next_sync,
            sp: &sp,
            // Every edited copy was snapshotted above; the seam stays armed for the crash window
            // between that snapshot and the swap (idempotent — a re-snapshot of the same bytes is
            // a no-op commit).
            snapshot: Some(&|scanned: &ScannedBundle| {
                snapshot_draft(ctx, &sp, &lock, scanned).map(|_| ())
            }),
            takeover: None,
            self_ignore: ctx.layout.is_project_scope(),
            expected: None,
            project_root: ctx.layout.project_root(),
        },
    )?;
    log_apply(ctx, skill_id, "pull-goback", target, &report);
    Ok(PullSkill {
        skill: name,
        // The workspace provenance is stamped by the pull aggregator (`pull.rs`), which owns the
        // follow-state; a go-back target may be an unfollowed local copy, so it can honestly be `None`.
        workspace_id: None,
        observed: next_sync.observed,
        applied: next_sync.applied,
        action: PullAction::Held,
        merge: None,
        synced_placements: None,
        destinations: Vec::new(),
        kept: Vec::new(),
        display: None,
        note: None,
        scope: None,
        harnesses: Vec::new(),
        kind: None,
        // A go-back RESTORES the recorded bytes over the placement, so nothing of the person's is
        // left unshared in it.
        draft: false,
        narrowed: None,
    })
}

/// `update --reset <skill>` — DISCARD the local draft, restoring the followed `current` (an imported
/// skill's adopted origin snapshot). The draft is snapshotted into the sidecar store FIRST (never lost —
/// recoverable), then the base bytes are re-materialized over the placement. `observed`/`applied` are
/// untouched (the team's current did not move) and `held` stays `false`, so a later sweep sees the skill
/// current again. A pristine working tree is a clean no-op (nothing to discard). A RECORDED merge
/// conflict is cleared with the draft it described — the record AND the marked-up copy in the scope's
/// `conflicts/` dir (the reset resolves the divergence the team's way) — so publish is not left blocked
/// by a conflict whose draft is gone. After a conflict `lock.base_commit` IS the team's version, which
/// is what makes `--reset` on a blocked bundle mean "take theirs, drop mine".
///
/// `sel` narrows the REWRITE to the ONE copy a `-a`/`--dest` selection names — the symmetric
/// counterpart of a per-copy publish, and the other way out of a divergent-copies freeze. What it
/// narrows is only which folders are written back to base: every edited copy is STILL snapshotted
/// into the store first (the loss rail is not a per-copy thing — bytes that survive on disk are
/// still bytes nobody wrote down), and the surviving copy stays exactly as it is, which makes it
/// the single ordinary draft once the reset lands. A narrowed reset therefore does not usually end
/// a stopped merge — but that is the settled state talking, not the selector: ANY reset concludes
/// the merge exactly when every recorded copy proves settled (see [`every_copy_settled`]), and
/// leaves the record live when one does not.
///
/// Returns what the receipt needs to say about what this reset LEFT: the workbench folder a hand
/// merge survives in (this command never read it, so it never deletes it), and where the reset
/// leaves any merge that was stopped here.
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] on an unscannable placement; a store / io / integrity
/// failure; [`ClientError::SelectionRefused`] when the selection names no edited copy.
pub(crate) fn reset_to_base(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    sel: &super::Selection,
) -> Result<ResetLanded, ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    let sp = ctx.layout.published(skill_id);
    let sid = skill_id.as_str();
    let sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    let map: PlacementMap = read_map_required(ctx, &sp)?;
    // Same rule as the go-back: a connected server never reaches here (the verb refuses the kind
    // at its own door), and an empty-map record with no kind evidence fails CLOSED rather than
    // materializing on a guess.
    if planning_kind(ctx, sid, &map, &lock.name)?.is_mcp() {
        return Err(server_has_no_versions(&lock.name));
    }
    let plan = placement::plan_for_skill(ctx, sid, &lock, &map);
    // The selection is resolved against the RECORDED map, before reconcile: a reconcile only ever
    // appends targets or replaces never-materialized reservations, so an EDITED copy keeps its
    // index — and resolving against the dir rather than the index below makes that structural.
    let picked = if sel.is_empty() {
        None
    } else {
        Some(super::dest_select::select_copy(ctx, sel, &lock.name, &map)?)
    };
    let map = placement::reconcile_map(&map, &plan);
    let managed = placement::managed_indices(&map, &plan);
    // The apply set: every managed placement, or the ONE the selection named — located by its DIR
    // in the reconciled map, so no index arithmetic can drift.
    let managed = match &picked {
        None => managed,
        Some(p) => map
            .placements
            .iter()
            .enumerate()
            .filter(|(_, dir)| std::path::Path::new(dir) == p.dir)
            .map(|(i, _)| i)
            .collect(),
    };

    let base = super::parse_hex32(&lock.base_commit)?;
    let base_digest = super::parse_hex32(&lock.bundle_digest)?;
    let base_digest_hex = lock.bundle_digest.clone();

    // The record this reset may CONCLUDE, read once under the flock. Unreadable reads as none —
    // an unreadable record cannot be marked, and the clear below still removes the file either
    // way (it names no folder, so it removes none).
    let recorded: Option<topos_types::persisted::ConflictState> =
        doc::read_doc(ctx.fs, &sp.conflict).ok().flatten();

    // Snapshot-on-touch FIRST — a reset OVERWRITES the placements, so EVERY distinct edited copy is
    // committed to the store (recoverable) before any swap. `update --reset` is also the disclosed
    // way OUT of the divergent-copies freeze, so it never freezes itself — it snapshots each copy and
    // converges them all back to base. An unreadable placement fails closed rather than risk a clobber.
    snapshot_all_modified(ctx, &sp, &lock, &map, "a reset")?;

    let store = Store::open(&sp.store)?;
    let bundle = store.render_verified(base, base_digest)?;
    fsync_batch(ctx, &store.version_durability(&base)?)?;

    // The restored state: base bytes on the placements, work_hash back at the base digest, held cleared,
    // observed/applied unchanged (the team's current never moved).
    let next_sync = SyncState {
        schema_version: sync.schema_version,
        observed: sync.observed,
        observed_version_id: sync.observed_version_id.clone(),
        applied: sync.applied,
        base_commit: lock.base_commit.clone(),
        work_hash: base_digest_hex.clone(),
        held: false,
        draft_observed: None,
    };
    let report = materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id: sid,
            target_indices: &managed,
            bundle: &bundle,
            next_map: next_map(&map, base, &base_digest_hex),
            next_lock: &lock,
            next_sync: &next_sync,
            sp: &sp,
            snapshot: Some(&|scanned: &ScannedBundle| {
                snapshot_draft(ctx, &sp, &lock, scanned).map(|_| ())
            }),
            takeover: None,
            self_ignore: ctx.layout.is_project_scope(),
            expected: None,
            project_root: ctx.layout.project_root(),
        },
    )?;
    // A recorded merge conflict describes the divergence this reset just DISCARDED — clear the
    // block and the marked-up copy it named (idempotent; absent is fine), or publish would stay
    // refused by a conflict whose draft no longer exists. Cleared AFTER the placements landed,
    // mirroring the escape's order.
    //
    // But only once NOTHING is left unresolved. A `--dest` reset narrows to ONE copy so the others
    // keep their edits — and a copy still holding its edits is a copy still holding this merge, so
    // clearing the record there would unblock publishing while the merge stands. In git, resolving
    // one path does not end a merge; only the commit does, and it refuses while anything is
    // unresolved. The scan reads every recorded copy, and an unreadable one keeps the block (fail
    // toward the gate, never through it).
    //
    // The removal is `Unread`: this command never looked inside the workbench, so a hand merge in
    // it is left where the person can see it and named on the receipt.
    let settled = every_copy_settled(ctx, &sp);
    let hand_merge = if settled {
        // The reset CONCLUDED the merge, and only here is that true — the proof just passed, so
        // the mark goes down before the clear and a crash between the two still reads as a
        // conclusion to finish. Full and narrowed resets are identical in this: what ends a merge
        // is the settled state, never the shape of the command. A reset that does NOT settle
        // every copy writes no mark at all, so the record stays LIVE — re-disclosed, and both
        // ways out still work — rather than becoming a conclusion nothing can finish (the apply
        // set is the current PLAN's, and a recorded copy outside the plan is never written, so a
        // pre-materialize mark could strand a block no run could clear). A mark already there is
        // left as it is: a crashed exit's own conclusion is not this command's to rewrite.
        if let Some(cs) = &recorded
            && cs.concluded.is_none()
        {
            super::merge_resolve::mark_concluded(
                ctx,
                &sp,
                cs,
                topos_types::persisted::ConcludedExit::Reset,
            )?;
        }
        super::merge_resolve::clear_conflict(ctx, &sp, super::merge_resolve::Workbench::Unread)?
    } else {
        None
    };
    log_apply(ctx, sid, "update-reset", base, &report);
    // WHERE this leaves the merge, for the receipt to say out loud. A narrowed reset that settled
    // only its own copy leaves the block STANDING — the person who just watched one folder go back
    // to the team's version is the reader most likely to believe the whole thing is over.
    Ok(ResetLanded {
        hand_merge,
        merge: match (&recorded, settled) {
            (None, _) => ResetMerge::NoneStood,
            (Some(_), true) => ResetMerge::Concluded,
            (Some(_), false) => ResetMerge::StillStopped,
        },
    })
}

/// What a [`reset_to_base`] left behind, for its receipt.
#[derive(Debug)]
pub(crate) struct ResetLanded {
    /// The folder holding a by-hand merge this reset did NOT read and so did not delete, as a
    /// receipt spells it. `None` when nothing survived there.
    pub hand_merge: Option<String>,
    /// Where the reset leaves a merge that had stopped on this bundle.
    pub merge: ResetMerge,
}

/// Where a reset leaves a stopped merge — the fact only the command that ran the settled-state
/// proof can state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResetMerge {
    /// No merge was recorded here at all: the ordinary reset, and the receipt says nothing.
    NoneStood,
    /// A record stood and STILL stands — this reset settled its own copy and left another
    /// unsettled, so both ways out are still live.
    StillStopped,
    /// This reset CONCLUDED the merge: every recorded copy proved settled, and the record is gone.
    Concluded,
}

/// Whether every copy this bundle records now sits at its own recorded baseline — the reset's
/// "nothing is left unresolved" test, read from disk after the placements landed. An unreadable
/// map or an unscannable copy answers NO: a block that cannot be proven finished stands.
fn every_copy_settled(ctx: &Ctx<'_>, sp: &sidecar::SkillPaths) -> bool {
    let Ok(Some(map)) = doc::read_map(ctx.fs, &sp.map) else {
        return false;
    };
    let Ok(scans) = placement::scan_placements(ctx, &map) else {
        return false;
    };
    !scans.iter().any(|s| {
        matches!(
            s.status.drift(),
            placement::Drift::Modified | placement::Drift::Unscannable
        )
    })
}

/// The current local state of a tracked skill as a read-only `PullSkill` (UpToDate) — used when a
/// targeted pull names a tracked-but-unfollowed skill (there is no `current` to pull).
pub(crate) fn current_state(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
) -> Result<PullSkill, ClientError> {
    let sp = ctx.layout.published(skill_id);
    let sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    Ok(state_row(&lock.name, &sync, PullAction::UpToDate))
}

// ---------------------------------------------------------------------------------------------
// Situation mapping — the engine's only choice; the OUTCOME is the kernel's one policy.
// ---------------------------------------------------------------------------------------------

/// Map the follow-state + invocation to the consent situation. A follower only ever receives an
/// already-approved `current` (the gate is server-side), so a forward move under `review_required` is
/// `ReviewRequiredApproved`, and an ordinary delivered move is `FollowedAutoNewVersion` — the standing
/// pre-authorization the recipe row records. A never-received bundle takes the same rows as every
/// later version: a row that demands it IS the consent, so its first bytes land on the bare sweep.
///
/// An explicit `topos pull <skill>` is a direct local command the user typed, so it maps to
/// `ExplicitLocalPull` — the same apply, attributed to the person rather than to the standing follow.
fn situation_for(follow: &FollowContext, explicit: bool) -> topos_core::consent::Situation {
    use topos_core::consent::Situation;
    if explicit {
        Situation::ExplicitLocalPull
    } else if follow.review_required {
        Situation::ReviewRequiredApproved
    } else {
        Situation::FollowedAutoNewVersion
    }
}

// ---------------------------------------------------------------------------------------------
// Apply / heal.
// ---------------------------------------------------------------------------------------------

/// The verified target of a forward apply / heal — the resolved commit, its digest, and its bytes.
struct ApplyTarget<'a> {
    commit: [u8; 32],
    digest_hex: &'a str,
    bundle: &'a topos_gitstore::RenderedBundle,
}

/// A clean forward apply: materialize the target onto EVERY managed placement (each its own staged
/// atomic swap; already-landed dirs skip the swap) and advance `applied → observed` only after all of
/// them hold the new bytes.
#[allow(clippy::too_many_arguments)]
fn apply_forward(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    map: &PlacementMap,
    managed: &[usize],
    lock: &Lock,
    sync: &SyncState,
    skill_id: &str,
    t: &ApplyTarget<'_>,
) -> Result<(), ClientError> {
    let next_sync = forwarded_sync(sync, t.commit, t.digest_hex);
    let next_lock = lock_from_bundle(lock, t.commit, t.bundle);
    let report = materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id,
            target_indices: managed,
            bundle: t.bundle,
            next_map: next_map(map, t.commit, t.digest_hex),
            next_lock: &next_lock,
            next_sync: &next_sync,
            sp,
            // The pre-overwrite rail: a dir whose bytes differ from ITS recorded sha (an edit no
            // snapshot captured — a crash-window residue on this clean-follower path) is snapshotted
            // into the store before the swap. Never a lost byte.
            snapshot: Some(&|scanned: &ScannedBundle| {
                snapshot_draft(ctx, sp, lock, scanned).map(|_| ())
            }),
            takeover: None,
            self_ignore: ctx.layout.is_project_scope(),
            expected: None,
            project_root: ctx.layout.project_root(),
        },
    )?;
    log_apply(ctx, skill_id, "pull", t.commit, &report);
    Ok(())
}

/// The receipt's second fact for a fast-forward that UNDOES the version it replaces: the copy
/// stood, unmodified, at a version whose content the target does not carry — a revert restored an
/// older state (this machine's own publish, say), and by the merge model a clean tree follows it,
/// the way a clean checkout follows `git pull`. The line names the replaced version, the current
/// one, and the one command that brings the replaced version back; unpublished edits are never
/// touched here (an edited copy takes the merge arm, never this one). The command spells the
/// version IN FULL: `revert` resolves a prefix against the machine store's own history, and a
/// version this scope applied (a project store's own publish) need not be there — the full id
/// resolves everywhere.
///
/// "Undoes" is a CONTENT fact, read from this store's own history: the target's bytes equal a
/// version OLDER than the replaced one (an ancestor on its first-parent line). A target that
/// builds on the replaced version — the ordinary teammate's publish — restores no ancestor and
/// earns no line. `None` too when the history this store holds is too shallow to say.
fn undone_version_note(
    store: &Store,
    lock: &Lock,
    name: &str,
    target: [u8; 32],
    target_digest_hex: &str,
) -> Option<String> {
    let replaced = super::parse_hex32(&lock.base_commit).ok()?;
    if replaced == target {
        return None;
    }
    let restores_ancestor = store
        .log(replaced)
        .ok()?
        .into_iter()
        .skip(1)
        .take(256)
        .any(|node| {
            store_bundle_digest_opt(store, node.version_id)
                .ok()
                .flatten()
                .is_some_and(|d| to_hex(&d) == target_digest_hex)
        });
    // The way back is spelled SHORT, like every version every surface prints: `revert --to`
    // resolves a prefix against the workspace's history, in the scope the command stands in.
    let short = |hex: &str| hex.get(..12).unwrap_or(hex).to_owned();
    restores_ancestor.then(|| {
        let (was, now) = (short(&lock.base_commit), short(&to_hex(&target)));
        format!(
            "replaced your copy (= version {was}) with current {now} — {was} stays in history: \
             topos revert {name} --to {was}"
        )
    })
}

/// Converge the MANAGED placements onto the ALREADY-APPLIED version from the LOCAL store: fill a
/// never-materialized / absent target (a newly detected harness, a fresh `--agent` scope) and refresh
/// a clean-but-stale copy — without any network and without touching `observed`/`applied` (the team's
/// current did not move; this is purely the local fan-out catching up). A draft copy is never
/// touched (only Absent / stale-Clean dirs are targets), and a never-received baseline (no local
/// bytes yet) is a no-op.
///
/// Returns the COMMITTED map — the caller must thread it into any later committing step this run
/// (the settled-draft fan-out): handing a later materialize the pre-converge map would commit it
/// wholesale and erase the baselines just recorded here, leaving an installed dir that the next
/// scan classifies as foreign — plus the placements this converge CREATED (see [`LocalConverge`]).
#[allow(clippy::too_many_arguments)]
fn converge_placements(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    skill_id: &str,
    lock: &Lock,
    sync: &SyncState,
    map: &PlacementMap,
    managed: &[usize],
    work: &WorkTree,
) -> Result<LocalConverge, ClientError> {
    if is_zero_commit(&lock.base_commit) {
        // Never received — nothing local to place yet.
        return Ok(LocalConverge::unchanged(map));
    }
    // The converge's two target classes, told apart because the receipt speaks differently about
    // them: an ABSENT dir filled is a placement CREATED (a folder materialized where none was);
    // a STALE clean replica rewritten is a REFRESH (a copy caught up to the applied version).
    let mut absent: Vec<usize> = Vec::new();
    let mut stale: Vec<usize> = Vec::new();
    let mut missing: Vec<usize> = Vec::new();
    // Already holding the applied version before this run touched anything. Not a target — but
    // the receipt's destination column counts it, because it IS one of the places the bundle is.
    // An EDITED copy is deliberately excluded: it does not hold this version, and the removal
    // receipt's `kept …` line is what speaks for it.
    let mut settled: Vec<usize> = Vec::new();
    for &i in managed {
        let Some(s) = work.scans.get(i) else { continue };
        match &s.status {
            ScanStatus::Absent => {
                absent.push(i);
                missing.push(i);
            }
            // A clean REPLICA at a different version than the lock's base is stale — refreshed
            // ONLY when the work tree itself is at base (never toward or over a live draft:
            // a recorded draft-on-current keeps every copy untouched until it resolves).
            ScanStatus::Clean { digest }
                if matches!(work.state, WorkState::CleanAtBase)
                    && to_hex(digest) != lock.bundle_digest =>
            {
                stale.push(i);
                missing.push(i);
            }
            ScanStatus::Clean { digest } if to_hex(digest) == lock.bundle_digest => {
                settled.push(i);
            }
            // Edited, foreign, or unreadable dirs are never converge targets.
            ScanStatus::Clean { .. }
            | ScanStatus::Modified { .. }
            | ScanStatus::Foreign
            | ScanStatus::Unscannable => {}
        }
    }
    if missing.is_empty() {
        return Ok(LocalConverge::unchanged(map));
    }
    let base = super::parse_hex32(&lock.base_commit)?;
    let base_digest = super::parse_hex32(&lock.bundle_digest)?;
    let store = Store::open(&sp.store)?;
    let bundle = store.render_verified(base, base_digest)?;
    fsync_batch(ctx, &store.version_durability(&base)?)?;
    let report = materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id,
            target_indices: &missing,
            bundle: &bundle,
            next_map: next_map(map, base, &lock.bundle_digest),
            next_lock: lock,
            next_sync: sync, // unchanged — the served target did not move
            sp,
            snapshot: Some(&|scanned: &ScannedBundle| {
                snapshot_draft(ctx, sp, lock, scanned).map(|_| ())
            }),
            takeover: None,
            self_ignore: ctx.layout.is_project_scope(),
            expected: None,
            project_root: ctx.layout.project_root(),
        },
    )?;
    log_apply(ctx, skill_id, "converge", base, &report);
    // The materializer committed the map (per-target skips included) — re-read it so the caller
    // holds exactly what is on disk, never the pre-converge picture.
    let after = read_map_required(ctx, sp)?;
    // What this run CREATED: an Absent target whose dir now stands. Existence, not intent, is
    // the receipt's fact — a target the materializer skipped stays absent and claims nothing.
    let created = absent
        .iter()
        .filter_map(|&i| after.placements.get(i))
        .filter(|p| ctx.fs.exists(Path::new(p)))
        .map(|p| super::inventory::pretty(ctx, Path::new(p)))
        .collect();
    // What this run REFRESHED: a stale target whose recorded baseline NOW names the applied
    // version — the same existence-over-intent rule (a target the materializer re-stat-skipped
    // keeps its old baseline and claims nothing).
    let caught_up: Vec<usize> = stale
        .iter()
        .copied()
        .filter(|&i| {
            after
                .placement_state
                .get(i)
                .is_some_and(|st| st.materialized_sha.as_deref() == Some(&lock.bundle_digest))
        })
        .collect();
    let pretty = |i: &usize| {
        after
            .placements
            .get(*i)
            .map(|p| super::inventory::pretty(ctx, Path::new(p)))
    };
    let refreshed: Vec<String> = caught_up.iter().filter_map(pretty).collect();
    // Where the bundle stands at the applied version now, in placement-map order: the copies that
    // caught up, plus the ones that were already there. `created` is deliberately absent — a dir
    // that appeared this run leads its own `installed` row, which names it.
    let mut at_version: Vec<usize> = caught_up;
    at_version.extend(settled);
    at_version.sort_unstable();
    let at_version = at_version.iter().filter_map(pretty).collect();
    Ok(LocalConverge {
        map: after,
        created,
        refreshed,
        at_version,
    })
}

/// A local placement converge's outcome: the COMMITTED map, plus the disk facts a receipt speaks
/// differently about (display paths, receipt-ready) — the placements it CREATED (dirs materialized
/// where nothing stood) and the stale copies it REFRESHED (dirs that existed but held bytes behind
/// the applied version). Creations flip the row to `installed`, refreshes to `refreshed`: "up to
/// date" may only claim itself when nothing changed on disk.
///
/// `at_version` is a different KIND of fact, and the one a person actually asked for: not what the
/// disk did, but WHERE the bundle now stands at the applied version — the refreshed copies plus
/// every sibling that already held those bytes. A row that names only what it rewrote reports one
/// folder for a bundle that lives in two, which reads as a placement having gone missing.
struct LocalConverge {
    map: PlacementMap,
    created: Vec<String>,
    refreshed: Vec<String>,
    at_version: Vec<String>,
}

impl LocalConverge {
    /// The no-op outcome: the map as it stands, nothing written.
    fn unchanged(map: &PlacementMap) -> Self {
        Self {
            map: map.clone(),
            created: Vec::new(),
            refreshed: Vec::new(),
            at_version: Vec::new(),
        }
    }
}

/// The SECOND fact a receipt row carries when this run wrote folders its action does not name —
/// `also installed <path>` / `also updated <path>` (several join with ", "). It rides the row's
/// `note`, which the renderer prints as an indented line under the row.
pub(crate) fn also_line(verb: &str, dirs: &[String]) -> String {
    format!("also {verb} {}", dirs.join(", "))
}

/// The settled-draft fan-out. Compares the draft's digest against the durable observation
/// (`sync.draft_observed`):
///
/// - UNSETTLED (first sighting, or the content moved since): only the observation is updated —
///   a mid-edit file never spreads. Returns 0.
/// - SETTLED (byte-identical across two runs): the draft's bytes are copied onto the bundle's
///   OTHER placements in this scope — each stale/clean sibling by an ordinary atomic swap staged
///   from the draft content, each landing recorded as that placement's NEW baseline
///   (`materialized_sha` = the draft digest), so a later edit there is a fresh draft against it,
///   never a false competitor. The draft copy itself is untouched; the lock (the pristine version)
///   and `observed`/`applied` do not move. Snapshot-on-touch stays armed, and the swap re-stats
///   each target immediately before exchanging — bytes that moved in the window are SKIPPED
///   (never frozen); the next sweep reconciles. Returns how many placements actually landed.
#[allow(clippy::too_many_arguments)]
fn settle_or_spread(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    skill_id: &str,
    lock: &Lock,
    sync: &SyncState,
    map: &PlacementMap,
    managed: &[usize],
    work: &WorkTree,
    scanned: &ScannedBundle,
) -> Result<Vec<String>, ClientError> {
    let d_hex = to_hex(&scanned.bundle_digest);
    if sync.draft_observed.as_deref() != Some(d_hex.as_str()) {
        let observed = SyncState {
            draft_observed: Some(d_hex),
            ..sync.clone()
        };
        doc::write_doc(ctx.fs, &sp.sync, &observed)?;
        return Ok(Vec::new());
    }
    // Settled. The targets: every OTHER managed placement that is a sync target — a clean copy at
    // any other content, an edited copy the classifier proved STALE BEHIND this draft (or a
    // byte-identical twin, whose baseline merely advances in place), or an absent dir. Foreign and
    // unscannable dirs are never touched.
    let mut targets: Vec<usize> = Vec::new();
    let mut expected: Vec<(usize, Option<String>)> = Vec::new();
    for &i in managed {
        if work.draft_idx == Some(i) {
            continue;
        }
        let Some(s) = work.scans.get(i) else { continue };
        match &s.status {
            ScanStatus::Clean { digest } => {
                let hex = to_hex(digest);
                if hex != d_hex {
                    targets.push(i);
                    expected.push((i, Some(hex)));
                }
            }
            ScanStatus::Modified { scanned: other } => {
                targets.push(i);
                expected.push((i, Some(to_hex(&other.bundle_digest))));
            }
            ScanStatus::Absent => {
                targets.push(i);
                expected.push((i, None));
            }
            ScanStatus::Foreign | ScanStatus::Unscannable => {}
        }
    }
    if targets.is_empty() {
        return Ok(Vec::new());
    }
    // The draft's bytes get a store identity FIRST (idempotent), so the recorded baselines below
    // always name recoverable content.
    snapshot_draft(ctx, sp, lock, scanned)?;
    let bundle = rendered_from_scanned(scanned);
    let next_sync = SyncState {
        draft_observed: Some(d_hex.clone()),
        ..sync.clone()
    };
    // The lock and the map-level applied version DO NOT move — the pristine version is unchanged;
    // only the landed placements' per-placement baselines advance (the materializer records them).
    materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id,
            target_indices: &targets,
            bundle: &bundle,
            next_map: map.clone(),
            next_lock: lock,
            next_sync: &next_sync,
            sp,
            snapshot: Some(&|s: &ScannedBundle| snapshot_draft(ctx, sp, lock, s).map(|_| ())),
            takeover: None,
            self_ignore: ctx.layout.is_project_scope(),
            expected: Some(&expected),
            project_root: ctx.layout.project_root(),
        },
    )?;
    // Collect what actually landed (the skip arm may have left some targets put): a target whose
    // recorded baseline NOW names the draft and did not before — as display paths, the receipt's
    // destination column.
    let after = read_map_required(ctx, sp)?;
    let landed = targets
        .iter()
        .filter(|&&i| {
            after
                .placement_state
                .get(i)
                .is_some_and(|st| st.materialized_sha.as_deref() == Some(d_hex.as_str()))
                && map
                    .placement_state
                    .get(i)
                    .is_none_or(|st| st.materialized_sha.as_deref() != Some(d_hex.as_str()))
        })
        .filter_map(|&i| after.placements.get(i))
        .map(|p| super::inventory::pretty(ctx, Path::new(p)))
        .collect();
    Ok(landed)
}

/// A scanned working tree as a [`topos_gitstore::RenderedBundle`] (the spread stages the draft's
/// exact bytes).
fn rendered_from_scanned(b: &ScannedBundle) -> topos_gitstore::RenderedBundle {
    topos_gitstore::RenderedBundle {
        files: b
            .files
            .iter()
            .map(|f| topos_gitstore::RenderedFile {
                path: f.path.clone(),
                mode: f.mode,
                bytes: f.bytes.clone(),
                content_sha256: digest::sha256(&f.bytes),
            })
            .collect(),
        bundle_digest: b.bundle_digest,
    }
}

/// Snapshot EVERY distinct edited copy into the sidecar store (the explicit-overwrite verbs' rail:
/// go-back / reset converge divergent copies rather than freezing, so each distinct edit is retained
/// first). Fails closed on an unscannable placement — `what` names the refusing verb.
fn snapshot_all_modified(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    map: &PlacementMap,
    what: &str,
) -> Result<(), ClientError> {
    let scans = placement::scan_placements(ctx, map)?;
    if scans
        .iter()
        .any(|s| matches!(s.status, ScanStatus::Unscannable))
    {
        return Err(ClientError::PlacementUnsupported {
            reason: format!("a placement cannot be read; refusing {what} that might clobber it"),
        });
    }
    for (idx, _) in placement::distinct_modified(&scans) {
        if let ScanStatus::Modified { scanned } = &scans[idx].status {
            snapshot_draft(ctx, sp, lock, scanned)?;
        }
    }
    Ok(())
}

/// Whether every MANAGED placement already holds the target bytes (the no-swap heal precondition).
/// An empty managed set is vacuously false — there is nothing to advance `applied` over.
fn all_managed_at_target(
    scans: &[placement::PlacementScan],
    managed: &[usize],
    target_digest_hex: &str,
) -> bool {
    !managed.is_empty()
        && managed.iter().all(|&i| {
            scans.get(i).is_some_and(|s| match &s.status {
                ScanStatus::Clean { digest } => to_hex(digest) == target_digest_hex,
                ScanStatus::Modified { scanned } => {
                    to_hex(&scanned.bundle_digest) == target_digest_hex
                }
                ScanStatus::Absent | ScanStatus::Foreign | ScanStatus::Unscannable => false,
            })
        })
}

/// A best-effort action-log note (the spec's "quiet note") — the apply already succeeded, so a log hiccup
/// never undoes it. Reads the materialize report (the effective swap capability + whether prior bytes were
/// preserved) so the local `log` shows what landed.
fn log_apply(
    ctx: &Ctx<'_>,
    skill_id: &str,
    action: &str,
    version_id: [u8; 32],
    report: &MaterializeReport,
) {
    let _ = logfile::append_event(
        ctx.fs,
        &ctx.layout.log_path(),
        &serde_json::json!({
            "action": action,
            "skill_id": skill_id,
            "version_id": to_hex(&version_id),
            "swap": format!("{:?}", report.swap_capability),
            "preserved_prior": report.pre_existing_sha.is_some(),
            "at": ctx.clock.now_unix_millis(),
        }),
    );
}

/// The heal: every managed placement already holds the target bytes (a completed-but-unrecorded
/// apply). Advance the docs (map → lock → sync) with NO swap — each managed placement's state records
/// the target sha (pre-existing captured stickily; the dirs are present, they hold the bytes).
fn heal_forward(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    map: &PlacementMap,
    managed: &[usize],
    lock: &Lock,
    sync: &SyncState,
    t: &ApplyTarget<'_>,
) -> Result<(), ClientError> {
    let next_sync = forwarded_sync(sync, t.commit, t.digest_hex);
    let next_lock = lock_from_bundle(lock, t.commit, t.bundle);
    let mut next = next_map(map, t.commit, t.digest_hex);
    for &i in managed {
        let prior = next.placement_state[i].clone();
        next.placement_state[i] = topos_types::persisted::PlacementState {
            materialized_sha: Some(t.digest_hex.to_owned()),
            pre_existing_sha: materialize::derive_pre_existing_state(&prior, true),
            ..prior
        };
    }
    materialize::mirror_first_placement(&mut next);
    materialize::commit_docs(ctx.fs, sp, &next, &next_lock, &next_sync)
}

/// The forward target sync state: `applied = observed`, base/work move to the target, `held` cleared.
/// `observed` + `observed_version_id` are the served target (unchanged by an apply).
pub(crate) fn forwarded_sync(
    sync: &SyncState,
    target: [u8; 32],
    target_digest_hex: &str,
) -> SyncState {
    SyncState {
        schema_version: sync.schema_version,
        observed: sync.observed,
        observed_version_id: sync.observed_version_id.clone(),
        applied: sync.observed,
        base_commit: to_hex(&target),
        work_hash: target_digest_hex.to_owned(),
        held: false,
        // A forward apply/heal lands the pristine target everywhere — no standing draft remains.
        draft_observed: None,
    }
}

/// The engine-computed next map for an apply toward `target`: the (already reconciled) placements +
/// their PRIOR per-placement states — the materializer updates each landed state — with the map-level
/// summary advanced.
pub(crate) fn next_map(
    map: &PlacementMap,
    target: [u8; 32],
    target_digest_hex: &str,
) -> PlacementMap {
    PlacementMap {
        applied_commit: to_hex(&target),
        materialized_sha: target_digest_hex.to_owned(),
        ..map.clone()
    }
}

// ---------------------------------------------------------------------------------------------
// The store side: snapshot a draft, backfill + record a fetched version, read a stored digest.
// ---------------------------------------------------------------------------------------------

/// Snapshot the working bytes (already scanned by `compute_work`, so the saved draft is byte-consistent
/// with the decision that surfaced it — scanned exactly once) into the sidecar store as a commit on
/// `base_commit`, so a draft is never lost. Returns the snapshot `version_id` (the saved draft).
pub(crate) fn snapshot_draft(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
    lock: &Lock,
    scanned: &ScannedBundle,
) -> Result<String, ClientError> {
    // A snapshot commits the working bytes, so the bundle must carry them — never the digest-only
    // shape a cache-clean placement yields (the byte-consuming callers full-scan before snapshotting).
    debug_assert!(
        !scanned.files.is_empty(),
        "snapshot_draft needs a full byte bundle, not a digest-only clean scan"
    );
    let base = super::parse_hex32(&lock.base_commit)?;
    let draft_id = identity::commit_id(&Commit {
        parents: &[base],
        tree: scanned.bundle_digest,
        author: &ctx.device_id,
        message: DRAFT_SNAPSHOT_MESSAGE,
    })
    .map_err(|_| ClientError::Corrupt("draft snapshot commit id".into()))?;

    let store = Store::open(&sp.store)?;
    // Idempotent: the snapshot id is deterministic (base + tree + author + a fixed message), so a
    // re-snapshot of the same bytes (the multi-placement paths may see one draft twice — once at the
    // pre-verb snapshot, once at the materializer's pre-overwrite rail) is a clean no-op.
    if store.list_versions()?.contains(&draft_id) {
        return Ok(to_hex(&draft_id));
    }
    let import: Vec<ImportFile<'_>> = scanned
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    store.commit(
        draft_id,
        &[base],
        &tree,
        &ctx.device_id,
        DRAFT_SNAPSHOT_MESSAGE,
    )?;
    // The snapshot's own objects + ref — durable before the draft id is surfaced or recorded anywhere.
    fsync_batch(ctx, &store.version_durability(&draft_id)?)?;
    Ok(to_hex(&draft_id))
}

/// Ensure `version_id` (and any missing ancestors) is committed in the local store, so a later go-back,
/// diff, or log can render it. Recursively backfills absent parents (the fixture serves each) so
/// `Store::commit`'s parent-present precondition holds across a multi-generation gap. Returns the version's
/// `bundle_digest` (recomputed over the fetched bytes — the integrity tree hash).
///
/// Every version this call WRITES adds its own durability set to `written` (accumulated across the
/// backfill; the caller fsyncs once at the end, before any JSON records the target) — so the fsync cost
/// is bounded by this op's writes, never the store's lifetime history. An already-present version adds
/// its set too: present-and-renderable does not imply durable (a prior pull may have crashed between its
/// write and its fsync), and the caller is about to record it. That present arm RETURNS before the
/// parent walk below, so a present parent contributes exactly its own set (no-op fsyncs when already
/// durable) without recursing into its own ancestors — the recursion frontier stops at the first
/// present generation.
/// PREFETCH one version's bytes into the scope's store and verify them — the frozen
/// preflight's unit: everything a later apply will need is made local and digest-checked
/// BEFORE any checkout byte moves, so a failing input refuses the run with zero placements.
/// Store writes are cache population, not checkout mutation (`npm ci` fills its cache the
/// same way).
pub(crate) fn prefetch_version(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
    version_hex: &str,
) -> Result<(), ClientError> {
    // A bundle this scope ALREADY holds prefetches straight into its own store: the objects land
    // where the arm below reads them, and nothing else about the run changes.
    if ctx.fs.exists(&ctx.layout.skill_dir(skill_id)) {
        return prefetch_into(
            ctx,
            &ctx.layout.published(skill_id).store,
            skill_id,
            version_hex,
        );
    }
    // A bundle it has NEVER held has no store yet — the ordinary state of a genuine clone, whose
    // `.topos/` is gitignored whole and so never travels. The proof still has to run, but it must
    // not mint the PUBLISHED directory: the arm's never-received baseline keys off that
    // directory's absence, and a store-only directory there would make it skip the sidecar
    // documents it lays, leaving the rest of the run reading state nobody wrote. So the proof
    // runs in the scope's staging namespace and is torn down again; what survives it is the
    // machine-wide blob cache the arm's own fetch then reads — `npm ci` fills a cache and
    // installs out of it the same way.
    let (scratch, sp) = ctx.layout.staging(skill_id);
    if ctx.fs.exists(&scratch) {
        ctx.fs.remove_dir_all(&scratch)?;
    }
    let proved = prefetch_into(ctx, &sp.store, skill_id, version_hex);
    // Torn down whichever way the proof went: a refused `--frozen` run promises the checkout AND
    // the store behind it are exactly as it found them.
    let _ = ctx.fs.remove_dir_all(&scratch);
    proved
}

/// Fetch + digest-verify one version into the bare store at `store_dir`, birthing the store when it
/// is not there yet.
///
/// # Errors
/// The store/plane failure that stopped the fetch, or the integrity failure that stopped the verify.
fn prefetch_into(
    ctx: &Ctx<'_>,
    store_dir: &std::path::Path,
    skill_id: &crate::id::SkillId,
    version_hex: &str,
) -> Result<(), ClientError> {
    // Open the standing store, or birth one. `Store::init` is `init_bare`: it creates the store
    // directory itself and NOT the directories above it, so a scope whose store tree does not
    // exist at all — every genuine clone — needs those laid first. Without that the preflight met
    // its own first customer with "the local skill store reported an error".
    let store = match Store::open(store_dir) {
        Ok(store) => store,
        Err(_) => {
            ctx.fs.create_dir_all(store_dir)?;
            Store::init(store_dir)?
        }
    };
    let commit = super::parse_hex32(version_hex)?;
    let mut written = WriteBatch::default();
    let digest = ensure_local(ctx, &store, skill_id.as_str(), commit, 0, &mut written)?
        .unwrap_or_else(|| unreachable!("depth-0 ensure_local errors instead of shallow-stopping"));
    fsync_batch(ctx, &written)?;
    // The verify half: the rendered bytes must reproduce their own digest.
    let _ = store.render_verified(commit, digest)?;
    Ok(())
}

fn ensure_local(
    ctx: &Ctx<'_>,
    store: &Store,
    skill_id: &str,
    version_id: [u8; 32],
    depth: usize,
    written: &mut WriteBatch,
) -> Result<Option<[u8; 32]>, ClientError> {
    if depth > MAX_BACKFILL {
        return Err(ClientError::Corrupt(
            "version lineage too deep to backfill".into(),
        ));
    }
    if let Some(existing) = store_bundle_digest_opt(store, version_id)? {
        written.extend(store.version_durability(&version_id)?);
        return Ok(Some(existing));
    }
    let Some(fetched) = fetch_served(ctx, skill_id, version_id)? else {
        if depth == 0 {
            // The TARGET must be served — a miss here is the ordinary not-served error, never a
            // silent shallow stop.
            return Err(ClientError::Plane(format!(
                "the server does not serve version {}, or not to you",
                to_hex(&version_id)
            )));
        }
        return Ok(None);
    };
    // Walk EVERY parent — unconditionally. An absent parent is backfilled (so the commit sees its
    // parents); a PRESENT parent still contributes its durability set via the early-return arm above,
    // because present ≠ durable (a prior pull may have crashed after the parent's write but before its
    // fsync, and this pull is about to record a child that names it). The present arm returns before
    // its own parent walk, so a present parent never recurses further.
    //
    // SHALLOW STOP: an ANCESTOR the plane no longer serves (its version was purged — the tombstone
    // story — or upstream pruned history) must not wedge the install of the LIVE target: the walk
    // stops at that branch (the recursive call answers `None`) and `commit_backfill` below omits
    // the absent parent from the local git linkage — identity is unaffected, the version id is
    // over the frame's parent ids, which the wire supplied. Local `log`/`diff`/merge simply end at
    // the gap, the honest shape of purged history. Only a NOT-SERVED miss shallow-stops; a
    // transport/availability fault still fails the pull (retry later), and the TARGET itself
    // (depth 0) is never skipped — its miss stays the hard error below.
    for parent in &fetched.parents {
        ensure_local(ctx, store, skill_id, *parent, depth + 1, written)?;
    }
    let import: Vec<ImportFile<'_>> = fetched
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    // `commit_backfill` re-derives the version_id from (parents, tree.bundle_digest, author, message) and
    // refuses a ref that lies about its identity — so tampered bytes / metadata fail here (recompute ==
    // version_id); a parent the shallow stop above skipped is omitted from the local git linkage only.
    store
        .commit_backfill(
            version_id,
            &fetched.parents,
            &tree,
            &fetched.author,
            &fetched.message,
        )
        .map_err(|_| {
            ClientError::Corrupt(format!(
                "fetched version {} does not match its id",
                to_hex(&version_id)
            ))
        })?;
    // No fsync here — name what this commit created and let the caller fsync ONCE after the whole
    // backfill, so durability cost is proportional to the bytes written, not paid per ancestor commit.
    written.extend(store.version_durability(&version_id)?);
    Ok(Some(tree.bundle_digest))
}

/// The `bundle_digest` of a stored version, or `None` if it is not present **or not readable**. A present
/// ref whose objects cannot be rendered (a dangling ref left by a crash between the ref write and the
/// object fsync) is treated as absent, so `ensure_local` re-fetches + re-commits and heals it rather than
/// wedging forever, and a go-back to such a version is refused as unknown. `pub(crate)` — the publish
/// describe's merge preview reads the locally-held observed current through the same is-it-really-here
/// gate.
pub(crate) fn store_bundle_digest_opt(
    store: &Store,
    version_id: [u8; 32],
) -> Result<Option<[u8; 32]>, ClientError> {
    if !store.list_versions()?.contains(&version_id) {
        return Ok(None);
    }
    Ok(store_bundle_digest(store, version_id).ok())
}

/// The `bundle_digest` of a present stored version (recomputed via the tree-structure walk → kernel digest
/// over the recorded content ids). Used to pin `render_verified`.
pub(super) fn store_bundle_digest(
    store: &Store,
    version_id: [u8; 32],
) -> Result<[u8; 32], ClientError> {
    let leaves = store.read_tree_structure(version_id)?;
    let mut entries = Vec::with_capacity(leaves.len());
    for leaf in &leaves {
        let (_, content_sha256) = store.read_git_blob_verified(leaf.git_oid)?;
        entries.push(digest::ManifestEntry {
            path: leaf.path.clone(),
            mode: leaf.mode,
            content_sha256,
        });
    }
    digest::bundle_digest(&entries)
        .map_err(|r| ClientError::Corrupt(format!("stored digest: {r:?}")))
}

// ---------------------------------------------------------------------------------------------
// Working-tree classification.
// ---------------------------------------------------------------------------------------------

/// The AGGREGATE work-tree state across every recorded placement — draft-anywhere classification.
pub(crate) enum WorkState {
    /// No placement holds bytes — a clean first install (nothing to clobber).
    Absent,
    /// The work tree matches the locked base bytes (replicas may be STALE — a clean converge
    /// refreshes them from the local store, losing nothing).
    CleanAtBase,
    /// The work tree differs from the locked base — the kernel's state-③/④ fodder. Carries the scan
    /// so the draft is snapshotted from the exact bytes the decision was made on (the single-work-tree
    /// surfaces — diff / publish / merge — locate the copy through `placement::work_tree_dir`).
    Draft { scanned: ScannedBundle },
    /// A placement exists but cannot be scanned safely — fail closed, never overwrite it.
    Unscannable,
}

/// The classified work tree + the per-placement scans it was derived from (the apply loop and the
/// at-target refinement read the rows; the kernel reads the aggregate).
pub(crate) struct WorkTree {
    pub scans: Vec<placement::PlacementScan>,
    pub state: WorkState,
    /// The placement index the [`WorkState::Draft`] bytes were read from (the advanced copy), so
    /// the settled-draft fan-out knows which copy is THE draft. `None` for every other state.
    pub draft_idx: Option<usize>,
}

/// Classify the placements into ONE work tree — draft-anywhere, with the two detectors SPLIT:
///
/// - The DRAFT detector reads each copy against ITS OWN recorded per-placement sha; one distinct
///   edited content per bundle+scope is THE work tree (the draft may live in the shared dir or any
///   native copy).
/// - The CONFLICT detector reads the edited copies against EACH OTHER: two copies are competitors
///   ONLY when neither's bytes equal the other's RECORDED BASELINE. A copy sitting at a sibling's
///   baseline is merely STALE BEHIND that sibling's draft — the draft resolves to the advanced
///   copy, and only TRUE competitors raise the typed [`ClientError::PlacementsDiverged`] freeze
///   (nothing overwritten, each competing path disclosed, `update --reset` the named way out).
/// - With NO edited copy, the work tree is the FIRST placement's copy (the canonical one — the exact
///   single-placement behavior), falling back to the first present copy. Its digest vs the LOCK
///   decides clean-vs-draft for the kernel: a copy that matches its recorded sha but not the lock is
///   a RECORDED draft (draft-on-current, the merge's landing shape), never silently re-based.
///
/// Distinguishes ABSENT (a safe install) from UNSCANNABLE (fail closed, never clobber).
pub(crate) fn compute_work(
    ctx: &Ctx<'_>,
    map: &PlacementMap,
    lock: &Lock,
    skill_name: &str,
) -> Result<WorkTree, ClientError> {
    let scans = placement::scan_placements(ctx, map)?;
    if scans
        .iter()
        .any(|s| matches!(s.status, ScanStatus::Unscannable))
    {
        return Ok(WorkTree {
            scans,
            state: WorkState::Unscannable,
            draft_idx: None,
        });
    }
    // The work tree: the resolved draft when one exists, else the first (canonical) present copy.
    let chosen: Option<&placement::PlacementScan> = match placement::classify_draft(&scans, map) {
        placement::DraftVerdict::Competitors(indices) => {
            return Err(placement::placements_diverged(
                ctx, skill_name, &scans, &indices,
            ));
        }
        placement::DraftVerdict::One { idx, .. } => Some(&scans[idx]),
        placement::DraftVerdict::NoDraft => scans.iter().find(|s| {
            matches!(
                s.status,
                ScanStatus::Clean { .. } | ScanStatus::Modified { .. }
            )
        }),
    };
    let mut draft_idx = None;
    let state = match chosen {
        None => WorkState::Absent,
        Some(s) => {
            let digest_hex = match &s.status {
                ScanStatus::Clean { digest } => to_hex(digest),
                ScanStatus::Modified { scanned } => to_hex(&scanned.bundle_digest),
                _ => unreachable!("chosen is always a scanned copy"),
            };
            if digest_hex == lock.bundle_digest {
                WorkState::CleanAtBase
            } else {
                // A draft: an edited copy, or a stale clean replica recorded as a draft-on-current.
                // It needs the exact working bytes — a Modified status already carries them; a Clean
                // status is digest-only (the stat cache spared the read), so re-scan its dir now.
                let scanned = match &s.status {
                    ScanStatus::Modified { scanned } => scanned.clone(),
                    ScanStatus::Clean { .. } => crate::scan::scan(&s.dir)?,
                    _ => unreachable!("chosen is always a scanned copy"),
                };
                draft_idx = Some(s.idx);
                WorkState::Draft { scanned }
            }
        }
    };
    Ok(WorkTree {
        scans,
        state,
        draft_idx,
    })
}

// ---------------------------------------------------------------------------------------------
// Scope-checking a served record.
// ---------------------------------------------------------------------------------------------

/// Confirm a served `current` record's `(workspace_id, skill_id)` scope is the one we follow and return its
/// `version_id`. A mis-scoped record (a cross-workspace / cross-skill record served in error) is a
/// malformed response, NONE — never the sync target. Shared by the engine and the `follow` offer
/// disclosure. There is no signature: authority is the database row behind the pointer, integrity is the
/// content-addressed `version_id` re-verified by digest on apply.
pub(crate) fn scoped_version_id(
    rec: &topos_types::WireCurrentRecord,
    skill_id: &str,
    workspace_id: &str,
) -> Option<[u8; 32]> {
    if rec.scope.skill_id != skill_id || rec.scope.workspace_id != workspace_id {
        return None;
    }
    super::parse_hex32(&rec.record.version_id).ok()
}

// ---------------------------------------------------------------------------------------------
// Small helpers.
// ---------------------------------------------------------------------------------------------

/// [`fetch`] distinguishing the NOT-SERVED miss (`Ok(None)` — the backfill's shallow-stop signal:
/// a purged/pruned ancestor) from real faults (transport, availability, malformed — all still
/// errors: the state is retryable, never silently shallow).
fn fetch_served(
    ctx: &Ctx<'_>,
    skill_id: &str,
    version_id: [u8; 32],
) -> Result<Option<crate::plane::FetchedVersion>, ClientError> {
    match ctx.plane.fetch_version(skill_id, version_id) {
        Ok(v) => Ok(Some(v)),
        Err(PlaneError::NotFound) => Ok(None),
        // The version floor keeps its own identity here too — a backfill that stops because the
        // server refuses this build has one fix, and it is not "retry".
        Err(PlaneError::UpdateRequired { min }) => Err(ClientError::UpdateRequired { min }),
        Err(PlaneError::Unavailable(m) | PlaneError::Unreachable(m) | PlaneError::Malformed(m)) => {
            Err(ClientError::Plane(m))
        }
    }
}

/// Whether `g` is the genesis sentinel `(0,0)`.
fn is_zero_gen(g: u64) -> bool {
    g == 0
}

/// Whether this followed skill has NEVER received bytes — the first-receive baseline the reconcile lays:
/// nothing applied, on the all-zero base. An `add`-ed skill carries a real local genesis (a non-zero
/// `base_commit`), and a received skill has applied a version (`applied` > `(0,0)`), so neither is ever
/// mistaken for it.
///
/// DURABLE across sweeps: keyed on `applied` + the zero base, NOT `observed`. A sweep that reaches the
/// plane moves `observed` to the served target (so the conditional GET keeps working) whether or not its
/// apply then succeeds — so keying on `observed` would make a run that failed mid-fetch report its
/// eventual first materialization as an ordinary fast-forward. `applied` stays `(0,0)` and `base_commit`
/// stays all-zero until bytes actually LAND, so they remain a true "never placed" signal every sweep.
pub(crate) fn is_never_received(sync: &SyncState) -> bool {
    is_zero_gen(sync.applied) && is_zero_commit(&sync.base_commit)
}

/// Whether a commit-id hex is the all-zero sentinel the first-receive baseline lays for `base_commit` (no
/// local bytes yet) — a real content-addressed commit id is never all-zero.
fn is_zero_commit(commit_hex: &str) -> bool {
    commit_hex.len() == 64 && commit_hex.bytes().all(|b| b == b'0')
}

/// What the client holds for the conditional GET: the observed generation + the commit it names, or `None`
/// for the never-received baseline (no observed commit yet — the all-zero sentinel) → an unconditional first
/// GET. A skill that has ever seen a `current` carries a real `observed_version_id`, so it resolves to `Some`.
fn known_current(sync: &SyncState) -> Result<Option<KnownCurrent>, ClientError> {
    if is_zero_commit(&sync.observed_version_id) {
        return Ok(None);
    }
    Ok(Some(KnownCurrent {
        generation: sync.observed,
        version_id: super::parse_hex32(&sync.observed_version_id)?,
    }))
}

pub(crate) fn lock_from_bundle(
    prior: &Lock,
    version_id: [u8; 32],
    bundle: &topos_gitstore::RenderedBundle,
) -> Lock {
    Lock {
        schema_version: prior.schema_version,
        skill_id: prior.skill_id.clone(),
        name: prior.name.clone(),
        base_commit: to_hex(&version_id),
        bundle_digest: to_hex(&bundle.bundle_digest),
        files: bundle
            .files
            .iter()
            .map(|f| LockedFile {
                path: f.path.clone(),
                mode: f.mode.as_str().to_owned(),
                sha256: to_hex(&f.content_sha256),
                size: f.bytes.len() as u64,
            })
            .collect(),
    }
}

/// Read the required `map.json` through the per-doc versioned reader (`v1` upgrades in memory).
pub(crate) fn read_map_required(
    ctx: &Ctx<'_>,
    sp: &sidecar::SkillPaths,
) -> Result<PlacementMap, ClientError> {
    doc::read_map(ctx.fs, &sp.map)?
        .ok_or_else(|| ClientError::Corrupt("missing map.json for a followed skill".into()))
}

/// fsync a named durability batch through the fault-injectable fs seam — files first, then the dirs
/// whose entries changed. Paths are deduped first (insertion order kept), so a multi-version
/// accumulation (an ancestor backfill naming a shared object twice) never pays twice for one path —
/// macOS `F_FULLFSYNC` is roughly milliseconds per call.
pub(crate) fn fsync_batch(ctx: &Ctx<'_>, batch: &WriteBatch) -> Result<(), ClientError> {
    let mut seen = std::collections::HashSet::new();
    for f in batch.files.iter().filter(|p| seen.insert(*p)) {
        ctx.fs.fsync_file(f)?;
    }
    seen.clear();
    for d in batch.dirs.iter().filter(|p| seen.insert(*p)) {
        ctx.fs.fsync_dir(d)?;
    }
    Ok(())
}

fn read_required<T: serde::de::DeserializeOwned>(
    ctx: &Ctx<'_>,
    path: &Path,
    what: &str,
) -> Result<T, ClientError> {
    doc::read_doc(ctx.fs, path)?
        .ok_or_else(|| ClientError::Corrupt(format!("missing {what} for a followed skill")))
}

// ---- PullSkill row builders ----

/// Flip an applied row to `installed` when this apply landed the bundle's FIRST bytes in this
/// scope (the never-received baseline), and name the DESTINATIONS the receipt speaks in: the
/// managed placement dirs for a file bundle, or — for a config-placed bundle, whose managed set
/// is empty — the config files an inline converge just filled (`row.harnesses`).
fn mark_installed(
    ctx: &Ctx<'_>,
    row: &mut PullSkill,
    first_receive: bool,
    map: &PlacementMap,
    managed: &[usize],
) {
    if !first_receive {
        return;
    }
    row.action = PullAction::Installed;
    row.destinations = destination_paths(ctx, map, managed);
    if row.destinations.is_empty() {
        row.destinations = config_destinations(ctx, &row.harnesses);
    }
}

/// The display destinations `indices` name in `map` — `~`-abbreviated, receipt-ready.
fn destination_paths(ctx: &Ctx<'_>, map: &PlacementMap, indices: &[usize]) -> Vec<String> {
    indices
        .iter()
        .filter_map(|&i| map.placements.get(i))
        .map(|p| super::inventory::pretty(ctx, Path::new(p)))
        .collect()
}

/// The config FILES this converge WROTE (the `placed` states), deduped — what a config-placed
/// bundle's `installed (…)` / `updated (…)` column counts and spells out. `~`-abbreviated.
///
/// Written, never merely holding: a destination column answers "what did this run do to my
/// machine", so a file that already held the entry byte-for-byte is not counted among the ones
/// the run landed. The per-agent lines beside the row still name it — as `unchanged`.
pub(crate) fn config_destinations(
    ctx: &Ctx<'_>,
    states: &[topos_types::results::McpAgentState],
) -> Vec<String> {
    let mut out: Vec<String> = states
        .iter()
        .filter(|s| s.state.wrote())
        .filter_map(|s| s.file.as_deref())
        .map(|f| super::inventory::pretty(ctx, Path::new(f)))
        .collect();
    out.dedup();
    out
}

fn state_row(name: &str, sync: &SyncState, action: PullAction) -> PullSkill {
    // `workspace_id` is stamped by the pull aggregator (`pull.rs`), which holds the follow-state; every row
    // builder here leaves it `None`.
    PullSkill {
        skill: name.to_owned(),
        workspace_id: None,
        observed: sync.observed,
        applied: sync.applied,
        action,
        merge: None,
        synced_placements: None,
        destinations: Vec::new(),
        kept: Vec::new(),
        display: None,
        note: None,
        scope: None,
        harnesses: Vec::new(),
        kind: None,
        draft: false,
        narrowed: None,
    }
}

fn applied_row(name: &str, sync: &SyncState, _target: [u8; 32]) -> PullSkill {
    // `applied` is now `observed` on disk; report the advanced state.
    PullSkill {
        skill: name.to_owned(),
        workspace_id: None,
        observed: sync.observed,
        applied: sync.observed,
        action: PullAction::FastForwarded,
        merge: None,
        synced_placements: None,
        destinations: Vec::new(),
        kept: Vec::new(),
        display: None,
        note: None,
        scope: None,
        harnesses: Vec::new(),
        kind: None,
        draft: false,
        narrowed: None,
    }
}

/// The settled-draft fan-out's receipt row: `n` other agent folders now carry the draft.
fn synced_row(name: &str, sync: &SyncState, n: u32) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        workspace_id: None,
        observed: sync.observed,
        applied: sync.applied,
        action: PullAction::DraftSynced,
        merge: None,
        synced_placements: Some(n),
        destinations: Vec::new(),
        kept: Vec::new(),
        display: None,
        note: None,
        scope: None,
        harnesses: Vec::new(),
        kind: None,
        draft: false,
        narrowed: None,
    }
}
