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
//! as it stood before the update. The marked-up tree (diff3 markers, and the sidecar siblings a
//! binary / add-add / oversize conflict keeps) is committed to the sidecar store as `result_commit` and
//! written ONCE, into the scope's own workbench — `~/.topos/conflicts/<name>/`, or a project store's
//! `<project>/.topos/state/<user>/conflicts/<name>/` ([`crate::sidecar::Layout::conflict_copy_dir`]).
//! Nothing under a store root is a skills dir for any harness, and `publish` ships a PLACEMENT's bytes,
//! so a marker (or a sidecar sibling) can be neither loaded by an agent nor published. What the
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
//! simply takes the next rung of the naming ladder. The reverse residual — a resolution's placements
//! landed but its record outlived them — is recognized from the two durable documents
//! ([`resolution_landed`]) and finishes the clear rather than re-blocking a resolved bundle.
//!
//! ## The workbench folder is named by parsing, never by trusting
//!
//! `conflicts/<leaf>/` is the one place in this module where a string off disk becomes a path, and
//! every candidate for it is UNTRUSTED: a project store travels with its checkout, so a hostile
//! clone commits both the `conflict.json` that records the leaf and the `lock.json` whose `name`
//! and `skill_id` the fallbacks read. So the leaf is a validated [`ConflictDir`]
//! ([`conflict_copy_leaf`] is the one derivation) — one plainly safe component, never `..`, never a
//! separator, never a dot-name — and both the WRITE and the REMOVAL run at a held handle on
//! `conflicts/` itself, by that leaf ([`write_conflict_copy`] / [`remove_conflict_copy`]). Nothing
//! below `conflicts/` is ever re-resolved as a path, so neither a `conflicts` component swapped for
//! a symlink nor a recorded leaf climbing out of it can aim a write — or a recursive delete —
//! outside the store.

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
    ConflictPath, ConflictPathKind, ConflictReason, ConflictState, Lock, PlacementMap, Superseded,
    SyncState,
};
use topos_types::results::{
    ConflictPathReport, MergePreview, MergePreviewVerdict, MergeReport, PullAction, PullSkill,
};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::materialize::{self, MaterializeReq};
use crate::placement;
use crate::scan::ScannedBundle;
use crate::sidecar::{CONFLICT_STAGING_PREFIX, ConflictDir, SkillPaths};
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
const MINE_SUFFIX: &str = ".topos-mine";
/// The suffix for the team's side, written beside the author's own in the no-base workbench.
const THEIRS_SUFFIX: &str = ".topos-theirs";
/// Both suffixes topos itself mints inside a workbench — compare-and-resolve aids, never content
/// the author wrote, and never publishable bundle content.
const TOPOS_SUFFIXES: [&str; 2] = [MINE_SUFFIX, THEIRS_SUFFIX];

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
) -> Result<PullSkill, ClientError> {
    // The no-base fallback re-parents MINE onto `current`, so it needs no renderable base (the base is
    // exactly what is gone there); only the 3-way merge does. So render the base FIRST and branch —
    // snapshotting the draft on its base only on the path that has one.
    let store = Store::open(&sp.store)?;
    let base_commit = super::parse_hex32(&lock.base_commit)?;
    let base_digest = super::parse_hex32(&lock.bundle_digest)?;

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
                // CARRIED, not cleared: this merge landed the team's NEWEST version, but a draft
                // that already dropped an older one still drops it, and the row that says so
                // (and the publish that must disclose it) reads this field.
                sync.superseded.clone(),
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
                None,
            ))
        }
        MergeOutcome::BlockedConflict => {
            // The workbench folder is named FIRST: a conflict whose marked-up copy could not be
            // named anywhere safe refuses rather than recording a block with no resolution surface.
            let copy_leaf = choose_conflict_dir(ctx, skill_id, lock).ok_or_else(|| {
                ClientError::Corrupt(
                    "this bundle's conflict workbench folder cannot be named safely".into(),
                )
            })?;
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
                copy_dir: Some(copy_leaf.as_str().to_owned()),
                reason: ConflictReason::ThreeWay,
                paths: assembled.conflicts.clone(),
            };
            doc::write_doc(ctx.fs, &sp.conflict, &cs)?;
            // The markers land HERE and nowhere else — every placement keeps the author's own
            // version — and the docs then move to draft-on-current without a single placement write.
            write_conflict_copy(ctx, &copy_leaf, &merged)?;
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

