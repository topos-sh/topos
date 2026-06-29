//! Author-side resolution of a DIVERGED draft: the three-way merge, the conflict materialization, the
//! disclosed "fresh commit on current" escape, and the 2-way no-base fallback — plus crash recovery.
//!
//! The kernel ([`topos_core::merge`]) decides the per-path plan + the outcome; [`topos_gitstore::merge`]
//! runs the per-file byte merge; this module orchestrates: render base/mine/theirs, assemble the complete
//! resolved (or conflict-marked) tree, commit it as a **forward 1-parent** commit on `current`, and place
//! it via the existing crash-safe [`crate::materialize`] dir-swap. A clean merge lands a **draft-on-current**
//! (state ③ with `base = theirs`); a conflict writes a complete marker tree AND a durable [`ConflictState`]
//! (`conflict.json`) that is both the publish-block fact and a pre-swap recovery journal.
//!
//! ## Structural author-only
//!
//! [`resolve_diverged`] takes a [`DivergedWitness`] by value. That token's field is private to
//! [`super::sync_engine`], which mints it ONLY in the post-fetch `Diverged` arm (reachable only when
//! `work != base`). No other code in the crate can construct one, so merge code is unreachable from a
//! current/behind/clean-draft state by construction — not a role check.
//!
//! ## Crash safety (the highest-risk invariant)
//!
//! A conflict tree carries remote-controlled marker bytes, so it must never be published nor re-merged
//! into nested markers. The conflict path commits `M` (the result tree) + writes `conflict.json` BEFORE
//! the swap; [`recover_resolution`] re-materializes `M` only when the placement holds the conflict tree,
//! still holds the pre-resolution draft, or is absent (the swap's crash windows) — and never clobbers an
//! edited tree. The escape is idempotent: a crash before it clears `conflict.json` is healed by re-running
//! it (the guard stays conservatively ON until the clear completes).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::merge::{
    ContentMergeResult, FileId, FileSetConflictKind, MergeOutcome, PathPlan, decide_outcome,
    plan_merge,
};
use topos_core::sign::{self, Commit};
use topos_gitstore::{
    DiffFile, ImportFile, MergeFileResult, RenderedBundle, RenderedFile, Store, merge_file,
    unified_diff,
};
use topos_types::persisted::{
    ConflictPath, ConflictPathKind, ConflictReason, ConflictState, Lock, PlacementMap, SyncState,
};
use topos_types::results::{ConflictPathReport, MergeReport, PullAction, PullSkill};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::materialize::{self, MaterializeReq};
use crate::scan::ScannedBundle;
use crate::sidecar::SkillPaths;
use crate::{doc, logfile};

use super::sync_engine::{
    DivergedWitness, WorkState, compute_work, first_placement, forwarded_sync, fsync_store,
    lock_from_bundle, map_core, snapshot_draft,
};

/// The fixed commit messages for the resolution commits (folded into the `version_id`; must stay constant).
const MERGE_CLEAN_MESSAGE: &str = "topos: merge";
const MERGE_CONFLICT_MESSAGE: &str = "topos: merge conflict";
const MERGE_ESCAPE_MESSAGE: &str = "topos: merge escape";
const MERGE_NOBASE_MESSAGE: &str = "topos: merge no-base";
/// The suffix for the preserved "mine" side when a conflict keeps both versions on disk.
const SIDECAR_SUFFIX: &str = ".topos-mine";

/// How the author asked to resolve a diverged draft.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveStrategy {
    /// Run the three-way merge (clean → draft-on-current; conflict → markers + block).
    Merge,
    /// The disclosed escape: commit MY bytes on top of `current`, dropping the merge (a 2-way diff of what
    /// is dropped is surfaced); the pre-escape draft is snapshotted recoverably.
    Escape,
}

/// A borrowed side of the merge — bytes + mode, regardless of whether it came from a scan or a render.
#[derive(Clone, Copy)]
struct Side<'a> {
    mode: FileMode,
    bytes: &'a [u8],
}

