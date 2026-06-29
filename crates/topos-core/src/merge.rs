//! The pure author-merge POLICY: three-way file-set reconciliation + the outcome decision + the publish
//! guard. **Metadata only** — this module reconciles `(path, mode, content_sha256)` triples and emits a
//! *plan*; it never sees file bytes, runs no content merge, and touches no filesystem. The byte-level
//! three-way content merge (the `diffy` execution) lives in `topos-gitstore::merge`; the orchestration
//! (rendering bytes, running the content merges, assembling + materializing the tree) lives in the client.
//!
//! ## The split, and why it is here
//!
//! "diff3" has two halves: deciding *what happens to each path* (a total function over file metadata) and
//! *merging the bytes of a genuinely-conflicting file* (an LCS reconciliation). The first half is pure
//! policy with no library dependency, so it is the kernel's — every row of the reconciliation table is a
//! one-line truth-table test here, byte-stable across releases. The second half needs a diff engine and
//! real bytes, so it is the gitstore's.
//!
//! ## Determinism is consent
//!
//! The merged (or conflict-marked) bytes become a content-addressed, human-approved artifact, so the plan
//! must be byte-deterministic: [`plan_merge`] emits paths in raw-path-byte order (the same order
//! [`crate::digest::canonical_manifest`] uses), and every per-path verdict is a total function over the
//! `(presence, content_sha256, mode)` of the three sides — no clock, no RNG, no hashing-order dependence.
//!
//! ## Two file modes
//!
//! The manifest admits exactly two modes ([`FileMode::Regular`] / [`FileMode::Executable`]). With two
//! modes a *base-present* mode disagreement is impossible (if both sides changed the mode away from the
//! base they changed it to the *same* mode), so a content-merge's mode always resolves cleanly. The one
//! real mode disagreement is a *base-absent* add/add of identical content with different modes — there is
//! no base to anchor "who changed it", so the two sides genuinely disagree and it is a conflict.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use crate::digest::FileMode;

/// One file's identity for reconciliation: its bundle-relative path, mode, and content hash. The merge
/// policy compares only these — never the bytes themselves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileId {
    pub path: String,
    pub mode: FileMode,
    pub content_sha256: [u8; 32],
}

/// What the three-way reconciliation decided for one path. A `Take*` / [`PathPlan::ContentMerge`] carries
/// the resolved `(content_sha256, mode)` so the policy is fully testable without bytes; the client uses
/// the variant to pull the right side's bytes (and, for [`PathPlan::ContentMerge`], to run the byte merge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathPlan {
    /// Both present sides are byte- and mode-identical — take that content (no merge).
    TakeEither {
        content_sha256: [u8; 32],
        mode: FileMode,
    },
    /// Theirs equals base (they didn't touch it) — keep mine's change. Also a mine-only add.
    TakeMine {
        content_sha256: [u8; 32],
        mode: FileMode,
    },
    /// Mine equals base (I didn't touch it) — take theirs' change. Also a theirs-only add.
    TakeTheirs {
        content_sha256: [u8; 32],
        mode: FileMode,
    },
    /// A deletion both sides accept (one deleted, the other left base untouched, or both deleted) — the
    /// path is omitted from the resolved tree.
    Delete,
    /// All three sides differ in content — a genuine three-way content merge the client must run (via
    /// `topos-gitstore::merge::merge_file`). `mode` is the resolved consensus mode (always clean with two
    /// modes; see the module docs).
    ContentMerge {
        base: [u8; 32],
        mine: [u8; 32],
        theirs: [u8; 32],
        mode: FileMode,
    },
    /// A structural conflict with no single resolvable content — it has no natural merged bytes, so the
    /// client renders a deterministic representation (see the conflict-tree rules in the client) and the
    /// outcome is blocked.
    FileSetConflict { kind: FileSetConflictKind },
}