/// The escape (`--keep-mine`): commit the author's chosen bytes (`committed`) as a fresh 1-parent commit
/// on `current` (which snapshots them recoverably), write them to every managed placement, and clear the
/// conflict record (and the marked-up copy it named). The CALLER chooses `committed`: the author's hand
/// resolution from the conflict copy, or — if that copy is still the raw conflict tree — their ORIGINAL
/// draft, so the escape never commits unresolved markers as bundle content.
///
/// ## One ordinary commit, on top of theirs
///
/// The escape's commit is `parents = [theirs], tree = mine` and the docs say exactly that: the recorded
/// base becomes the team's version ([`place_draft_on_current`]), `applied` catches up to `observed`, and
/// the working bytes read as an ordinary DRAFT on top of `current`. Nothing is rebased, nothing is
/// rewritten, and the version has exactly one parent — the history stays a list.
///
/// ## What is special is what the commit CONTAINS, so that is what is recorded
///
/// The one unusual thing about this draft is that its contents may quietly drop what a teammate wrote.
/// That is a fact about bytes, so it is COMPUTED rather than assumed: the merge is re-run in memory over
/// the CHOSEN tree ([`already_carries_theirs`]), and only when the answer is "no, theirs is not in
/// here" does [`SyncState::superseded`] record the version being dropped. A hand resolution that took
/// the team's contested line records nothing and publishes like any other draft.
///
/// The record is what makes the later publish announce itself instead of shipping in silence, and what
/// keeps `list`/`status` reading `draft` for a copy topos itself wrote into every folder.
///
/// The no-deadlock guarantee is unchanged: it needs no renderable base, it never touches the plane, and
/// it always leaves a coherent bundle in every folder.
#[allow(clippy::too_many_arguments)]
fn escape(
    ctx: &Ctx<'_>,
    skill_id: &str,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    committed: &RenderedBundle,
    base: Option<&RenderedBundle>,
    base_commit: [u8; 32],
    theirs: &RenderedBundle,
    theirs_commit: [u8; 32],
) -> Result<PullSkill, ClientError> {
    let store = Store::open(&sp.store)?;
    let merged_digest_hex = to_hex(&committed.bundle_digest);
    let result_commit = commit_result(ctx, &store, theirs_commit, committed, MERGE_ESCAPE_MESSAGE)?;
    let drop = drop_diff(theirs, committed);
    // The containment question, answered from bytes: does the chosen tree already hold the team's
    // version? An unrenderable base cannot merge, so it answers `false` by definition.
    let superseded = (!already_carries_theirs(base, committed, theirs)).then(|| Superseded {
        version_id: to_hex(&theirs_commit),
        bundle_digest: to_hex(&theirs.bundle_digest),
    });
    let supersedes_row = superseded.as_ref().map(|s| s.version_id.clone());

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
        superseded,
    )?;
    // The escape RESOLVES — clear the block last, once the placements have settled (idempotent: a
    // crash before this is healed by re-running).
    clear_conflict(ctx, sp, lock)?;
    log_resolution(ctx, skill_id, "merge-escape", result_commit);
    Ok(merged_row(
        &lock.name,
        sync,
        base_commit,
        theirs_commit,
        result_commit,
        &merged_digest_hex,
        Some(drop),
        supersedes_row,
    ))
}

/// Whether `committed` ALREADY contains everything `theirs` brought — the one question that decides
/// whether a `--keep-mine` resolution supersedes the team's version or merely reconciles with it.
///
/// It is answered by running the merge, not by inspecting the command: plan `(base, committed, theirs)`
/// with the SAME kernel the real resolution uses, assemble the tree, and ask two things of the result —
/// that it merged CLEANLY, and that it came out byte-identical to `committed`. Both together mean
/// "merging theirs in changes nothing here", which is exactly containment. Pure and in memory: no
/// commit, no placement, no store write, no network.
///
/// `None` base (the no-base fallback's unrenderable one) is `false` by definition — with no fork point
/// there is no merge to run, so nothing can be proven contained.
fn already_carries_theirs(
    base: Option<&RenderedBundle>,
    committed: &RenderedBundle,
    theirs: &RenderedBundle,
) -> bool {
    let Some(base) = base else { return false };
    let plan = plan_merge(&file_ids(base), &file_ids(committed), &file_ids(theirs));
    let base_map = render_map(base);
    let mine_map = render_map(committed);
    let theirs_map = render_map(theirs);
    let Ok(assembled) = assemble(&plan, &base_map, &mine_map, &theirs_map) else {
        return false;
    };
    if decide_outcome(&plan, &assembled.content_results) != MergeOutcome::CleanCommitOnTip {
        return false;
    }
    build_bundle(assembled.files).is_ok_and(|m| m.bundle_digest == committed.bundle_digest)
}