/// Resolve a diverged draft. `mine` is the working tree (already scanned), `theirs` the fetched `current`,
/// `theirs_commit` its `version_id`. Reachable only with a [`DivergedWitness`] (see the module docs).
#[allow(clippy::too_many_arguments)]
pub(crate) fn resolve_diverged(
    _witness: DivergedWitness,
    ctx: &Ctx<'_>,
    skill_id: &str,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    mine: &ScannedBundle,
    theirs: &RenderedBundle,
    theirs_commit: [u8; 32],
    strategy: ResolveStrategy,
) -> Result<PullSkill, ClientError> {
    // The escape and the no-base fallback both re-parent MINE onto `current`, so neither needs a renderable
    // base (the base is exactly what is gone in the no-base case); only the 3-way merge does. So render the
    // base FIRST and branch — snapshotting the draft on its base only on the path that has one.
    let store = Store::open(&sp.store)?;
    let base_commit = super::parse_hex32(&lock.base_commit)?;
    let base_digest = super::parse_hex32(&lock.bundle_digest)?;

    if strategy == ResolveStrategy::Escape {
        // A fresh escape (no recorded conflict): the placement holds MINE (no markers), so commit it.
        let committed = scanned_to_bundle(mine)?;
        return escape(
            ctx,
            skill_id,
            sp,
            sync,
            lock,
            map,
            &committed,
            theirs,
            theirs_commit,
        );
    }

    // Render the base; if it cannot be rendered (unrelated histories / pruned base), fall back to 2-way.
    let Ok(base) = store.render_verified(base_commit, base_digest) else {
        return no_base(
            ctx,
            skill_id,
            sp,
            sync,
            lock,
            map,
            mine,
            theirs,
            theirs_commit,
            base_commit,
            base_digest,
        );
    };

    // The base renders, so its commit is a valid parent: snapshot the working draft on it (never lost,
    // and the recoverable `draft_commit`).
    let draft_id = snapshot_draft(ctx, sp, lock, mine)?;
    let draft_commit = super::parse_hex32(&draft_id)?;

    // Plan over metadata, then run the byte merges the plan calls for, assembling the complete tree.
    let plan = plan_merge(&file_ids(&base), &scanned_file_ids(mine), &file_ids(theirs));
    let base_map = render_map(&base);
    let mine_map = scanned_map(mine);
    let theirs_map = render_map(theirs);

    let assembled = assemble(&plan, &base_map, &mine_map, &theirs_map)?;
    let outcome = decide_outcome(&plan, &assembled.content_results);
    let merged = build_bundle(assembled.files)?;
    let merged_digest_hex = to_hex(&merged.bundle_digest);

    match outcome {
        MergeOutcome::CleanCommitOnTip => {
            let result_commit =
                commit_result(ctx, &store, theirs_commit, &merged, MERGE_CLEAN_MESSAGE)?;
            // Clear any (defensively) stale conflict record, then place the merged draft-on-current.
            ctx.fs.remove_file(&sp.conflict)?;
            place_draft_on_current(
                ctx,
                skill_id,
                sp,
                sync,
                lock,
                map,
                &merged,
                theirs,
                theirs_commit,
            )?;
            log_resolution(ctx, skill_id, "merge", result_commit);
            Ok(merged_row(
                &lock.name,
                sync,
                base_commit,
                theirs_commit,
                result_commit,
                &merged_digest_hex,
                None,
            ))
        }
        MergeOutcome::BlockedConflict => {
            let result_commit =
                commit_result(ctx, &store, theirs_commit, &merged, MERGE_CONFLICT_MESSAGE)?;
            // The journal is written + fsynced BEFORE the swap, so a crash mid-materialize is recoverable.
            let cs = ConflictState {
                schema_version: topos_types::SCHEMA_VERSION,
                base_commit: to_hex(&base_commit),
                base_digest: to_hex(&base.bundle_digest),
                current_commit: to_hex(&theirs_commit),
                current_digest: to_hex(&theirs.bundle_digest),
                draft_commit: to_hex(&draft_commit),
                draft_digest: to_hex(&mine.bundle_digest),
                result_commit: to_hex(&result_commit),
                conflicted_digest: merged_digest_hex.clone(),
                reason: ConflictReason::ThreeWay,
                paths: assembled.conflicts.clone(),
            };
            doc::write_doc(ctx.fs, &sp.conflict, &cs)?;
            place_draft_on_current(
                ctx,
                skill_id,
                sp,
                sync,
                lock,
                map,
                &merged,
                theirs,
                theirs_commit,
            )?;
            log_resolution(ctx, skill_id, "merge-conflict", result_commit);
            Ok(conflicted_row(
                &lock.name,
                sync,
                base_commit,
                theirs_commit,
                result_commit,
                &merged_digest_hex,
                conflict_reports(&assembled.conflicts),
                None,
            ))
        }
        // `decide_outcome` never returns NoBaseTwoWay — that branch is taken before planning (above).
        MergeOutcome::NoBaseTwoWay => Err(ClientError::Corrupt(
            "decide_outcome returned NoBaseTwoWay".into(),
        )),
    }
}

