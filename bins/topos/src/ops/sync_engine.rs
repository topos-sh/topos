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
//! 3. **apply** — act on `consent::decide()`: materialize + advance `applied` (auto / explicit accept),
//!    offer (confirm-each), or snapshot + surface the DIVERGED panel (never clobber).
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
use topos_types::results::{Conflict, Offer, PullAction, PullSkill};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::materialize::{self, MaterializeReport, MaterializeReq};
use crate::placement::{self, ScanStatus};
use crate::plane::{FollowContext, FollowMode, KnownCurrent, PlaneError, PointerFetch};
use crate::scan::ScannedBundle;
use crate::{doc, logfile, sidecar};

/// The fixed commit message for a draft snapshot (folded into its `version_id`; must stay constant).
/// `pub(crate)` so `keep-as-yours` (`ops::add`) can recognise a retained draft snapshot in the store.
pub(crate) const DRAFT_SNAPSHOT_MESSAGE: &str = "topos: draft snapshot";
/// A bound on ancestor backfill — far beyond any real lineage gap; stops a forged cyclic store.
const MAX_BACKFILL: usize = 256;
/// The `applied` generation a go-back leaves behind: the genesis sentinel `(0,0)`, which is strictly below
/// any real served `observed`, so a later `pull` sees `applied != observed` (behind) and — once the `held`
/// pin is released by an explicit pull — fast-forwards back to the team's current. (The go-back installs an
/// OLD version whose true generation is no longer tracked locally; `(0,0)` is the honest "not at current".)
const GO_BACK_APPLIED: u64 = 0;

/// A capability token proving the author-merge code was reached from a divergence. Its field is private to
/// this module, so NO other module can mint one; [`super::merge_resolve::resolve_diverged`] takes it by
/// value, so the merge is unreachable from a current/behind/clean-follower state **by construction** — a
/// structural gate, not a role check. It is minted at exactly two guarded sites in [`sync_one`]: the
/// post-fetch `Diverged` arm (entered only when `work != base`), and the entry escape of an already-recorded
/// conflict (a `conflict.json` exists only for an author who diverged). A clean follower hits neither.
pub(crate) struct DivergedWitness(());

/// What a per-skill `sync_one` invocation is — the bare sweep, an explicit accept, or the disclosed escape.
/// Replaces the old `explicit: bool`: the escape is also "explicit", but it resolves a divergence by
/// committing the author's bytes on `current` rather than accepting a pending update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Invocation {
    /// The bare session-start sweep (`topos pull`).
    Sweep,
    /// A targeted accept / resume (`topos pull <skill>`).
    Accept,
    /// The disclosed escape (`topos pull <skill> --onto-current`): commit MY bytes on top of `current`.
    Escape,
}

impl Invocation {
    /// Whether the user's command itself supplies consent (a targeted accept or escape) vs the bare sweep.
    fn is_explicit(self) -> bool {
        matches!(self, Invocation::Accept | Invocation::Escape)
    }
}