/// The no-base fallback: keep MINE on disk, block, and surface a 2-way diff of what theirs would add —
/// never a silent merge. The author resolves by editing or escaping.
///
/// ## The workbench holds BOTH sides here too
///
/// There is no fork point, so there is nothing to mark up — the diff3 markers a three-way conflict
/// writes have no meaning between unrelated histories. But a folder holding only one of the two
/// versions is not a place anyone can merge in: it invites a by-hand reconciliation while withholding
/// half of what would have to be reconciled. So the workbench is the author's own tree PLUS the team's,
/// each of theirs written beside its counterpart as a `<path>.topos-theirs` sibling — the same
/// mechanism a binary conflict already uses for `.topos-mine`, and stripped out by the same rule
/// ([`strip_topos_sidecars`]) so neither suffix can ever become published bundle content.
///
/// The two trees are committed SEPARATELY, and that separation is load-bearing: `result_commit`
/// (the workbench, with the siblings) is what recovery re-renders and what the untouched test
/// compares against, while `draft_commit` (the author's tree alone) is what a keep-my-version exit
/// commits. Folding them into one would publish topos's own comparison files.
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
    // Named FIRST, exactly as in the three-way arm: no block is recorded without a folder to
    // resolve it in.
    let copy_leaf = choose_conflict_dir(ctx, skill_id, lock).ok_or_else(|| {
        ClientError::Corrupt(
            "this bundle's conflict workbench folder cannot be named safely".into(),
        )
    })?;
    let merged = scanned_to_bundle(mine)?; // keep mine; never merge unrelated trees silently
    let merged_digest_hex = to_hex(&merged.bundle_digest);
    // `M` = mine re-parented on `current`; it snapshots mine recoverably (the base is unrenderable, so
    // a base-parented snapshot is impossible — M is the recoverable draft, and what a keep-my-version
    // exit commits).
    let draft_commit = commit_result(ctx, &store, theirs_commit, &merged, MERGE_NOBASE_MESSAGE)?;
    // The workbench tree: mine, with the team's files beside it. Committed separately, so the exit
    // above never ships these siblings.
    let workbench = with_theirs_siblings(&merged, theirs)?;
    let workbench_digest_hex = to_hex(&workbench.bundle_digest);
    let result_commit =
        commit_result(ctx, &store, theirs_commit, &workbench, MERGE_NOBASE_MESSAGE)?;
    let cs = ConflictState {
        schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
        base_commit: to_hex(&base_commit),
        base_digest: to_hex(&base_digest),
        current_commit: to_hex(&theirs_commit),
        current_digest: to_hex(&theirs.bundle_digest),
        draft_commit: to_hex(&draft_commit),
        draft_digest: merged_digest_hex.clone(),
        result_commit: to_hex(&result_commit),
        conflicted_digest: workbench_digest_hex,
        copy_dir: Some(copy_leaf.as_str().to_owned()),
        reason: ConflictReason::NoBase,
        paths: Vec::new(),
    };
    doc::write_doc(ctx.fs, &sp.conflict, &cs)?;
    // The same shape as a three-way conflict, so ONE set of exits serves both: the copy carries both
    // sides, the placements are not touched at all, and the docs move to draft-on-current.
    write_conflict_copy(ctx, &copy_leaf, &workbench)?;
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
/// they still hold the author's own version). The folder is read as exactly one of three things, and
/// the difference between the last two is the difference between an exit and a loss:
///
/// - **absent** (or unnameable) ⇒ commit the author's ORIGINAL draft (`draft_commit`). There is no
///   hand resolution on disk to lose, and this is what makes the removal ORDER in
///   [`clear_conflict`] safe (see the module doc).
/// - **present and scanning to `conflicted_digest`** — the raw, unedited marker tree ⇒ commit the
///   ORIGINAL draft too. This is the "keep my version" exit: leave the folder alone and the merge
///   is dropped, never the markers published.
/// - **present and edited** ⇒ those bytes ARE the hand resolution; commit them.
///
/// Whichever of the three it is, the result is committed on the team's version and the docs advance
/// with it — one ordinary commit on top of theirs. What the three differ in is only what is INSIDE
/// that commit, which is why [`escape`] measures the contents rather than trusting the command.
///
/// A folder that is present but CANNOT BE READ as a bundle is none of the three, and it is
/// refused. The scanner rejects a tree holding a symlink, a non-regular file, a non-UTF-8 name, or
/// nothing at all — every one of them a plausible state for a person mid-hand-merge — and this arm
/// used to fold that into "absent": it committed the original draft, wrote it over every
/// placement, and then deleted the folder, destroying a resolution that lives nowhere else (it is
/// outside the placement map, so the materializer's snapshot rail never sees it) while the row
/// said `Merged`. So an unreadable-but-present folder now refuses, naming the folder and both ways
/// out; nothing is committed, nothing is placed, and the block stands.
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
    let committed = match read_hand_resolution(ctx, lock, cs)? {
        Some(bundle) => bundle,
        None => store.render_verified(
            super::parse_hex32(&cs.draft_commit)?,
            super::parse_hex32(&cs.draft_digest)?,
        )?,
    };
    // Read the copies BEFORE the escape converges them, so the row can say what it is about to
    // overwrite (see [`collapsed_copies`]).
    let collapsed = collapsed_copies(ctx, sp, lock, map);
    // The conflict's own fork point — the base the containment check merges against. It is the ONE
    // thing a no-base record cannot supply, and its absence is exactly what makes that record's exit
    // count as superseding.
    let base_commit = super::parse_hex32(&cs.base_commit)?;
    let base = store
        .render_verified(base_commit, super::parse_hex32(&cs.base_digest)?)
        .ok();
    let mut row = escape(
        ctx,
        skill_id,
        sp,
        sync,
        lock,
        map,
        &committed,
        base.as_ref(),
        base_commit,
        &theirs,
        theirs_commit,
    )?;
    if let Some(saved) = collapsed {
        row.note = Some(collapsed_note(ctx, &lock.name, &saved));
    }
    Ok(row)
}