/// The kinds of structural (file-set) conflict — named **mine-then-theirs**.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileSetConflictKind {
    /// Both sides added the same path with **different content** (no base to merge against).
    AddAddDifferent,
    /// Mine modified the file; theirs deleted it.
    ModifyDelete,
    /// Mine deleted the file; theirs modified it.
    DeleteModify,
    /// Both sides added the same path with **identical content** but **different modes** (no base to
    /// anchor which side changed the mode — a genuine, consent-significant disagreement).
    AddAddModeDiffers,
}

/// One reconciled path + its verdict, in raw-path-byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedPath {
    pub path: String,
    pub plan: PathPlan,
}

/// The reconciliation plan over the union of the three file-sets, in raw-path-byte order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergePlan {
    pub paths: Vec<PlannedPath>,
}

impl MergePlan {
    /// The paths needing a byte-level content merge, in plan order — the client runs the gitstore merge
    /// for each and feeds the verdicts back to [`decide_outcome`] **in this same order**.
    pub fn content_merge_paths(&self) -> impl Iterator<Item = &PlannedPath> {
        self.paths
            .iter()
            .filter(|p| matches!(p.plan, PathPlan::ContentMerge { .. }))
    }

    /// Whether any path is a structural file-set conflict (decided without running a byte merge).
    #[must_use]
    pub fn has_structural_conflict(&self) -> bool {
        self.paths
            .iter()
            .any(|p| matches!(p.plan, PathPlan::FileSetConflict { .. }))
    }
}

/// One path's byte-merge verdict from the gitstore executor, fed back to [`decide_outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentMergeResult {
    /// `diffy` produced a clean merge.
    Clean,
    /// `diffy` produced conflict markers, the file was binary/non-UTF-8 with a true three-way divergence,
    /// or a size cap was hit — any of which blocks the overall merge.
    Conflicted,
}

/// The overall outcome of an author merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MergeOutcome {
    /// Every path resolved cleanly to a non-empty tree — commit it as a forward 1-parent draft on the
    /// live tip and land draft-on-current.
    CleanCommitOnTip,
    /// At least one path conflicts (structural, mode, or content), or the clean resolution would empty
    /// the skill — materialize the complete conflict representation and block publish until resolved.
    BlockedConflict,
    /// No recorded base (unrelated histories) — fall back to a 2-way manual choice; never a silent merge.
    /// Selected by the client before planning (when the base cannot be rendered), never returned here.
    NoBaseTwoWay,
}

/// Reconcile the three file-sets into a per-path plan, in raw-path-byte order.
///
/// Pure and total over `(presence, content_sha256, mode)` of each side. Inputs need not be pre-sorted;
/// the union is ordered by raw path bytes (matching [`crate::digest::canonical_manifest`]) so the plan —
/// and therefore the assembled tree's digest — is deterministic.
#[must_use]
pub fn plan_merge(base: &[FileId], mine: &[FileId], theirs: &[FileId]) -> MergePlan {
    #[derive(Default)]
    struct Sides<'a> {
        base: Option<&'a FileId>,
        mine: Option<&'a FileId>,
        theirs: Option<&'a FileId>,
    }
    // A BTreeMap<&str> iterates in key order == raw-byte lexicographic order == the manifest order.
    let mut union: BTreeMap<&str, Sides<'_>> = BTreeMap::new();
    for f in base {
        union.entry(f.path.as_str()).or_default().base = Some(f);
    }
    for f in mine {
        union.entry(f.path.as_str()).or_default().mine = Some(f);
    }
    for f in theirs {
        union.entry(f.path.as_str()).or_default().theirs = Some(f);
    }

    let mut paths = Vec::with_capacity(union.len());
    for (path, s) in union {
        let plan = classify(s.base, s.mine, s.theirs);
        paths.push(PlannedPath {
            path: String::from(path),
            plan,
        });
    }
    MergePlan { paths }
}

/// Whether two present sides have identical content **and** mode (a mode change alone is a real change).
fn same(a: &FileId, b: &FileId) -> bool {
    a.content_sha256 == b.content_sha256 && a.mode == b.mode
}