/// The escape: commit the author's chosen bytes (`committed`) on top of `current` (a fresh 1-parent commit
/// — which also snapshots them recoverably), place it draft-on-current, and clear any conflict record.
/// Always produces a clean, publishable candidate (no deadlock), and needs no renderable base. The CALLER
/// chooses `committed`: a fresh escape commits the placement (mine); a recorded-conflict escape commits the
/// author's edited resolution, or — if the placement is still the raw conflict tree — their ORIGINAL draft,
/// so the escape never commits unresolved markers as a publishable bundle.
#[allow(clippy::too_many_arguments)]
fn escape(
    ctx: &Ctx<'_>,
    skill_id: &str,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    committed: &RenderedBundle,
    theirs: &RenderedBundle,
    theirs_commit: [u8; 32],
) -> Result<PullSkill, ClientError> {
    let store = Store::open(&sp.store)?;
    let merged_digest_hex = to_hex(&committed.bundle_digest);
    let result_commit = commit_result(ctx, &store, theirs_commit, committed, MERGE_ESCAPE_MESSAGE)?;
    let drop = drop_diff(theirs, committed);

    place_draft_on_current(
        ctx,
        skill_id,
        sp,
        sync,
        lock,
        map,
        committed,
        theirs,
        theirs_commit,
    )?;
    // The escape RESOLVES — clear the block last (idempotent: a crash before this is healed by re-running).
    ctx.fs.remove_file(&sp.conflict)?;
    log_resolution(ctx, skill_id, "merge-escape", result_commit);
    Ok(merged_row(
        &lock.name,
        sync,
        super::parse_hex32(&lock.base_commit).unwrap_or([0u8; 32]),
        theirs_commit,
        result_commit,
        &merged_digest_hex,
        Some(drop),
    ))
}

/// The no-base fallback: keep MINE on disk, block, and surface a 2-way diff of what theirs would add —
/// never a silent merge. The author resolves by editing or escaping.
#[allow(clippy::too_many_arguments)]
fn no_base(
    ctx: &Ctx<'_>,
    skill_id: &str,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    mine: &ScannedBundle,
    theirs: &RenderedBundle,
    theirs_commit: [u8; 32],
    base_commit: [u8; 32],
    base_digest: [u8; 32],
) -> Result<PullSkill, ClientError> {
    let store = Store::open(&sp.store)?;
    let merged = scanned_to_bundle(mine)?; // keep mine; never merge unrelated trees silently
    let merged_digest_hex = to_hex(&merged.bundle_digest);
    // `M` = mine re-parented on `current`; it both records the result and snapshots mine recoverably (the
    // base is unrenderable, so a base-parented snapshot is impossible — M is the recoverable draft).
    let result_commit = commit_result(ctx, &store, theirs_commit, &merged, MERGE_NOBASE_MESSAGE)?;
    let cs = ConflictState {
        schema_version: topos_types::SCHEMA_VERSION,
        base_commit: to_hex(&base_commit),
        base_digest: to_hex(&base_digest),
        current_commit: to_hex(&theirs_commit),
        current_digest: to_hex(&theirs.bundle_digest),
        draft_commit: to_hex(&result_commit),
        draft_digest: merged_digest_hex.clone(),
        result_commit: to_hex(&result_commit),
        conflicted_digest: merged_digest_hex.clone(),
        reason: ConflictReason::NoBase,
        paths: Vec::new(),
    };
    doc::write_doc(ctx.fs, &sp.conflict, &cs)?;
    place_draft_on_current(
        ctx,
        skill_id,
        sp,
        sync,
        lock,
        map,
        &merged,
        theirs,
        theirs_commit,
    )?;
    log_resolution(ctx, skill_id, "merge-no-base", result_commit);
    Ok(conflicted_row(
        &lock.name,
        sync,
        base_commit,
        theirs_commit,
        result_commit,
        &merged_digest_hex,
        Vec::new(),
        Some(drop_diff(theirs, &merged)),
    ))
}

