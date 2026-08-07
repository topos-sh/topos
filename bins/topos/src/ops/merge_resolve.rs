//! Author-side resolution of a DIVERGED draft: the three-way merge, the conflict materialization, the
//! disclosed "fresh commit on current" escape, and the 2-way no-base fallback — plus crash recovery.
//!
//! The kernel ([`topos_core::merge`]) decides the per-path plan + the outcome; [`topos_gitstore::merge`]
//! runs the per-file byte merge; this module orchestrates: render base/mine/theirs, assemble the complete
//! resolved (or conflict-marked) tree, and commit it as a **forward 1-parent** commit on `current`. A
//! clean merge lands a **draft-on-current** (state ③ with `base = theirs`) via the crash-safe
//! [`crate::materialize`] dir-swap.
//!
//! ## A conflict never reaches an agent folder
//!
//! **A folder an agent reads always holds one coherent, complete bundle.** So a conflict writes
//! NOTHING to any placement: every managed folder keeps the author's own version, byte for byte, exactly
//! as it stood before the update. The marked-up tree (diff3 markers, and the `.topos-mine` siblings a
//! binary / add-add / oversize conflict keeps) is committed to the sidecar store as `result_commit` and
//! written ONCE, into the scope's own workbench — `~/.topos/conflicts/<name>/`, or a project store's
//! `<project>/.topos/state/<user>/conflicts/<name>/` ([`crate::sidecar::Layout::conflict_copy_dir`]).
//! Nothing under a store root is a skills dir for any harness, and `publish` ships a PLACEMENT's bytes,
//! so a marker (or a `.topos-mine` sibling) can be neither loaded by an agent nor published. What the
//! conflict DOES advance is the durable docs — `lock`/`sync` base ⇒ theirs, `applied` ⇒ `observed` — plus
//! the durable [`ConflictState`] (`conflict.json`) that is both the publish-block fact and the recovery
//! journal. The placement map is left untouched: nothing was written, so nothing is recorded as written.
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
//! The conflict path's order is: commit `M` (the result tree) + fsync → write `conflict.json` → write
//! the conflict copy → commit the docs. No placement is touched at any point, so no crash window can
//! leave an agent folder holding markers or a half-state — the strongest form of the guarantee, since
//! the dangerous bytes never enter the namespace an agent reads. The copy itself is built in a staging
//! sibling and RENAMED into place, so `conflicts/<name>/` is either absent or complete;
//! [`recover_resolution`] re-renders it only when it is ABSENT (an edited copy is never clobbered) and
//! re-commits the docs when they lag. Every resolution ([`escape`], a reset, a clean re-merge) clears
//! `conflict.json` FIRST and the copy after — never the reverse, because an absent copy reads as
//! "untouched", and a re-run must not commit the original draft over a hand resolution the crashed run
//! already committed. The residual that ordering leaves is one unreferenced folder under `conflicts/`
//! after a crash between the two removals: litter, never loss, and the next conflict for that bundle
//! simply takes the next rung of the naming ladder.

use std::collections::{BTreeMap, BTreeSet};