/// Resolve the consensus mode for a content merge over the two-mode domain: if the sides agree, that
/// mode; otherwise the side that changed away from the base (with two modes exactly one side did).
fn resolve_mode(base: FileMode, mine: FileMode, theirs: FileMode) -> FileMode {
    if mine == theirs {
        mine
    } else if mine == base {
        theirs
    } else {
        mine
    }
}

/// The per-path reconciliation rule — total over the eight presence combinations.
fn classify(base: Option<&FileId>, mine: Option<&FileId>, theirs: Option<&FileId>) -> PathPlan {
    match (base, mine, theirs) {
        // All three present.
        (Some(b), Some(m), Some(t)) => {
            let m_eq_b = same(m, b);
            let t_eq_b = same(t, b);
            if m_eq_b && t_eq_b {
                PathPlan::TakeEither {
                    content_sha256: b.content_sha256,
                    mode: b.mode,
                }
            } else if m_eq_b {
                // I didn't touch it; take their change.
                PathPlan::TakeTheirs {
                    content_sha256: t.content_sha256,
                    mode: t.mode,
                }
            } else if t_eq_b {
                // They didn't touch it; keep my change.
                PathPlan::TakeMine {
                    content_sha256: m.content_sha256,
                    mode: m.mode,
                }
            } else if m.content_sha256 == t.content_sha256 {
                // Both changed to the same content — a clean take with the resolved (here always clean) mode.
                PathPlan::TakeEither {
                    content_sha256: m.content_sha256,
                    mode: resolve_mode(b.mode, m.mode, t.mode),
                }
            } else {
                // Genuine three-way content divergence — the client runs the byte merge.
                PathPlan::ContentMerge {
                    base: b.content_sha256,
                    mine: m.content_sha256,
                    theirs: t.content_sha256,
                    mode: resolve_mode(b.mode, m.mode, t.mode),
                }
            }
        }

        // Base present, one side deleted.
        (Some(b), None, Some(t)) => {
            if same(t, b) {
                PathPlan::Delete // I deleted; they left it untouched.
            } else {
                PathPlan::FileSetConflict {
                    kind: FileSetConflictKind::DeleteModify,
                }
            }
        }
        (Some(b), Some(m), None) => {
            if same(m, b) {
                PathPlan::Delete // They deleted; I left it untouched.
            } else {
                PathPlan::FileSetConflict {
                    kind: FileSetConflictKind::ModifyDelete,
                }
            }
        }
        (Some(_), None, None) => PathPlan::Delete, // Both deleted.

        // Base absent — adds.
        (None, Some(m), None) => PathPlan::TakeMine {
            content_sha256: m.content_sha256,
            mode: m.mode,
        },
        (None, None, Some(t)) => PathPlan::TakeTheirs {
            content_sha256: t.content_sha256,
            mode: t.mode,
        },
        (None, Some(m), Some(t)) => {
            if m.content_sha256 == t.content_sha256 {
                if m.mode == t.mode {
                    PathPlan::TakeEither {
                        content_sha256: m.content_sha256,
                        mode: m.mode,
                    }
                } else {
                    // Same content, different mode, no base to anchor who changed it → disagreement.
                    PathPlan::FileSetConflict {
                        kind: FileSetConflictKind::AddAddModeDiffers,
                    }
                }
            } else {
                PathPlan::FileSetConflict {
                    kind: FileSetConflictKind::AddAddDifferent,
                }
            }
        }

        // Unreachable: a path is in the union only because some side has it.
        (None, None, None) => PathPlan::Delete,
    }
}