/// Escape a skill that already holds a RECORDED conflict (state ③ draft-on-current + `conflict.json`): a
/// conflict consumed `theirs` into the (blocked) draft, so `applied == observed` and the normal DIVERGED
/// apply-arm is no longer reached — the escape resolves it here, committing MINE on the conflict's
/// `current`. Reachable only with a [`DivergedWitness`] (`conflict.json` ⟹ an author divergence).
#[allow(clippy::too_many_arguments)]
pub(crate) fn escape_recorded(
    witness: DivergedWitness,
    ctx: &Ctx<'_>,
    skill_id: &str,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    cs: &ConflictState,
) -> Result<PullSkill, ClientError> {
    let _ = witness; // the structural gate; the private `escape` below needs no token
    let WorkState::Present {
        scanned,
        digest_hex,
        ..
    } = compute_work(ctx, map, lock)?
    else {
        // The placement is gone / unreadable — there is nothing to commit, so the conflict is moot; clear
        // the (now-pointless) block rather than wedge.
        ctx.fs.remove_file(&sp.conflict)?;
        return Ok(PullSkill {
            skill: lock.name.clone(),
            observed: sync.observed,
            applied: sync.applied,
            action: PullAction::UpToDate,
            offer: None,
            conflict: None,
            merge: None,
        });
    };
    let store = Store::open(&sp.store)?;
    let theirs_commit = super::parse_hex32(&cs.current_commit)?;
    let theirs = store.render_verified(theirs_commit, super::parse_hex32(&cs.current_digest)?)?;
    // The bytes to commit. If the placement is STILL the raw, unedited conflict tree, committing it would
    // publish the markers — instead commit the author's ORIGINAL draft ("escape without editing" = drop the
    // merge, take my pre-merge bytes). An edited placement is the author's hand-resolution → commit it.
    let committed = if digest_hex == cs.conflicted_digest {
        store.render_verified(
            super::parse_hex32(&cs.draft_commit)?,
            super::parse_hex32(&cs.draft_digest)?,
        )?
    } else {
        scanned_to_bundle(&scanned)?
    };
    escape(
        ctx,
        skill_id,
        sp,
        sync,
        lock,
        map,
        &committed,
        &theirs,
        theirs_commit,
    )
}

/// Recover a resolution interrupted by a crash. Renders the recorded result `M` and re-materializes it
/// ONLY when the placement holds the conflict tree, still holds the pre-resolution draft, or is absent —
/// the materialize crash windows. An edited (or unscannable) tree is left untouched (never clobbered).
pub(crate) fn recover_resolution(
    ctx: &Ctx<'_>,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    cs: &ConflictState,
) -> Result<(), ClientError> {
    let store = Store::open(&sp.store)?;
    let result_commit = super::parse_hex32(&cs.result_commit)?;
    let conflicted_digest = super::parse_hex32(&cs.conflicted_digest)?;
    let theirs_commit = super::parse_hex32(&cs.current_commit)?;
    let current_digest = super::parse_hex32(&cs.current_digest)?;

    // The deterministic render target + theirs (for lock-as-base). A render failure here is genuine
    // corruption, surfaced loudly rather than healed.
    let result = store.render_verified(result_commit, conflicted_digest)?;
    let theirs = store.render_verified(theirs_commit, current_digest)?;

    let do_heal = match compute_work(ctx, map, lock)? {
        WorkState::Absent => true, // mid-swap absent window → finish the first-install.
        WorkState::Unscannable => false, // never clobber an unreadable tree.
        WorkState::Present { digest_hex, .. } => {
            // The conflict tree is on disk (finish docs) OR the pre-resolution draft is still there (the
            // swap never ran) — both heal to the result. Anything else is an author edit; leave it.
            digest_hex == cs.conflicted_digest || digest_hex == cs.draft_digest
        }
    };
    if do_heal {
        place_draft_on_current(
            ctx,
            &lock.skill_id,
            sp,
            sync,
            lock,
            map,
            &result,
            &theirs,
            theirs_commit,
        )?;
    }
    Ok(())
}