use topos_core::digest::{self, FileMode, ManifestEntry, to_hex};
use topos_core::identity::{self, Commit};
use topos_core::merge::{
    ContentMergeResult, FileId, FileSetConflictKind, MergeOutcome, PathPlan, decide_outcome,
    plan_merge,
};
use topos_gitstore::{
    DiffFile, ImportFile, MergeFileResult, RenderedBundle, RenderedFile, Store, merge_file,
    unified_diff,
};
use topos_types::persisted::{
    ConflictPath, ConflictPathKind, ConflictReason, ConflictState, Lock, PlacementMap, SyncState,
};
use topos_types::results::{
    ConflictPathReport, MergePreview, MergePreviewVerdict, MergeReport, PullAction, PullSkill,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::materialize::{self, MaterializeReq};
use crate::placement;
use crate::scan::ScannedBundle;
use crate::sidecar::SkillPaths;
use crate::{doc, logfile};

use super::sync_engine::{
    DivergedWitness, forwarded_sync, fsync_batch, lock_from_bundle, next_map, snapshot_draft,
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
            // Clear any (defensively) stale conflict record + its copy, then place the merged
            // draft-on-current. BEFORE the place, not after: a record left standing across the
            // swap would describe a divergence this merge just resolved, and the next sweep would
            // re-disclose it (and re-render its copy) over state that has moved on.
            clear_conflict(ctx, sp, lock)?;
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
            // The journal is written + fsynced BEFORE the copy, so a crash mid-write is recoverable.
            let cs = ConflictState {
                schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
                base_commit: to_hex(&base_commit),
                base_digest: to_hex(&base.bundle_digest),
                current_commit: to_hex(&theirs_commit),
                current_digest: to_hex(&theirs.bundle_digest),
                draft_commit: to_hex(&draft_commit),
                draft_digest: to_hex(&mine.bundle_digest),
                result_commit: to_hex(&result_commit),
                conflicted_digest: merged_digest_hex.clone(),
                copy_dir: Some(choose_conflict_dir(ctx, skill_id, lock)),
                reason: ConflictReason::ThreeWay,
                paths: assembled.conflicts.clone(),
            };
            doc::write_doc(ctx.fs, &sp.conflict, &cs)?;
            // The markers land HERE and nowhere else — every placement keeps the author's own
            // version — and the docs then move to draft-on-current without a single placement write.
            write_conflict_copy(ctx, &conflict_copy_path(ctx, lock, Some(&cs)), &merged)?;
            settle_conflict_docs(
                ctx,
                sp,
                sync,
                lock,
                map,
                theirs_commit,
                theirs,
                &cs.draft_digest,
            )?;
            log_resolution(ctx, skill_id, "merge-conflict", result_commit);
            Ok(conflicted_row(
                ctx,
                &lock.name,
                sync,
                base_commit,
                theirs_commit,
                result_commit,
                &merged_digest_hex,
                conflict_reports(&assembled.conflicts),
                None,
                conflict_disclosure(ctx, lock, map, &cs),
            ))
        }
        // `decide_outcome` never returns NoBaseTwoWay — that branch is taken before planning (above).
        MergeOutcome::NoBaseTwoWay => Err(ClientError::Corrupt(
            "decide_outcome returned NoBaseTwoWay".into(),
        )),
    }
}

/// The escape: commit the author's chosen bytes (`committed`) on top of `current` (a fresh 1-parent commit
/// — which also snapshots them recoverably), place it draft-on-current, and clear any conflict record
/// (and the marked-up copy it named). Always produces a clean, publishable candidate (no deadlock), and
/// needs no renderable base. The CALLER chooses `committed`: a fresh escape commits the placement (mine);
/// a recorded-conflict escape commits the author's hand resolution from the conflict copy, or — if that
/// copy is still the raw conflict tree — their ORIGINAL draft, so the escape never commits unresolved
/// markers as a publishable bundle.
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
    // The escape RESOLVES — clear the block last, once the placements have settled (idempotent: a
    // crash before this is healed by re-running).
    clear_conflict(ctx, sp, lock)?;
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
        schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
        base_commit: to_hex(&base_commit),
        base_digest: to_hex(&base_digest),
        current_commit: to_hex(&theirs_commit),
        current_digest: to_hex(&theirs.bundle_digest),
        draft_commit: to_hex(&result_commit),
        draft_digest: merged_digest_hex.clone(),
        result_commit: to_hex(&result_commit),
        conflicted_digest: merged_digest_hex.clone(),
        copy_dir: Some(choose_conflict_dir(ctx, skill_id, lock)),
        reason: ConflictReason::NoBase,
        paths: Vec::new(),
    };
    doc::write_doc(ctx.fs, &sp.conflict, &cs)?;
    // The same shape as a three-way conflict, so ONE set of exits serves both: the copy is MINE
    // here (there is no base to mark up against), the placements are not touched at all, and the
    // docs move to draft-on-current.
    write_conflict_copy(ctx, &conflict_copy_path(ctx, lock, Some(&cs)), &merged)?;
    settle_conflict_docs(
        ctx,
        sp,
        sync,
        lock,
        map,
        theirs_commit,
        theirs,
        &merged_digest_hex,
    )?;
    log_resolution(ctx, skill_id, "merge-no-base", result_commit);
    Ok(conflicted_row(
        ctx,
        &lock.name,
        sync,
        base_commit,
        theirs_commit,
        result_commit,
        &merged_digest_hex,
        Vec::new(),
        Some(drop_diff(theirs, &merged)),
        conflict_disclosure(ctx, lock, map, &cs),
    ))
}