/// Decide the overall outcome from the plan + the byte-merge verdicts (in plan order).
///
/// Blocked iff any structural file-set conflict, any conflicted content merge, a verdict/plan count
/// mismatch (fail closed), or a clean resolution that would empty the skill (an all-delete tree — the
/// scanner cannot represent a fileless bundle, so emptying is treated as a conflict to resolve via the
/// escape rather than silently producing an unscannable placement). Otherwise clean.
#[must_use]
pub fn decide_outcome(plan: &MergePlan, content_results: &[ContentMergeResult]) -> MergeOutcome {
    let mut blocked = false;
    let mut kept_files = 0usize;
    let mut content_idx = 0usize;
    for p in &plan.paths {
        match &p.plan {
            PathPlan::Delete => {}
            PathPlan::TakeEither { .. }
            | PathPlan::TakeMine { .. }
            | PathPlan::TakeTheirs { .. } => {
                kept_files += 1;
            }
            PathPlan::FileSetConflict { .. } => {
                // The conflict representation keeps a file on disk, so the tree is not emptied.
                kept_files += 1;
                blocked = true;
            }
            PathPlan::ContentMerge { .. } => {
                kept_files += 1;
                match content_results.get(content_idx) {
                    Some(ContentMergeResult::Clean) => {}
                    // Conflicted, or a missing verdict (count mismatch) → fail closed.
                    _ => blocked = true,
                }
                content_idx += 1;
            }
        }
    }
    // A trailing verdict with no matching ContentMerge path is a contract violation — fail closed.
    if content_idx != content_results.len() {
        blocked = true;
    }
    if blocked || kept_files == 0 {
        MergeOutcome::BlockedConflict
    } else {
        MergeOutcome::CleanCommitOnTip
    }
}

/// A durable record that a skill's working tree holds an unresolved merge conflict — the kernel view of
/// the client's `conflict.json`. The commits/digest are carried for offline recovery + lineage
/// diagnostics; the *gate* is the fact's mere presence (see [`publish_blocked`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConflictFact<'a> {
    /// The three-way base the conflict was computed against.
    pub base_commit: &'a [u8; 32],
    /// `current` (theirs) at the time the conflict was recorded.
    pub current_commit: &'a [u8; 32],
    /// The author's draft (mine) snapshot the conflict was computed from.
    pub draft_commit: &'a [u8; 32],
    /// The digest of the conflict tree written to disk (the heal signal + the recovery pin).
    pub conflicted_digest: &'a [u8; 32],
}