// --------------------------------------------------------------------------------------------------
// Tree assembly.
// --------------------------------------------------------------------------------------------------

/// The complete resolved tree + the per-content-merge verdicts (in plan order) + the conflicting paths.
struct Assembled {
    files: Vec<RenderedFile>,
    content_results: Vec<ContentMergeResult>,
    conflicts: Vec<ConflictPath>,
}

/// Assemble the complete on-disk tree from the plan + the three side maps, running the byte merges the
/// plan calls for. One pass: the `content_results` come out in plan order (what [`decide_outcome`] wants).
fn assemble(
    plan: &topos_core::merge::MergePlan,
    base_map: &BTreeMap<&str, Side<'_>>,
    mine_map: &BTreeMap<&str, Side<'_>>,
    theirs_map: &BTreeMap<&str, Side<'_>>,
) -> Result<Assembled, ClientError> {
    let union: BTreeSet<&str> = plan.paths.iter().map(|p| p.path.as_str()).collect();
    let mut emitted: BTreeSet<String> = BTreeSet::new();
    let mut files: Vec<RenderedFile> = Vec::new();
    let mut content_results = Vec::new();
    let mut conflicts = Vec::new();

    let mut emit = |files: &mut Vec<RenderedFile>,
                    emitted: &mut BTreeSet<String>,
                    path: String,
                    mode: FileMode,
                    bytes: Vec<u8>| {
        emitted.insert(path.clone());
        files.push(RenderedFile {
            content_sha256: digest::sha256(&bytes),
            path,
            mode,
            bytes,
        });
    };

    for pp in &plan.paths {
        let path = pp.path.as_str();
        match &pp.plan {
            PathPlan::Delete => {}
            PathPlan::TakeEither { mode, .. } => {
                let s = side(mine_map, path)?;
                emit(
                    &mut files,
                    &mut emitted,
                    pp.path.clone(),
                    *mode,
                    s.bytes.to_vec(),
                );
            }
            PathPlan::TakeMine { mode, .. } => {
                let s = side(mine_map, path)?;
                emit(
                    &mut files,
                    &mut emitted,
                    pp.path.clone(),
                    *mode,
                    s.bytes.to_vec(),
                );
            }
            PathPlan::TakeTheirs { mode, .. } => {
                let s = side(theirs_map, path)?;
                emit(
                    &mut files,
                    &mut emitted,
                    pp.path.clone(),
                    *mode,
                    s.bytes.to_vec(),
                );
            }
            PathPlan::ContentMerge { mode, .. } => {
                let b = side(base_map, path)?;
                let m = side(mine_map, path)?;
                let t = side(theirs_map, path)?;
                match merge_file(b.bytes, m.bytes, t.bytes) {
                    Ok(MergeFileResult::Clean(bytes)) => {
                        emit(&mut files, &mut emitted, pp.path.clone(), *mode, bytes);
                        content_results.push(ContentMergeResult::Clean);
                    }
                    Ok(MergeFileResult::Conflict(bytes)) => {
                        emit(&mut files, &mut emitted, pp.path.clone(), *mode, bytes);
                        content_results.push(ContentMergeResult::Conflicted);
                        conflicts.push(cpath(path, ConflictPathKind::Content));
                    }
                    Ok(MergeFileResult::Binary) => {
                        keep_both(&mut files, &mut emitted, &mut emit, &union, path, m, t);
                        content_results.push(ContentMergeResult::Conflicted);
                        conflicts.push(cpath(path, ConflictPathKind::BinaryContent));
                    }
                    Err(_) => {
                        keep_both(&mut files, &mut emitted, &mut emit, &union, path, m, t);
                        content_results.push(ContentMergeResult::Conflicted);
                        conflicts.push(cpath(path, ConflictPathKind::Oversize));
                    }
                }
            }
            PathPlan::FileSetConflict { kind } => match kind {
                FileSetConflictKind::ModifyDelete => {
                    let m = side(mine_map, path)?;
                    emit(
                        &mut files,
                        &mut emitted,
                        pp.path.clone(),
                        m.mode,
                        m.bytes.to_vec(),
                    );
                    conflicts.push(cpath(path, ConflictPathKind::ModifyDelete));
                }
                FileSetConflictKind::DeleteModify => {
                    let t = side(theirs_map, path)?;
                    emit(
                        &mut files,
                        &mut emitted,
                        pp.path.clone(),
                        t.mode,
                        t.bytes.to_vec(),
                    );
                    conflicts.push(cpath(path, ConflictPathKind::DeleteModify));
                }
                FileSetConflictKind::AddAddDifferent => {
                    let m = side(mine_map, path)?;
                    let t = side(theirs_map, path)?;
                    keep_both(&mut files, &mut emitted, &mut emit, &union, path, m, t);
                    conflicts.push(cpath(path, ConflictPathKind::AddAdd));
                }
                FileSetConflictKind::AddAddModeDiffers => {
                    // Identical content, disagreeing modes — keep theirs' bytes + mode, flag the disagreement.
                    let t = side(theirs_map, path)?;
                    emit(
                        &mut files,
                        &mut emitted,
                        pp.path.clone(),
                        t.mode,
                        t.bytes.to_vec(),
                    );
                    conflicts.push(cpath(path, ConflictPathKind::ModeMode));
                }
            },
        }
    }

    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    Ok(Assembled {
        files,
        content_results,
        conflicts,
    })
}