/// Escape a skill that already holds a RECORDED conflict (state ③ draft-on-current + `conflict.json`): a
/// conflict consumed `theirs` into the (blocked) draft, so `applied == observed` and the normal DIVERGED
/// apply-arm is no longer reached — the escape resolves it here, committing MINE on the conflict's
/// `current`. Reachable only with a [`DivergedWitness`] (`conflict.json` ⟹ an author divergence).
///
/// The bytes come from the CONFLICT COPY, never from a placement (the placements were never written —
/// they still hold the author's own version):
///
/// - the copy still scans to `conflicted_digest` — the raw, unedited marker tree — or it is gone or
///   unreadable ⇒ commit the author's ORIGINAL draft (`draft_commit`). This is the "keep my version"
///   exit: leave the folder alone and the merge is dropped, never the markers published.
/// - the copy has been edited ⇒ those bytes ARE the hand resolution; commit them.
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
    let store = Store::open(&sp.store)?;
    let theirs_commit = super::parse_hex32(&cs.current_commit)?;
    let theirs = store.render_verified(theirs_commit, super::parse_hex32(&cs.current_digest)?)?;
    // A high-stakes commit ships bytes, so the copy is FULL-scanned (never a cache-derived digest);
    // an absent or unreadable folder falls to the original draft — the author's own bytes, already
    // recorded, so the fallback can never lose work.
    let hand_resolved = match crate::scan::scan(&conflict_copy_path(ctx, lock, Some(cs))) {
        Ok(scanned) if to_hex(&scanned.bundle_digest) != cs.conflicted_digest => Some(scanned),
        Ok(_) | Err(_) => None,
    };
    let committed = match &hand_resolved {
        Some(scanned) => scanned_to_bundle(scanned)?,
        None => store.render_verified(
            super::parse_hex32(&cs.draft_commit)?,
            super::parse_hex32(&cs.draft_digest)?,
        )?,
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

/// Recover a conflict recording interrupted by a crash. A conflict writes NO placement, so there is
/// nothing to heal in any agent folder — what can lag is the marked-up copy (written after
/// `conflict.json`) and the durable docs (written after the copy). Both are finished here,
/// idempotently: the copy is re-rendered from the recorded `result_commit` ONLY when it is absent (an
/// author's hand resolution is never clobbered), and the docs are re-committed only when they still
/// lag the recorded conflict.
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

    let copy = conflict_copy_path(ctx, lock, Some(cs));
    if !ctx.fs.exists(&copy) {
        write_conflict_copy(ctx, &copy, &result)?;
    }
    settle_conflict_docs(
        ctx,
        sp,
        sync,
        lock,
        map,
        theirs_commit,
        &theirs,
        &cs.draft_digest,
    )
}

// --------------------------------------------------------------------------------------------------
// The in-memory merge PREVIEW (no witness, no writes).
// --------------------------------------------------------------------------------------------------