/// Bring one followed skill current (the sweep, the explicit-accept path, and the diverged-draft resolve).
///
/// `inv` is [`Invocation::Sweep`] for the bare session-start sweep, [`Invocation::Accept`] for a targeted
/// `topos pull <skill>` (the command supplies consent, so a confirm-each skill applies rather than offers,
/// and a `held` pin is released), or [`Invocation::Escape`] for `--onto-current` (resolve a divergence by
/// committing the author's bytes on `current`).
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
    let explicit = inv.is_explicit();
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    let sp = ctx.layout.published(skill_id);
    let skill_id = skill_id.as_str();
    let mut sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    let map: PlacementMap = read_map_required(ctx, &sp)?;
    let name = lock.name.clone();

    // A never-received followed skill (the first-receive baseline `follow` lays: nothing observed yet, no
    // placement). I-TOFU: its first version is an OFFER behind one explicit accept/`--approve`, never
    // auto-landed — captured BEFORE checkForUpdates mutates `observed`.
    let first_receive = is_never_received(&sync);

    // The conditional-GET validator: what the client currently holds (its observed generation AND the commit
    // it names) — so a record reusing `(epoch,seq)` for a different commit is returned, not 304'd. `None`
    // for the never-received baseline (no observed commit yet) → an unconditional first GET.
    let known = known_current(&sync)?;

    // An unresolved conflict is on record. The escape (`--onto-current`) RESOLVES it (plane-independent, so
    // it runs even when the plane is unreachable — the no-deadlock guarantee). Any OTHER invocation heals a
    // crashed materialization and re-discloses the block WITHOUT re-merging (the conflict draft already
    // consumed `current`).
    if let Some(cs) = doc::read_doc::<topos_types::persisted::ConflictState>(ctx.fs, &sp.conflict)?
    {
        if inv == Invocation::Escape {
            // The 2nd witness mint site — guarded: a `conflict.json` only ever exists for an author who
            // diverged (a follower never reaches merge code, so never records one).
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
        super::merge_resolve::recover_resolution(ctx, &sp, &sync, &lock, &map, &cs)?;
        return super::merge_resolve::conflicted_row_from_state(&name, &sync, &cs);
    }

    // Whether THIS pull discovered a new target (moved `observed`). A confirm-each skill must re-offer such
    // a version rather than let an explicit accept apply bytes it never disclosed.
    let mut raised = false;

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
                raised = true;
            }
        }
        Err(PlaneError::NotFound) => return Ok(state_row(&name, &sync, PullAction::UpToDate)),
        Err(PlaneError::Unavailable(m) | PlaneError::Unreachable(m)) => {
            // Targeted accept: surface the failure. Bare sweep + the escape: fall through to drive `applied`
            // toward `observed` from the LOCAL store — a pending apply (or an escape) whose target is
            // already local still completes when the plane is unreachable (the escape is the offline-capable
            // no-deadlock guarantee); one that needs a fetch fails per-skill below, never a false UpToDate.
            if explicit && inv != Invocation::Escape {
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
    let applied_eq_observed = sync.applied == sync.observed;
    let plan = placement::plan_for_skill(ctx, skill_id, &lock, &map);
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
            converge_placements(ctx, &sp, skill_id, &lock, &sync, &map, &managed, &work)?;
            return Ok(state_row(&name, &sync, PullAction::UpToDate));
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
    let target_digest = ensure_local(ctx, &store, skill_id, target_commit, 0, &mut written)?
        .unwrap_or_else(|| unreachable!("depth-0 ensure_local errors instead of shallow-stopping"));
    // Once, after the whole backfill — exactly the versions THIS op wrote (plus the target's own set when
    // already local), durable before any JSON records the target. Never the whole store: the per-pull
    // fsync cost is bounded by the fetched bytes, not lifetime history.
    fsync_batch(ctx, &written)?;
    // A digest mismatch on the rendered bytes is a loud integrity ERROR (content addressing is the integrity
    // story) — corruption evidence, never a transient skip.
    let bundle = store.render_verified(target_commit, target_digest)?;
    let target_digest_hex = to_hex(&target_digest);
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
            Ok(applied_row(&name, &sync, target_commit))
        }
        ApplyClass::CleanForward => {
            // A swap happens only here, and only when `work_eq_base` (a clean follower) — so a swap never
            // overwrites a local draft.
            if topos_core::consent::decide(situation_for(follow, explicit, raised, first_receive))
                .applies_bytes()
            {
                apply_forward(ctx, &sp, &map, &managed, &lock, &sync, skill_id, &t)?;
                Ok(applied_row(&name, &sync, target_commit))
            } else {
                // confirm-each / first-receive TOFU: re-disclose the digest as a one-tap offer; nothing
                // materializes (a bare sweep never auto-lands a never-received skill — I-TOFU).
                Ok(offer_row(&name, &sync, target_commit, &target_digest_hex))
            }
        }
        ApplyClass::Diverged => {
            // ④ a GENUINE local draft vs a newer remote — resolve it (author-side three-way merge / escape).
            // DIVERGED implies `work != base`, which can only hold for a Draft work tree (the ONE
            // edited copy — several divergent copies froze typed back in compute_work).
            let WorkState::Draft { scanned } = &work.state else {
                return Ok(diverged_row(&name, &sync, target_commit, None));
            };
            // The structural author-only gate: the witness is minted ONLY here.
            let witness = DivergedWitness(());
            use super::merge_resolve::{ResolveStrategy, resolve_diverged};
            let confirm_each = follow.mode == FollowMode::ConfirmEach;
            let strategy = match inv {
                Invocation::Escape => Some(ResolveStrategy::Escape),
                // An explicit accept merges the disclosed divergence — UNLESS this pull discovered a
                // strictly-newer `current` (`raised`) for a confirm-each skill: that version's digest was
                // never offered, so re-offer it instead of merging undisclosed bytes (mirrors the
                // clean-forward `situation_for(raised)` re-offer).
                Invocation::Accept if confirm_each && raised => None,
                Invocation::Accept => Some(ResolveStrategy::Merge),
                // Full-auto: an AUTO follower's bare sweep runs the full merge unattended; a confirm-each
                // follower is surfaced instead (auto-merging would land theirs without the one-tap accept).
                Invocation::Sweep if confirm_each => None,
                Invocation::Sweep => Some(ResolveStrategy::Merge),
            };
            match strategy {
                Some(strategy) => resolve_diverged(
                    witness,
                    ctx,
                    skill_id,
                    &sp,
                    &sync,
                    &lock,
                    &map,
                    scanned,
                    &bundle,
                    target_commit,
                    strategy,
                ),
                None => {
                    let draft_id = snapshot_draft(ctx, &sp, &lock, scanned)?;
                    Ok(diverged_row(&name, &sync, target_commit, Some(draft_id)))
                }
            }
        }
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
        applied: GO_BACK_APPLIED,
        base_commit: target_hex.clone(),
        work_hash: target_digest_hex.clone(),
        held: true,
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
        offer: None,
        conflict: None,
        merge: None,
    })
}