/// Keep both sides of a conflict on disk: theirs at the path, mine in a deterministically-disambiguated
/// `.topos-mine` sidecar (so the author can compare + resolve in place).
#[allow(clippy::type_complexity)]
fn keep_both(
    files: &mut Vec<RenderedFile>,
    emitted: &mut BTreeSet<String>,
    emit: &mut impl FnMut(&mut Vec<RenderedFile>, &mut BTreeSet<String>, String, FileMode, Vec<u8>),
    union: &BTreeSet<&str>,
    path: &str,
    mine: Side<'_>,
    theirs: Side<'_>,
) {
    emit(
        files,
        emitted,
        path.to_owned(),
        theirs.mode,
        theirs.bytes.to_vec(),
    );
    let side_path = sidecar_path(path, union, emitted);
    emit(files, emitted, side_path, mine.mode, mine.bytes.to_vec());
}

/// A `.topos-mine` sidecar path that collides with neither a real bundle path nor an already-emitted one —
/// under the SAME equivalence the kernel digest enforces (exact / NFC / ASCII case-fold), not just exact
/// bytes. A byte-distinct-but-case-fold/NFC-colliding name would pass an exact check yet make the assembled
/// tree's `bundle_digest` reject, so we compare normalized forms (`digest::normalize_for_collision`).
fn sidecar_path(path: &str, union: &BTreeSet<&str>, emitted: &BTreeSet<String>) -> String {
    let taken: BTreeSet<String> = union
        .iter()
        .map(|p| digest::normalize_for_collision(p))
        .chain(emitted.iter().map(|p| digest::normalize_for_collision(p)))
        .collect();
    let base = format!("{path}{SIDECAR_SUFFIX}");
    if !taken.contains(&digest::normalize_for_collision(&base)) {
        return base;
    }
    for i in 1.. {
        let candidate = format!("{base}-{i}");
        if !taken.contains(&digest::normalize_for_collision(&candidate)) {
            return candidate;
        }
    }
    unreachable!("the suffix search is unbounded")
}