/// Predict the outcome of the three-way merge `(base, mine, theirs)` implies — the SAME kernel plan
/// ([`plan_merge`]) + per-file diff3 ([`merge_file`]) the real resolution runs, executed purely in
/// memory: nothing is committed, nothing is placed, no store or placement is written. That is why it
/// needs no [`DivergedWitness`] — the witness gates RESOLUTION (which mutates); a prediction over
/// bytes the caller already holds mutates nothing. Callers must hand it ALREADY-LOCAL bytes only
/// (the describe surfaces' no-new-network-calls constraint lives at the call sites).
pub(crate) fn preview_merge(
    base: &RenderedBundle,
    mine: &ScannedBundle,
    theirs: &RenderedBundle,
) -> MergePreview {
    let plan = plan_merge(&file_ids(base), &scanned_file_ids(mine), &file_ids(theirs));
    let base_map = render_map(base);
    let mine_map = scanned_map(mine);
    let theirs_map = render_map(theirs);

    let mut conflicts: Vec<String> = Vec::new();
    let mut content_results = Vec::new();
    for pp in &plan.paths {
        match &pp.plan {
            PathPlan::ContentMerge { .. } => {
                // Absent sides / failed merges classify CONFLICTED (fail closed) — the preview must
                // never predict cleaner than the real run would be.
                let verdict = match (
                    base_map.get(pp.path.as_str()),
                    mine_map.get(pp.path.as_str()),
                    theirs_map.get(pp.path.as_str()),
                ) {
                    (Some(b), Some(m), Some(t)) => match merge_file(b.bytes, m.bytes, t.bytes) {
                        Ok(MergeFileResult::Clean(_)) => ContentMergeResult::Clean,
                        Ok(MergeFileResult::Conflict(_) | MergeFileResult::Binary) | Err(_) => {
                            ContentMergeResult::Conflicted
                        }
                    },
                    _ => ContentMergeResult::Conflicted,
                };
                if verdict == ContentMergeResult::Conflicted {
                    conflicts.push(pp.path.clone());
                }
                content_results.push(verdict);
            }
            PathPlan::FileSetConflict { .. } => conflicts.push(pp.path.clone()),
            _ => {}
        }
    }
    let verdict = match decide_outcome(&plan, &content_results) {
        MergeOutcome::CleanCommitOnTip => MergePreviewVerdict::Clean,
        // BlockedConflict also covers the emptying-merge edge (a clean plan whose tree would be
        // empty) — conflicted with no named path, honestly.
        MergeOutcome::BlockedConflict | MergeOutcome::NoBaseTwoWay => {
            MergePreviewVerdict::Conflicted
        }
    };
    MergePreview { verdict, conflicts }
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
/// Reuses the crash-safe dir-swap; the auto-update/harness hook is NOT fired (materialize only writes bytes).
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
    // The resolution converges EVERY managed placement onto the merged tree (the draft copy included
    // — its bytes are already committed in the store as a merge parent / the recoverable draft).
    let plan = placement::plan_for_skill(ctx, skill_id, lock, map);
    let map = placement::reconcile_map(map, &plan);
    let managed = placement::managed_indices(&map, &plan);
    materialize::materialize(
        ctx.fs,
        &MaterializeReq {
            skill_id,
            target_indices: &managed,
            bundle: merged,
            next_map: next_map(&map, theirs_commit, &merged_digest_hex),
            next_lock: &next_lock,
            next_sync: &next_sync,
            sp,
            snapshot: Some(&|s: &ScannedBundle| snapshot_draft(ctx, sp, lock, s).map(|_| ())),
            takeover: None,
            self_ignore: ctx.layout.is_project_scope(),
            expected: None,
            project_root: ctx.layout.project_root(),
        },
    )?;
    Ok(())
}

// --------------------------------------------------------------------------------------------------
// The conflict copy — the ONE place a marked-up tree is ever written.
// --------------------------------------------------------------------------------------------------