/// `update --reset <skill>` — DISCARD the local draft, restoring the followed `current` (an imported
/// skill's adopted origin snapshot). The draft is snapshotted into the sidecar store FIRST (never lost —
/// recoverable), then the base bytes are re-materialized over the placement. `observed`/`applied` are
/// untouched (the team's current did not move) and `held` stays `false`, so a later sweep sees the skill
/// current again. A pristine working tree is a clean no-op (nothing to discard).
///
/// # Errors
/// [`ClientError::PlacementUnsupported`] on an unscannable placement; a store / io / integrity failure.
pub(crate) fn reset_to_base(
    ctx: &Ctx<'_>,
    skill_id: &crate::id::SkillId,
) -> Result<[u8; 32], ClientError> {
    let _guard = sidecar::lock_skill(ctx.fs, &ctx.layout, skill_id)?;
    let sp = ctx.layout.published(skill_id);
    let sid = skill_id.as_str();
    let sync: SyncState = read_required(ctx, &sp.sync, "sync.json")?;
    let lock: Lock = read_required(ctx, &sp.lock, "lock.json")?;
    let map: PlacementMap = read_map_required(ctx, &sp)?;
    let plan = placement::plan_for_skill(ctx, sid, &lock, &map);
    let map = placement::reconcile_map(&map, &plan);
    let managed = placement::managed_indices(&map, &plan);

    let base = super::parse_hex32(&lock.base_commit)?;
    let base_digest = super::parse_hex32(&lock.bundle_digest)?;
    let base_digest_hex = lock.bundle_digest.clone();

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
        },
    )?;
    log_apply(ctx, sid, "update-reset", base, &report);
    Ok(base)
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
/// `ReviewRequiredApproved`; auto is `FollowedAutoNewVersion`; confirm-each is `FollowedConfirmEach`.
///
/// An explicit `topos pull <skill>` is the user accepting a **previously-disclosed** pending version: it
/// maps to `ExplicitLocalPull` (a direct local command that authorizes the apply and re-binds the digest)
/// ONLY when this pull did NOT discover a newer version (`!raised`). If the pointer advanced during the
/// accept (`raised`), that newer version was never offered, so it goes through the follow-mode gate — a
/// confirm-each skill re-offers it (re-disclosing its digest) rather than applying bytes it never showed.
///
/// A `first_receive` skill is TOFU (I-TOFU): a bare sweep maps to `FirstReceiveFromLink` (an OFFER, never
/// auto-landed — even an `auto` follower), while an explicit accept / `--approve` is the user's direct
/// first-receive yes and maps to `ExplicitLocalPull` (places the first bytes). This takes precedence over
/// the follow-mode gate, so a never-received skill is never silently materialized by a session-start sweep.
fn situation_for(
    follow: &FollowContext,
    explicit: bool,
    raised: bool,
    first_receive: bool,
) -> topos_core::consent::Situation {
    use topos_core::consent::Situation;
    if first_receive {
        return if explicit {
            Situation::ExplicitLocalPull
        } else {
            Situation::FirstReceiveFromLink
        };
    }
    if explicit && !raised {
        Situation::ExplicitLocalPull
    } else if follow.review_required {
        Situation::ReviewRequiredApproved
    } else {
        match follow.mode {
            FollowMode::Auto => Situation::FollowedAutoNewVersion,
            FollowMode::ConfirmEach => Situation::FollowedConfirmEach,
        }
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
        },
    )?;
    log_apply(ctx, skill_id, "pull", t.commit, &report);
    Ok(())
}