fn cpath(path: &str, kind: ConflictPathKind) -> ConflictPath {
    ConflictPath {
        path: path.to_owned(),
        kind,
    }
}

// --------------------------------------------------------------------------------------------------
// Placement + commit.
// --------------------------------------------------------------------------------------------------

/// Place `merged`'s bytes on the placement and advance the docs to **draft-on-current**: `base = theirs`,
/// `lock = theirs` (so the working bytes read as a draft), `applied = observed`, `work_hash = merged`.
/// Reuses the crash-safe dir-swap; the currency/harness hook is NOT fired (materialize only writes bytes).
#[allow(clippy::too_many_arguments)]
fn place_draft_on_current(
    ctx: &Ctx<'_>,
    skill_id: &str,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    merged: &RenderedBundle,
    theirs: &RenderedBundle,
    theirs_commit: [u8; 32],
) -> Result<(), ClientError> {
    let merged_digest_hex = to_hex(&merged.bundle_digest);
    let next_lock = lock_from_bundle(lock, theirs_commit, theirs);
    let next_sync = forwarded_sync(sync, theirs_commit, &merged_digest_hex);
    let placement = first_placement(map)?;
    materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id,
            placement_dir: Path::new(&placement),
            bundle: merged,
            prior_map: map,
            next_map_core: map_core(map, theirs_commit, &merged_digest_hex),
            next_lock: &next_lock,
            next_sync: &next_sync,
            sp,
        },
    )?;
    Ok(())
}

/// Commit an assembled tree as a forward 1-parent commit on `parent`, returning its `version_id`.
fn commit_result(
    ctx: &Ctx<'_>,
    store: &Store,
    parent: [u8; 32],
    bundle: &RenderedBundle,
    message: &str,
) -> Result<[u8; 32], ClientError> {
    let id = sign::commit_id(&Commit {
        parents: &[parent],
        tree: bundle.bundle_digest,
        author: &ctx.device_id,
        message,
    })
    .map_err(|_| ClientError::Corrupt("merge result commit id".into()))?;
    let import: Vec<ImportFile<'_>> = bundle
        .files
        .iter()
        .map(|f| ImportFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    let tree = store.write_bundle(&import)?;
    store
        .commit(id, &[parent], &tree, &ctx.device_id, message)
        .map_err(|_| ClientError::Corrupt("merge result does not match its id".into()))?;
    fsync_store(ctx, store)?;
    Ok(id)
}

fn log_resolution(ctx: &Ctx<'_>, skill_id: &str, action: &str, result: [u8; 32]) {
    let _ = logfile::append_event(
        ctx.fs,
        &ctx.layout.log_path(),
        &serde_json::json!({
            "action": action,
            "skill_id": skill_id,
            "version_id": to_hex(&result),
            "at": ctx.clock.now_unix_millis(),
        }),
    );
}

// --------------------------------------------------------------------------------------------------
// Small conversions.
// --------------------------------------------------------------------------------------------------

fn file_ids(b: &RenderedBundle) -> Vec<FileId> {
    b.files
        .iter()
        .map(|f| FileId {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: f.content_sha256,
        })
        .collect()
}

fn scanned_file_ids(b: &ScannedBundle) -> Vec<FileId> {
    b.files
        .iter()
        .map(|f| FileId {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect()
}

fn render_map(b: &RenderedBundle) -> BTreeMap<&str, Side<'_>> {
    b.files
        .iter()
        .map(|f| {
            (
                f.path.as_str(),
                Side {
                    mode: f.mode,
                    bytes: &f.bytes,
                },
            )
        })
        .collect()
}

fn scanned_map(b: &ScannedBundle) -> BTreeMap<&str, Side<'_>> {
    b.files
        .iter()
        .map(|f| {
            (
                f.path.as_str(),
                Side {
                    mode: f.mode,
                    bytes: &f.bytes,
                },
            )
        })
        .collect()
}

/// A side present at `path` — absence is a contract violation (the plan never references an absent side).
fn side<'a>(map: &BTreeMap<&str, Side<'a>>, path: &str) -> Result<Side<'a>, ClientError> {
    map.get(path)
        .copied()
        .ok_or_else(|| ClientError::Corrupt(format!("merge plan references absent side at {path}")))
}