/// The `conflicts/` component this bundle's marked-up copy takes, chosen ONCE when the conflict is
/// recorded and stored in `conflict.json`. The bundle's NAME leads (a person has to open it), and a
/// name another live conflict already holds climbs the SAME ladder a skill dir climbs
/// ([`topos_harness::choose_skill_dir`]): `<name>-<workspace>`, counting up, then the unique id.
///
/// A folder left behind by a crash between the two removals a resolution makes (see the module doc)
/// counts as taken here — so the next conflict for that bundle names the next rung rather than
/// adopting a stale tree. That is the right direction: a wrong-but-fresh folder, never a shared one.
fn choose_conflict_dir(ctx: &Ctx<'_>, skill_id: &str, lock: &Lock) -> String {
    let ws = super::followed_workspace(ctx, skill_id);
    let slug = placement::workspace_slug(ctx, ws.as_deref());
    let chosen = topos_harness::choose_skill_dir(
        &ctx.layout.conflicts_dir(),
        skill_id,
        topos_harness::PlacementNaming {
            name: Some(&lock.name),
            workspace_slug: slug.as_deref(),
        },
        &topos_harness::dir_taken,
        &|_: &std::path::Path| false,
    );
    chosen
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(skill_id)
        .to_owned()
}

/// Where a recorded conflict's marked-up copy lives. The recorded component is UNTRUSTED input — a
/// project store travels with its checkout, so a hostile clone can commit a `conflict.json` naming
/// anything — and is used only when it is one plainly safe path component (ASCII alphanumerics, `-`,
/// `_`, no leading dot, so it can be neither `.`/`..` nor carry a separator). Anything else, and a
/// record written before the copy moved out of the placements, falls back to the bundle's sanitized
/// name; a name nothing safe survives falls to the validated id.
fn conflict_copy_path(
    ctx: &Ctx<'_>,
    lock: &Lock,
    cs: Option<&ConflictState>,
) -> std::path::PathBuf {
    let recorded = cs
        .and_then(|c| c.copy_dir.clone())
        .filter(|d| safe_component(d));
    let dir = recorded.unwrap_or_else(|| {
        topos_harness::sanitize_skill_dir(&lock.name).unwrap_or_else(|| lock.skill_id.clone())
    });
    ctx.layout.conflict_copy_dir(&dir)
}

/// Whether `s` is one plainly safe path component (see [`conflict_copy_path`]). Deliberately
/// stricter than "not `..`": every component this module MINTS is alphanumerics plus `-`/`_`, so
/// nothing legitimate is refused, and no length-capping re-sanitize can silently name a different
/// folder than the one that was written.
fn safe_component(s: &str) -> bool {
    !s.is_empty()
        && !s.starts_with('.')
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Write the marked-up tree into the scope's conflict workbench — the ONE write of marker bytes
/// anywhere, and never into a folder an agent reads.
///
/// Built in a staging sibling and RENAMED into place, so `conflicts/<name>/` is only ever ABSENT or
/// COMPLETE — which is what lets [`recover_resolution`] read "present" as "the author's to edit" and
/// [`escape_recorded`] read "absent" as "untouched". Every create/write descends from a held handle
/// on the store's own `conflicts/` dir (no path below it is re-resolved), so a `conflicts` component
/// swapped for a symlink inside a checkout is met as itself and REFUSED, never followed.
///
/// The target is expected FREE at both call sites — [`choose_conflict_dir`] runs the ladder's own
/// taken-probe when the conflict is recorded, and recovery writes only when the folder is absent — so
/// the removal before the rename is defensive, closing the beat between the choice and the write
/// rather than deleting anything a caller knows about. A leftover staging tree is likewise dropped:
/// it is topos's own partial copy of `result_commit`, reproducible byte for byte from the store.
fn write_conflict_copy(
    ctx: &Ctx<'_>,
    dir: &std::path::Path,
    bundle: &RenderedBundle,
) -> Result<(), ClientError> {
    let conflicts = ctx.layout.conflicts_dir();
    let leaf = dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| ClientError::Corrupt("conflict copy has no usable name".into()))?
        .to_owned();
    let staging_leaf = format!(".topos-staging-{leaf}");
    let handle = ctx.fs.create_dir_nofollow(ctx.layout.home(), &conflicts)?;
    ctx.fs.remove_dir_all_at(&handle, &staging_leaf)?;
    let staging = conflicts.join(&staging_leaf);
    materialize::build_staging(ctx.fs, &handle, &staging, bundle, false)?;
    ctx.fs.remove_dir_all_at(&handle, &leaf)?;
    ctx.fs.rename_at(&handle, &staging_leaf, &leaf)?;
    ctx.fs.fsync_dir(&conflicts)?;
    Ok(())
}