/// Whether a publish must be refused because an unresolved conflict is on record.
///
/// **Presence is the gate** — a durable conflict fact means "blocked", period. The guard is deliberately
/// NOT keyed on a byte/marker scan (a crafted bundle whose content merely *looks* like conflict markers
/// can neither trip nor defeat it) and NOT self-invalidated by an incidental edit (an edit elsewhere in
/// the bundle must not unblock a tree whose markers are still unresolved). The fact is cleared only by a
/// clean resolution (a clean merge) or the disclosed escape — both of which produce a genuinely
/// publishable candidate — so an author is never deadlocked.
#[must_use]
pub fn publish_blocked(conflict: Option<ConflictFact<'_>>) -> bool {
    conflict.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::digest::sha256;
    use alloc::vec;

    fn fid(path: &str, mode: FileMode, content: &[u8]) -> FileId {
        FileId {
            path: String::from(path),
            mode,
            content_sha256: sha256(content),
        }
    }
    fn r(path: &str, content: &[u8]) -> FileId {
        fid(path, FileMode::Regular, content)
    }
    fn x(path: &str, content: &[u8]) -> FileId {
        fid(path, FileMode::Executable, content)
    }

    /// The single path's plan, for one-line row assertions.
    fn plan1(base: &[FileId], mine: &[FileId], theirs: &[FileId]) -> PathPlan {
        let p = plan_merge(base, mine, theirs);
        assert_eq!(p.paths.len(), 1, "expected exactly one path");
        p.paths.into_iter().next().expect("one path").plan
    }

    #[test]
    fn fileset_table_all_present_rows() {
        // m==b, t==b → take either.
        assert!(matches!(
            plan1(&[r("f", b"a")], &[r("f", b"a")], &[r("f", b"a")]),
            PathPlan::TakeEither { .. }
        ));
        // m==b, t changed → take theirs.
        assert!(matches!(
            plan1(&[r("f", b"a")], &[r("f", b"a")], &[r("f", b"T")]),
            PathPlan::TakeTheirs { .. }
        ));
        // t==b, m changed → take mine.
        assert!(matches!(
            plan1(&[r("f", b"a")], &[r("f", b"M")], &[r("f", b"a")]),
            PathPlan::TakeMine { .. }
        ));
        // both changed to the same content → take either.
        assert!(matches!(
            plan1(&[r("f", b"a")], &[r("f", b"S")], &[r("f", b"S")]),
            PathPlan::TakeEither { .. }
        ));
        // all three differ → content merge.
        assert!(matches!(
            plan1(&[r("f", b"a")], &[r("f", b"M")], &[r("f", b"T")]),
            PathPlan::ContentMerge { .. }
        ));
    }

    #[test]
    fn fileset_table_delete_rows() {
        // mine deleted, theirs untouched → accept delete.
        assert_eq!(
            plan1(&[r("f", b"a")], &[], &[r("f", b"a")]),
            PathPlan::Delete
        );
        // theirs deleted, mine untouched → accept delete.
        assert_eq!(
            plan1(&[r("f", b"a")], &[r("f", b"a")], &[]),
            PathPlan::Delete
        );
        // both deleted → delete.
        assert_eq!(plan1(&[r("f", b"a")], &[], &[]), PathPlan::Delete);
        // mine deleted, theirs modified → conflict.
        assert!(matches!(
            plan1(&[r("f", b"a")], &[], &[r("f", b"T")]),
            PathPlan::FileSetConflict {
                kind: FileSetConflictKind::DeleteModify
            }
        ));
        // mine modified, theirs deleted → conflict.
        assert!(matches!(
            plan1(&[r("f", b"a")], &[r("f", b"M")], &[]),
            PathPlan::FileSetConflict {
                kind: FileSetConflictKind::ModifyDelete
            }
        ));
    }

    #[test]
    fn fileset_table_add_rows() {
        // mine-only add.
        assert!(matches!(
            plan1(&[], &[r("f", b"M")], &[]),
            PathPlan::TakeMine { .. }
        ));
        // theirs-only add.
        assert!(matches!(
            plan1(&[], &[], &[r("f", b"T")]),
            PathPlan::TakeTheirs { .. }
        ));
        // add/add identical → take either.
        assert!(matches!(
            plan1(&[], &[r("f", b"S")], &[r("f", b"S")]),
            PathPlan::TakeEither { .. }
        ));
        // add/add different content → conflict.
        assert!(matches!(
            plan1(&[], &[r("f", b"M")], &[r("f", b"T")]),
            PathPlan::FileSetConflict {
                kind: FileSetConflictKind::AddAddDifferent
            }
        ));
        // add/add same content, different mode (no base) → mode conflict.
        assert!(matches!(
            plan1(&[], &[r("f", b"S")], &[x("f", b"S")]),
            PathPlan::FileSetConflict {
                kind: FileSetConflictKind::AddAddModeDiffers
            }
        ));
    }

    #[test]
    fn mode_change_is_consent_significant() {
        // Same content, mine flips the executable bit, theirs untouched → take mine (with its mode).
        let p = plan1(
            &[r("run", b"#!/bin/sh\n")],
            &[x("run", b"#!/bin/sh\n")],
            &[r("run", b"#!/bin/sh\n")],
        );
        assert_eq!(
            p,
            PathPlan::TakeMine {
                content_sha256: sha256(b"#!/bin/sh\n"),
                mode: FileMode::Executable,
            }
        );
        // A pure mode change is NOT "unchanged": mine == base only if mode matches too.
        assert!(matches!(
            plan1(&[r("run", b"x")], &[x("run", b"x")], &[r("run", b"y")]),
            // mine changed (mode), theirs changed (content); contents differ → content merge.
            PathPlan::ContentMerge { .. }
        ));
    }

    #[test]
    fn content_merge_resolves_mode_with_one_changed_side() {
        // base R, mine X, theirs R, all contents differ → content merge keeps the executable bit.
        let p = plan1(&[r("f", b"a")], &[x("f", b"M")], &[r("f", b"T")]);
        match p {
            PathPlan::ContentMerge { mode, .. } => assert_eq!(mode, FileMode::Executable),
            other => panic!("expected ContentMerge, got {other:?}"),
        }
    }

    #[test]
    fn plan_is_sorted_by_raw_path_bytes() {
        let p = plan_merge(
            &[r("b", b"1"), r("a", b"1")],
            &[r("b", b"1"), r("a", b"1")],
            &[r("b", b"1"), r("a", b"1")],
        );
        let paths: Vec<&str> = p.paths.iter().map(|pp| pp.path.as_str()).collect();
        assert_eq!(paths, vec!["a", "b"]);
    }

    #[test]
    fn decide_outcome_clean_when_all_resolve() {
        let plan = plan_merge(&[r("f", b"a")], &[r("f", b"M")], &[r("f", b"a")]);
        // One TakeMine, no content merges.
        assert_eq!(decide_outcome(&plan, &[]), MergeOutcome::CleanCommitOnTip);
    }

    #[test]
    fn decide_outcome_blocks_on_structural_conflict() {
        let plan = plan_merge(&[r("f", b"a")], &[], &[r("f", b"T")]); // delete/modify
        assert_eq!(decide_outcome(&plan, &[]), MergeOutcome::BlockedConflict);
    }

    #[test]
    fn decide_outcome_blocks_on_conflicted_content_merge() {
        let plan = plan_merge(&[r("f", b"a")], &[r("f", b"M")], &[r("f", b"T")]); // content merge
        assert_eq!(
            decide_outcome(&plan, &[ContentMergeResult::Clean]),
            MergeOutcome::CleanCommitOnTip
        );
        assert_eq!(
            decide_outcome(&plan, &[ContentMergeResult::Conflicted]),
            MergeOutcome::BlockedConflict
        );
    }

    #[test]
    fn decide_outcome_fails_closed_on_verdict_count_mismatch() {
        let plan = plan_merge(&[r("f", b"a")], &[r("f", b"M")], &[r("f", b"T")]); // one content merge
        // Too few verdicts.
        assert_eq!(decide_outcome(&plan, &[]), MergeOutcome::BlockedConflict);
        // Too many verdicts.
        assert_eq!(
            decide_outcome(
                &plan,
                &[ContentMergeResult::Clean, ContentMergeResult::Clean]
            ),
            MergeOutcome::BlockedConflict
        );
    }

    #[test]
    fn decide_outcome_blocks_an_emptying_merge() {
        // Both sides deleted the only file → an empty clean tree → blocked (resolve via escape).
        let plan = plan_merge(&[r("f", b"a")], &[], &[]);
        assert_eq!(plan.paths.len(), 1);
        assert_eq!(plan.paths[0].plan, PathPlan::Delete);
        assert_eq!(decide_outcome(&plan, &[]), MergeOutcome::BlockedConflict);
    }

    #[test]
    fn content_merge_paths_are_in_plan_order() {
        let plan = plan_merge(
            &[r("a", b"0"), r("b", b"0")],
            &[r("a", b"1"), r("b", b"1")],
            &[r("a", b"2"), r("b", b"2")],
        );
        let cm: Vec<&str> = plan
            .content_merge_paths()
            .map(|p| p.path.as_str())
            .collect();
        assert_eq!(cm, vec!["a", "b"]);
        assert!(!plan.has_structural_conflict());
    }

    #[test]
    fn publish_guard_is_presence_based() {
        assert!(!publish_blocked(None));
        let z = [0u8; 32];
        assert!(publish_blocked(Some(ConflictFact {
            base_commit: &z,
            current_commit: &z,
            draft_commit: &z,
            conflicted_digest: &z,
        })));
    }
}