/// Converge the MANAGED placements onto the ALREADY-APPLIED version from the LOCAL store: fill a
/// never-materialized / absent target (a newly detected harness, a fresh `--agent` scope) and refresh
/// a clean-but-stale copy — without any network and without touching `observed`/`applied` (the team's
/// current did not move; this is purely the local fan-out catching up). A draft copy is never
/// touched (only Absent / stale-Clean dirs are targets), and a never-received baseline (no local
/// bytes yet) is a no-op.
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
) -> Result<(), ClientError> {
    if is_zero_commit(&lock.base_commit) {
        return Ok(()); // never received — nothing local to place yet
    }
    let missing: Vec<usize> = managed
        .iter()
        .copied()
        .filter(|&i| {
            work.scans.get(i).is_some_and(|s| match &s.status {
                ScanStatus::Absent => true,
                // A clean REPLICA at a different version than the lock's base is stale — refreshed
                // ONLY when the work tree itself is at base (never toward or over a live draft:
                // a recorded draft-on-current keeps every copy untouched until it resolves).
                ScanStatus::Clean { scanned } => {
                    matches!(work.state, WorkState::CleanAtBase)
                        && to_hex(&scanned.bundle_digest) != lock.bundle_digest
                }
                // Edited, foreign, or unreadable dirs are never converge targets.
                ScanStatus::Modified { .. } | ScanStatus::Foreign | ScanStatus::Unscannable => {
                    false
                }
            })
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
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
        },
    )?;
    log_apply(ctx, skill_id, "converge", base, &report);
    Ok(())
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
                ScanStatus::Clean { scanned } | ScanStatus::Modified { scanned } => {
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
                "version {} not served",
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
/// wedging forever, and a go-back to such a version is refused as unknown.
fn store_bundle_digest_opt(
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
fn store_bundle_digest(store: &Store, version_id: [u8; 32]) -> Result<[u8; 32], ClientError> {
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
}

/// Classify the placements into ONE work tree — draft-anywhere:
///
/// - Each copy is scanned against ITS OWN recorded per-placement sha. Exactly one distinct EDITED
///   copy ⇒ that copy is THE work tree (the draft may live in the shared dir or any native copy);
///   MORE than one (with differing bytes) ⇒ the typed [`ClientError::PlacementsDiverged`] freeze —
///   nothing is overwritten, every edited path is disclosed, and `update --reset` (or
///   hand-reconciling) is the named way out.
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
        });
    }
    let modified = placement::distinct_modified(&scans);
    if modified.len() > 1 {
        return Err(placement::placements_diverged(skill_name, &scans));
    }
    // The work tree: the single edited copy when one exists, else the first (canonical) present copy.
    let chosen: Option<&placement::PlacementScan> = match modified.first() {
        Some((idx, _)) => Some(&scans[*idx]),
        None => scans.iter().find(|s| {
            matches!(
                s.status,
                ScanStatus::Clean { .. } | ScanStatus::Modified { .. }
            )
        }),
    };
    let state = match chosen {
        None => WorkState::Absent,
        Some(s) => {
            let scanned = match &s.status {
                ScanStatus::Clean { scanned } | ScanStatus::Modified { scanned } => scanned,
                _ => unreachable!("chosen is always a scanned copy"),
            };
            if to_hex(&scanned.bundle_digest) == lock.bundle_digest {
                WorkState::CleanAtBase
            } else {
                WorkState::Draft {
                    scanned: scanned.clone(),
                }
            }
        }
    };
    Ok(WorkTree { scans, state })
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
        Err(PlaneError::Unavailable(m) | PlaneError::Unreachable(m) | PlaneError::Malformed(m)) => {
            Err(ClientError::Plane(m))
        }
    }
}