/// Clear a recorded conflict — the ONE exit every resolution (the escape, a reset, a clean re-merge)
/// takes. Idempotent, and ORDERED: `conflict.json` goes first, the marked-up copy after, never the
/// reverse. An absent copy reads as "untouched", so a crash between a copy-first removal and the
/// record's would let a re-run of the escape commit the ORIGINAL DRAFT over a hand resolution the
/// crashed run had already committed. The other order's residual is one unreferenced folder.
///
/// An unreadable record still clears: the file is removed either way, and the copy is looked for
/// under the name the bundle would have taken.
///
/// # Errors
/// The [`crate::fs_seam::FsOps`] failure removing the record or the copy.
pub(crate) fn clear_conflict(
    ctx: &Ctx<'_>,
    sp: &SkillPaths,
    lock: &Lock,
) -> Result<(), ClientError> {
    let recorded: Option<ConflictState> = doc::read_doc(ctx.fs, &sp.conflict).ok().flatten();
    ctx.fs.remove_file(&sp.conflict)?;
    let copy = conflict_copy_path(ctx, lock, recorded.as_ref());
    if ctx.fs.exists(&copy) {
        ctx.fs.remove_dir_all(&copy)?;
    }
    Ok(())
}

/// Advance the durable docs to the conflict's draft-on-current WITHOUT touching a placement:
/// `lock` + `sync.base_commit` ⇒ theirs (so the untouched working bytes read as a draft on the
/// team's version — which is exactly what makes `--reset` mean "take the team's version"),
/// `applied` ⇒ `observed` (theirs is consumed into the blocked draft, so the sweep never re-merges),
/// `work_hash` ⇒ the author's own bytes, which is what the folders actually hold.
///
/// The MAP is written back unchanged: nothing was materialized, so nothing is recorded as
/// materialized, and each placement's `materialized_sha` keeps naming the last bytes topos really
/// wrote there — the baseline the never-a-lost-byte rail compares against on the next overwrite.
///
/// A no-op when the docs already say this (recovery's re-entry), so a blocked bundle does not
/// re-write three documents on every sweep.
#[allow(clippy::too_many_arguments)]
fn settle_conflict_docs(
    ctx: &Ctx<'_>,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    theirs_commit: [u8; 32],
    theirs: &RenderedBundle,
    work_hash_hex: &str,
) -> Result<(), ClientError> {
    let next_lock = lock_from_bundle(lock, theirs_commit, theirs);
    let next_sync = forwarded_sync(sync, theirs_commit, work_hash_hex);
    if lock.base_commit == next_lock.base_commit
        && lock.bundle_digest == next_lock.bundle_digest
        && sync.base_commit == next_sync.base_commit
        && sync.applied == next_sync.applied
        && sync.work_hash == next_sync.work_hash
    {
        return Ok(());
    }
    materialize::commit_docs(ctx.fs, sp, map, &next_lock, &next_sync)
}