/// One copy this escape is about to overwrite, as the disclosure names it: the folder a person
/// reads, and the version its bytes were saved as.
struct SavedCopy {
    display: String,
    version: String,
}

/// The copies holding edits that DISAGREE, each snapshotted and named — `None` when there is at
/// most one edited copy (the ordinary case, which has nothing to disclose).
///
/// A recorded conflict enters [`super::sync_engine::sync_one`] BEFORE the work-tree
/// classification, so the typed competitor freeze never fires on this path. That is deliberate:
/// the freeze exists because nothing can tell which copy to act on, and the workbench folder
/// answers exactly that question — freezing here would deadlock the one exit that always works.
/// What the freeze also carried, though, was the DISCLOSURE, and that is still owed. The escape
/// writes its committed bytes over EVERY managed placement, so copies that disagreed are collapsed
/// into one; the materializer snapshots each before it overwrites, but a person who is never told
/// has no reason to look for the snapshot and no id to ask for it back with.
///
/// So each competing copy is snapshotted HERE — [`snapshot_draft`] is content-addressed and
/// idempotent, so this is the same commit the materializer's rail makes moments later, and the id
/// is simply learned early enough to print. Best-effort by construction: a scan or a snapshot that
/// fails says nothing rather than failing an escape that must never deadlock.
fn collapsed_copies(
    ctx: &Ctx<'_>,
    sp: &SkillPaths,
    lock: &Lock,
    map: &PlacementMap,
) -> Option<Vec<SavedCopy>> {
    let scans = placement::scan_placements(ctx, map).ok()?;
    let placement::DraftVerdict::Competitors(indices) = placement::classify_draft(&scans, map)
    else {
        return None;
    };
    let mut out = Vec::new();
    for s in scans.iter().filter(|s| indices.contains(&s.idx)) {
        let crate::placement::ScanStatus::Modified { scanned } = &s.status else {
            continue; // a competitor is a Modified copy by construction
        };
        let version = snapshot_draft(ctx, sp, lock, scanned).ok()?;
        out.push(SavedCopy {
            display: super::inventory::pretty(ctx, &s.dir),
            version,
        });
    }
    (out.len() > 1).then_some(out)
}

/// What the escape's receipt row says about the copies it collapsed: the count, then ONE runnable
/// line per copy that restores it, each naming the folder it came from. The go-back spelling is
/// scope-exact (the layout IS the scope), so every line runs as printed.
///
/// It says COPY and FOLDER, never bytes: the reader is being told about work of theirs that a
/// folder no longer holds, and the word for that is the one they would use themselves.
fn collapsed_note(ctx: &Ctx<'_>, name: &str, saved: &[SavedCopy]) -> String {
    let g = crate::error::scope_flag(!ctx.layout.is_project_scope());
    let mut out = format!(
        "overwrote different edits in {} folders — restore a copy:",
        saved.len()
    );
    for c in saved {
        out.push_str(&format!(
            "\n  topos update{g} {name}@{}   (was in {})",
            short_version(&c.version),
            c.display
        ));
    }
    out
}