/// Build a [`RenderedBundle`] from assembled files, recomputing the canonical `bundle_digest`.
fn build_bundle(files: Vec<RenderedFile>) -> Result<RenderedBundle, ClientError> {
    let entries: Vec<ManifestEntry> = files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: f.content_sha256,
        })
        .collect();
    let bundle_digest = digest::bundle_digest(&entries)
        .map_err(|r| ClientError::Corrupt(format!("merge tree: {r:?}")))?;
    Ok(RenderedBundle {
        files,
        bundle_digest,
    })
}

fn scanned_to_bundle(b: &ScannedBundle) -> Result<RenderedBundle, ClientError> {
    let files = b
        .files
        .iter()
        .map(|f| RenderedFile {
            path: f.path.clone(),
            mode: f.mode,
            bytes: f.bytes.clone(),
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    build_bundle(files)
}

/// A 2-way unified diff of what choosing MINE drops vs theirs (theirs → mine), for the escape / no-base
/// disclosure. Both sides are sorted by raw path bytes (the renderer's contract).
fn drop_diff(theirs: &RenderedBundle, mine: &RenderedBundle) -> String {
    let t = diff_view(theirs);
    let m = diff_view(mine);
    unified_diff(&t, &m)
}

/// A bundle as `DiffFile` views, sorted by raw path bytes (the `unified_diff` contract).
fn diff_view(b: &RenderedBundle) -> Vec<DiffFile<'_>> {
    let mut v: Vec<DiffFile<'_>> = b
        .files
        .iter()
        .map(|f| DiffFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    v.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    v
}

fn conflict_reports(paths: &[ConflictPath]) -> Vec<ConflictPathReport> {
    paths
        .iter()
        .map(|c| ConflictPathReport {
            path: c.path.clone(),
            kind: c.kind,
        })
        .collect()
}

// --------------------------------------------------------------------------------------------------
// Row builders.
// --------------------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn merged_row(
    name: &str,
    sync: &SyncState,
    base: [u8; 32],
    theirs: [u8; 32],
    result: [u8; 32],
    result_digest_hex: &str,
    drop_diff: Option<String>,
) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        observed: sync.observed,
        applied: sync.observed, // the pending update is consumed into the merged draft
        action: PullAction::Merged,
        offer: None,
        conflict: None,
        merge: Some(MergeReport {
            base_version_id: to_hex(&base),
            theirs_version_id: to_hex(&theirs),
            result_version_id: to_hex(&result),
            result_digest: result_digest_hex.to_owned(),
            clean: true,
            conflicts: Vec::new(),
            drop_diff,
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn conflicted_row(
    name: &str,
    sync: &SyncState,
    base: [u8; 32],
    theirs: [u8; 32],
    result: [u8; 32],
    result_digest_hex: &str,
    conflicts: Vec<ConflictPathReport>,
    drop_diff: Option<String>,
) -> PullSkill {
    PullSkill {
        skill: name.to_owned(),
        observed: sync.observed,
        applied: sync.observed, // theirs is incorporated into the (blocked) conflict draft
        action: PullAction::Conflicted,
        offer: None,
        conflict: None,
        merge: Some(MergeReport {
            base_version_id: to_hex(&base),
            theirs_version_id: to_hex(&theirs),
            result_version_id: to_hex(&result),
            result_digest: result_digest_hex.to_owned(),
            clean: false,
            conflicts,
            drop_diff,
        }),
    }
}

/// Build the typed conflict row from a recorded [`ConflictState`] (re-disclosed each pull while blocked).
pub(crate) fn conflicted_row_from_state(
    name: &str,
    sync: &SyncState,
    cs: &ConflictState,
) -> Result<PullSkill, ClientError> {
    Ok(conflicted_row(
        name,
        sync,
        super::parse_hex32(&cs.base_commit)?,
        super::parse_hex32(&cs.current_commit)?,
        super::parse_hex32(&cs.result_commit)?,
        &cs.conflicted_digest,
        conflict_reports(&cs.paths),
        None,
    ))
}