/// Whether `g` is the genesis sentinel `(0,0)`.
fn is_zero_gen(g: u64) -> bool {
    g == 0
}

/// Whether this followed skill has NEVER received bytes — the first-receive baseline `follow` lays: nothing
/// applied, on the all-zero base. An `add`-ed skill carries a real local genesis (a non-zero `base_commit`),
/// and a received skill has applied a version (`applied` > `(0,0)`), so neither is ever mistaken for it.
///
/// DURABLE across sweeps: keyed on `applied` + the zero base, NOT `observed`. A bare sweep that only OFFERS a
/// first-receive baseline still moves `observed` to the served target (so the conditional GET keeps working)
/// — which would make `observed` non-zero after sweep 1, so keying on it would let a SECOND auto sweep
/// mistake the still-unapproved baseline for a normal followed skill and AUTO-LAND it (breaking I-TOFU).
/// `applied` stays `(0,0)` and `base_commit` stays all-zero until the first explicit accept actually
/// MATERIALIZES bytes, so they remain a true "never placed" signal every sweep.
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

fn state_row(name: &str, sync: &SyncState, action: PullAction) -> PullSkill {
    // `workspace_id` is stamped by the pull aggregator (`pull.rs`), which holds the follow-state; every row
    // builder here leaves it `None`.
    PullSkill {
        skill: name.to_owned(),
        workspace_id: None,
        observed: sync.observed,
        applied: sync.applied,
        action,
        offer: None,
        conflict: None,
        merge: None,
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
        offer: None,
        conflict: None,
        merge: None,
    }
}

fn offer_row(name: &str, sync: &SyncState, target: [u8; 32], target_digest_hex: &str) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        workspace_id: None,
        observed: sync.observed,
        applied: sync.applied,
        action: PullAction::Offered,
        offer: Some(Offer {
            version_id: to_hex(&target),
            bundle_digest: target_digest_hex.to_owned(),
        }),
        conflict: None,
        merge: None,
    }
}

fn diverged_row(
    name: &str,
    sync: &SyncState,
    target: [u8; 32],
    draft_id: Option<String>,
) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        workspace_id: None,
        observed: sync.observed,
        applied: sync.applied,
        action: PullAction::Diverged,
        offer: None,
        conflict: Some(Conflict {
            remote_version_id: to_hex(&target),
            local_version_id: draft_id,
        }),
        merge: None,
    }
}