/// A version id as a command line spells it — short enough to type, long enough to be unique in
/// one bundle's history (the same 12 the receipts print).
fn short_version(v: &str) -> &str {
    &v[..v.len().min(12)]
}

/// The author's hand resolution as a committable bundle, or `None` when the workbench folder is
/// absent/unnameable or still holds the raw marker tree (both of which mean "keep my version").
///
/// A high-stakes commit ships bytes, so the folder is FULL-scanned (never a cache-derived digest),
/// and the untouched comparison runs against that RAW scan — `conflicted_digest` covers the
/// the sidecar siblings too, so an untouched folder must include them to read as untouched.
///
/// # Errors
/// [`ClientError::ConflictCopyUnreadable`] when the folder is present but is not a readable bundle
/// (see [`escape_recorded`]), or when stripping topos's own scaffolding would leave nothing.
fn read_hand_resolution(
    ctx: &Ctx<'_>,
    lock: &Lock,
    cs: &ConflictState,
) -> Result<Option<RenderedBundle>, ClientError> {
    let Some(dir) = conflict_copy_path(ctx, lock, Some(cs)) else {
        return Ok(None);
    };
    let refuse = |reason: String| ClientError::ConflictCopyUnreadable {
        skill: lock.name.clone(),
        path: super::inventory::pretty(ctx, &dir),
        reason,
        global: !ctx.layout.is_project_scope(),
    };
    match ctx.fs.path_kind(&dir)? {
        // Absent: untouched, and nothing on disk to lose.
        None => return Ok(None),
        Some(crate::fs_seam::PathKind::Dir) => {}
        // A symlink or a file where the workbench belongs is not a bundle the escape may read —
        // and following it would commit (and then publish) bytes from wherever it points.
        Some(_) => return Err(refuse("it is not a directory".into())),
    }
    let scanned = crate::scan::scan(&dir).map_err(|e| refuse(unreadable_reason(&e)))?;
    if to_hex(&scanned.bundle_digest) == cs.conflicted_digest {
        return Ok(None); // the raw marker tree, untouched — keep my version
    }
    let files = strip_topos_sidecars(&scanned);
    if files.is_empty() {
        return Err(refuse(
            "it holds nothing but topos's own comparison files".into(),
        ));
    }
    Ok(Some(build_bundle(files)?))
}

/// Why a present workbench folder could not be read as a bundle, as one lowercase phrase — the
/// scanner's own reject text where it has one, so the refusal names the actual file.
fn unreadable_reason(e: &ClientError) -> String {
    match e {
        ClientError::EmptyBundle => "it holds no files".to_owned(),
        ClientError::Scan(reason) => reason.clone(),
        other => other.to_string(),
    }
}