/// Commit an assembled tree as a forward 1-parent commit on `parent`, returning its `version_id`.
fn commit_result(
    ctx: &Ctx<'_>,
    store: &Store,
    parent: [u8; 32],
    bundle: &RenderedBundle,
    message: &str,
) -> Result<[u8; 32], ClientError> {
    let id = identity::commit_id(&Commit {
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
    // The result's own objects + ref — durable before any doc names it; never the whole store.
    fsync_batch(ctx, &store.version_durability(&id)?)?;
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
        // Stamped by the pull aggregator (`pull.rs`), which owns the follow-state.
        workspace_id: None,
        observed: sync.observed,
        applied: sync.observed, // the pending update is consumed into the merged draft
        action: PullAction::Merged,
        merge: Some(MergeReport {
            base_version_id: to_hex(&base),
            theirs_version_id: to_hex(&theirs),
            result_version_id: to_hex(&result),
            result_digest: result_digest_hex.to_owned(),
            clean: true,
            conflicts: Vec::new(),
            drop_diff,
            // A clean merge REWRITES its placements, so it has neither an untouched-folder set nor
            // a marked-up copy to name.
            placements: Vec::new(),
            copy_dir: None,
        }),
        synced_placements: None,
        destinations: Vec::new(),
        kept: Vec::new(),
        display: None,
        note: None,
        scope: None,
        harnesses: Vec::new(),
        kind: None,
    }
}

/// The two facts a conflict row states about disk, read where they are true: the folders that still
/// hold the author's own version (the recorded map — a conflict wrote to none of them), and the one
/// folder the marked-up copy went to (the record's own `copy_dir`, never recomputed, so the receipt
/// names the folder every exit reads).
fn conflict_disclosure(
    ctx: &Ctx<'_>,
    lock: &Lock,
    map: &PlacementMap,
    cs: &ConflictState,
) -> (Vec<String>, Option<String>) {
    let placements = map
        .placements
        .iter()
        .map(|p| super::inventory::pretty(ctx, std::path::Path::new(p)))
        .collect();
    let copy = conflict_copy_path(ctx, lock, Some(cs));
    (placements, Some(super::inventory::pretty(ctx, &copy)))
}

/// The SCOPE a row's next commands are spelled for: `"person"` for the machine's own store (whose
/// verbs take `-g`), the checkout's path for a project store. The manifest reconcile re-stamps this
/// with its own scope label; a TARGETED run has none, and a conflict row's two exits must be
/// runnable exactly as printed either way.
fn scope_label(ctx: &Ctx<'_>) -> String {
    ctx.layout
        .project_root()
        .map_or_else(|| "person".to_owned(), |d| d.display().to_string())
}

#[allow(clippy::too_many_arguments)]
fn conflicted_row(
    ctx: &Ctx<'_>,
    name: &str,
    sync: &SyncState,
    base: [u8; 32],
    theirs: [u8; 32],
    result: [u8; 32],
    result_digest_hex: &str,
    conflicts: Vec<ConflictPathReport>,
    drop_diff: Option<String>,
    disclosure: (Vec<String>, Option<String>),
) -> PullSkill {
    let (placements, copy_dir) = disclosure;
    PullSkill {
        skill: name.to_owned(),
        workspace_id: None,
        observed: sync.observed,
        applied: sync.observed, // theirs is incorporated into the (blocked) conflict draft
        action: PullAction::Conflicted,
        merge: Some(MergeReport {
            base_version_id: to_hex(&base),
            theirs_version_id: to_hex(&theirs),
            result_version_id: to_hex(&result),
            result_digest: result_digest_hex.to_owned(),
            clean: false,
            conflicts,
            drop_diff,
            placements,
            copy_dir,
        }),
        synced_placements: None,
        destinations: Vec::new(),
        kept: Vec::new(),
        display: None,
        note: None,
        scope: Some(scope_label(ctx)),
        harnesses: Vec::new(),
        kind: None,
    }
}

/// Build the typed conflict row from a recorded [`ConflictState`] (re-disclosed each pull while blocked).
pub(crate) fn conflicted_row_from_state(
    ctx: &Ctx<'_>,
    name: &str,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    cs: &ConflictState,
) -> Result<PullSkill, ClientError> {
    Ok(conflicted_row(
        ctx,
        name,
        sync,
        super::parse_hex32(&cs.base_commit)?,
        super::parse_hex32(&cs.current_commit)?,
        super::parse_hex32(&cs.result_commit)?,
        &cs.conflicted_digest,
        conflict_reports(&cs.paths),
        None,
        conflict_disclosure(ctx, lock, map, cs),
    ))
}