/// Recover a conflict recording interrupted by a crash. A conflict writes NO placement, so there is
/// nothing to heal in any agent folder — what can lag is the marked-up copy (written after
/// `conflict.json`) and the durable docs (written after the copy). Both are finished here,
/// idempotently: the copy is re-rendered from the recorded `result_commit` ONLY when it is absent (an
/// author's hand resolution is never clobbered), and the docs are re-committed only when they still
/// lag the recorded conflict.
///
/// Reached only for a record that still describes a LIVE divergence — [`heal_landed_resolution`]
/// runs first and takes the leftover-record case away.
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

    // Re-render the workbench ONLY when it is genuinely absent — by `lstat`, so a symlink or a
    // file that appeared at the name reads as PRESENT and is left exactly where the author (or
    // whoever put it there) can see it; the escape refuses over it and names it.
    if let Some(leaf) = conflict_copy_leaf(lock, Some(cs))
        && ctx
            .fs
            .path_kind(&ctx.layout.conflict_copy_dir(&leaf))?
            .is_none()
    {
        write_conflict_copy(ctx, &leaf, &result)?;
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

/// Finish an exit whose RECORD outlived it, and say whether a block still stands.
///
/// Every exit ([`escape`], a reset) converges its placements FIRST and clears `conflict.json`
/// last, so a crash in that beat leaves a fully resolved bundle still carrying the record. Read
/// naively, that leftover is worse than litter in both directions:
///
/// - a sweep would re-settle the conflict's docs, putting `work_hash` back to the pre-exit draft
///   while the folders hold the resolution — a document naming bytes that exist nowhere — and
///   re-disclose a block on a bundle nothing is blocking;
/// - `--keep-mine` would see the (already removed) workbench folder as "untouched" and commit
///   the ORIGINAL DRAFT over the resolution the placements already hold — the exact loss the
///   record-before-copy removal order exists to prevent, arriving from the other side.
///
/// So the leftover is recognized and the interrupted clear is finished before anything reads the
/// record. Returns `true` when it did (no block stands; the caller pulls normally), `false` when
/// the record still describes a live divergence.
///
/// # Errors
/// The [`crate::fs_seam::FsOps`] failure clearing the record or the workbench folder.
pub(crate) fn heal_landed_resolution(
    ctx: &Ctx<'_>,
    sp: &SkillPaths,
    sync: &SyncState,
    lock: &Lock,
    map: &PlacementMap,
    cs: &ConflictState,
) -> Result<bool, ClientError> {
    if !resolution_landed(sync, map, cs) {
        return Ok(false);
    }
    clear_conflict(ctx, sp, lock)?;
    Ok(true)
}

/// Whether a resolution's PLACEMENT WRITE already landed for this recorded conflict — i.e. the
/// record is a leftover from a crash between an exit's materialize and its
/// [`clear_conflict`], not a live block.
///
/// The question is answered from the durable documents alone, by the ONE thing a live block cannot
/// say: that topos itself wrote what the folders hold, at the conflict's own `current`.
///
/// - `map.applied_commit` == the conflict's `current_commit` — the map names that version as the one
///   materialized here, AND
/// - `map.materialized_sha` == `sync.work_hash` — the bytes topos wrote ARE the bytes on disk.
///
/// While a block stands, the second half is false by construction: a conflict writes NO placement, so
/// the map keeps naming the last bytes topos really wrote while `work_hash` names the author's own
/// diverged draft. Every exit converges the placements and records what it wrote, which is what makes
/// the pair hold — so the pair means "an exit's placement write already landed" and nothing else.
fn resolution_landed(sync: &SyncState, map: &PlacementMap, cs: &ConflictState) -> bool {
    map.applied_commit == cs.current_commit && map.materialized_sha == sync.work_hash
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
    let side_path = sidecar_path(path, MINE_SUFFIX, union, emitted);
    emit(files, emitted, side_path, mine.mode, mine.bytes.to_vec());
}

/// The author's tree with the TEAM's files written beside it — the no-base workbench (see [`no_base`]).
/// Each of theirs lands at `<path>.topos-theirs`, disambiguated by the same rule the `.topos-mine`
/// siblings use, so a person can open one folder and see both versions of every file.
fn with_theirs_siblings(
    mine: &RenderedBundle,
    theirs: &RenderedBundle,
) -> Result<RenderedBundle, ClientError> {
    let union: BTreeSet<&str> = mine
        .files
        .iter()
        .chain(theirs.files.iter())
        .map(|f| f.path.as_str())
        .collect();
    let mut emitted: BTreeSet<String> = mine.files.iter().map(|f| f.path.clone()).collect();
    let mut files = mine.files.clone();
    for f in &theirs.files {
        let path = sidecar_path(&f.path, THEIRS_SUFFIX, &union, &emitted);
        emitted.insert(path.clone());
        files.push(RenderedFile {
            content_sha256: digest::sha256(&f.bytes),
            path,
            mode: f.mode,
            bytes: f.bytes.clone(),
        });
    }
    files.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
    build_bundle(files)
}

/// A sidecar path (of either suffix) that collides with neither a real bundle path nor an
/// already-emitted one — under the SAME equivalence the kernel digest enforces (exact / NFC / ASCII
/// case-fold), not just exact bytes. A byte-distinct-but-case-fold/NFC-colliding name would pass an
/// exact check yet make the assembled tree's `bundle_digest` reject, so we compare normalized forms
/// (`digest::normalize_for_collision`).
fn sidecar_path(
    path: &str,
    suffix: &str,
    union: &BTreeSet<&str>,
    emitted: &BTreeSet<String>,
) -> String {
    let taken: BTreeSet<String> = union
        .iter()
        .map(|p| digest::normalize_for_collision(p))
        .chain(emitted.iter().map(|p| digest::normalize_for_collision(p)))
        .collect();
    let base = format!("{path}{suffix}");
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

/// Whether `path` is a sidecar THIS module wrote into a workbench — the exact shape
/// [`sidecar_path`] mints, in either suffix: it at the end of the file's own name, optionally
/// followed by the disambiguating `-<n>` counter. Directory components are never examined, so a
/// real file living under a directory that happens to contain a suffix is untouched.
fn is_topos_sidecar(path: &str) -> bool {
    let leaf = path.rsplit('/').next().unwrap_or(path);
    TOPOS_SUFFIXES.iter().any(|suffix| {
        let Some(cut) = leaf.rfind(suffix) else {
            return false;
        };
        let tail = &leaf[cut + suffix.len()..];
        tail.is_empty()
            || (tail.starts_with('-')
                && tail.len() > 1
                && tail[1..].bytes().all(|b| b.is_ascii_digit()))
    })
}

/// A hand resolution's files with topos's OWN sidecar scaffolding stripped out.
///
/// A binary / add-add / oversize conflict keeps both sides on disk by writing the author's version
/// to a `<path>.topos-mine` sibling, and the no-base workbench writes the team's whole tree as
/// `<path>.topos-theirs` siblings — compare-and-resolve aids this module minted, never content
/// the author wrote. Those siblings stay out of the placements while the block stands, but the
/// escape COMMITS the folder, writes it to every placement, and `publish` ships a placement's
/// bytes — so left in, they would become published bundle content the moment the author edited
/// anything and escaped. They are stripped rather than refused: an author who resolved a binary
/// conflict the obvious way (fix the file, leave the aid alone) must not be sent back to delete
/// topos's own bookkeeping, and the escape's no-deadlock guarantee depends on it always producing
/// a publishable candidate. Keeping one is still possible — rename it to a name of your own.
fn strip_topos_sidecars(scanned: &ScannedBundle) -> Vec<RenderedFile> {
    scanned
        .files
        .iter()
        .filter(|f| !is_topos_sidecar(&f.path))
        .map(|f| RenderedFile {
            path: f.path.clone(),
            mode: f.mode,
            bytes: f.bytes.clone(),
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect()
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
///
/// `superseded` is the ONE thing the two callers disagree about, so it is a parameter rather than a
/// derivation: a clean merge CARRIES whatever stood (its result may still drop an older team version),
/// while the escape passes what it just measured. The forward apply/heal path — which lands the
/// pristine target everywhere and therefore drops nothing — clears it in [`forwarded_sync`] itself.
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
    superseded: Option<Superseded>,
) -> Result<(), ClientError> {
    let merged_digest_hex = to_hex(&merged.bundle_digest);
    let next_lock = lock_from_bundle(lock, theirs_commit, theirs);
    let next_sync = SyncState {
        superseded,
        ..forwarded_sync(sync, theirs_commit, &merged_digest_hex)
    };
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
fn choose_conflict_dir(ctx: &Ctx<'_>, skill_id: &str, lock: &Lock) -> Option<ConflictDir> {
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
    // Parsed at the MINT, by the same rule every later read applies — so a recorded component is
    // one this build will still accept, and no reader can be sent to a different folder than the
    // one that was written.
    chosen
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(ConflictDir::parse)
        .or_else(|| ConflictDir::parse(skill_id))
}

/// Which `conflicts/` component a recorded conflict's marked-up copy takes, as the VALIDATED
/// [`ConflictDir`] — the ONE derivation every read, write and removal of a workbench folder goes
/// through, so they can never disagree about which folder they mean.
///
/// EVERY candidate here is untrusted input, and every one is parsed rather than trusted: a project
/// store travels with its checkout, so a hostile clone commits the `conflict.json` whose `copy_dir`
/// leads AND the `lock.json` whose `name`/`skill_id` the fallbacks read. So the recorded component
/// is used only when it parses; then the bundle's sanitized name (already one safe component by
/// construction, re-parsed anyway so this function has exactly one exit); then the record's own
/// `skill_id`, which is a raw string on disk and is therefore parsed like the rest — the whole
/// point being that no unvalidated on-disk string reaches a path join.
///
/// `None` means the folder cannot be NAMED at all (a hostile record whose every candidate fails).
/// Callers read that as "there is no workbench folder": nothing is written, nothing is scanned, and
/// — this is the load-bearing part — nothing is removed.
fn conflict_copy_leaf(lock: &Lock, cs: Option<&ConflictState>) -> Option<ConflictDir> {
    cs.and_then(|c| c.copy_dir.as_deref())
        .and_then(ConflictDir::parse)
        .or_else(|| {
            topos_harness::sanitize_skill_dir(&lock.name)
                .as_deref()
                .and_then(ConflictDir::parse)
        })
        .or_else(|| ConflictDir::parse(&lock.skill_id))
}

/// Where a recorded conflict's marked-up copy lives — [`conflict_copy_leaf`] joined onto this
/// scope's `conflicts/`. For DISPLAY and for the scan; the write and the removal work at a held
/// handle on `conflicts/` and take the leaf itself, never this path.
fn conflict_copy_path(
    ctx: &Ctx<'_>,
    lock: &Lock,
    cs: Option<&ConflictState>,
) -> Option<std::path::PathBuf> {
    conflict_copy_leaf(lock, cs).map(|d| ctx.layout.conflict_copy_dir(&d))
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
    leaf: &ConflictDir,
    bundle: &RenderedBundle,
) -> Result<(), ClientError> {
    let conflicts = ctx.layout.conflicts_dir();
    let leaf = leaf.as_str();
    let staging_leaf = format!("{CONFLICT_STAGING_PREFIX}{leaf}");
    let handle = ctx.fs.create_dir_nofollow(ctx.layout.home(), &conflicts)?;
    ctx.fs.remove_dir_all_at(&handle, &staging_leaf)?;
    let staging = conflicts.join(&staging_leaf);
    materialize::build_staging(ctx.fs, &handle, &staging, bundle, false)?;
    ctx.fs.remove_dir_all_at(&handle, leaf)?;
    ctx.fs.rename_at(&handle, &staging_leaf, leaf)?;
    ctx.fs.fsync_dir(&conflicts)?;
    Ok(())
}

/// Remove ONE workbench folder — the mirror image of [`write_conflict_copy`], and the ONLY way a
/// workbench folder is ever deleted.
///
/// It descends the SAME way the write does: a HELD handle on this scope's own `conflicts/`
/// directory, then a single fd-anchored removal of one VALIDATED leaf inside it
/// ([`crate::fs_seam::FsOps::remove_dir_all_at`] — `openat` from the held fd, fd-anchored
/// iteration, `unlinkat`). No path below `conflicts/` is ever resolved, so nothing a record says
/// can aim this removal outside the store:
///
/// - the LEAF is a [`ConflictDir`], so it cannot be `..`, cannot carry a separator, and cannot hide
///   under a dot — it names an entry inside the held directory or nothing at all;
/// - the `conflicts` component itself is met as ITSELF: a symlink (or any non-directory) there is
///   refused rather than followed, so a checkout that ships `conflicts` pointing at a home
///   directory removes nothing. The write refuses the same swap through
///   [`crate::fs_seam::FsOps::create_dir_nofollow`]'s walk; before this the removal was a
///   path-based `remove_dir_all`, whose intermediate components the kernel resolved normally —
///   the two disagreed, and the delete was the one that could leave the sidecar.
///
/// An absent `conflicts/` is success (there is nothing to remove), and so is an absent leaf.
fn remove_conflict_copy(ctx: &Ctx<'_>, leaf: &ConflictDir) -> Result<(), ClientError> {
    let conflicts = ctx.layout.conflicts_dir();
    match ctx.fs.path_kind(&conflicts)? {
        None => return Ok(()),
        Some(crate::fs_seam::PathKind::Dir) => {}
        Some(_) => {
            return Err(ClientError::Io(format!(
                "{} is not a directory — refusing to remove anything through it",
                conflicts.display()
            )));
        }
    }
    let handle = ctx.fs.open_dir_handle(&conflicts)?;
    ctx.fs.remove_dir_all_at(&handle, leaf.as_str())?;
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
/// The copy's removal runs through [`remove_conflict_copy`] — anchored on a held handle over this
/// scope's own `conflicts/` directory, by ONE validated leaf. A record whose every candidate name
/// fails to parse names no folder, and no folder is removed.
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
    let Some(leaf) = conflict_copy_leaf(lock, recorded.as_ref()) else {
        return Ok(());
    };
    remove_conflict_copy(ctx, &leaf)
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
    // A conflict decides nothing, so it changes nothing about what this draft drops — whatever
    // an earlier `--keep-mine` recorded stands until an exit resolves it.
    let next_sync = SyncState {
        superseded: sync.superseded.clone(),
        ..forwarded_sync(sync, theirs_commit, work_hash_hex)
    };
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

/// [`drop_diff`] against a live SCAN rather than a stored tree — the publish describe's disclosure,
/// which diffs the version being replaced against the bytes actually sitting in the folder.
pub(crate) fn drop_diff_from_scan(theirs: &RenderedBundle, mine: &ScannedBundle) -> String {
    let t = diff_view(theirs);
    let mut m: Vec<DiffFile<'_>> = mine
        .files
        .iter()
        .map(|f| DiffFile {
            path: &f.path,
            mode: f.mode,
            bytes: &f.bytes,
        })
        .collect();
    m.sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
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
    supersedes: Option<String>,
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
            supersedes,
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
    let copy = conflict_copy_path(ctx, lock, Some(cs)).map(|p| super::inventory::pretty(ctx, &p));
    (placements, copy)
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
            // A block decides nothing, so it replaces nothing — the field belongs to the
            // resolution that follows it.
            supersedes: None,
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
