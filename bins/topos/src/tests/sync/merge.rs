//! Author-side merge resolution: the clean merge and its fixpoint, the targeted accept,
//! `--keep-mine`, hand resolutions, conflict blocks, resets, and the crash gates.

use topos_core::digest::{FileMode, to_hex};
use topos_types::results::PullAction;

use crate::fs_seam::FaultFs;
use crate::plane::{FollowSource, InertFollow};
use crate::{doc, ops};

use super::rig::*;

// =================================================================================================
// Author-side merge resolution (the diff3 increment): clean merge, the fixpoint, the targeted accept,
// the escape, conflict-blocks-publish, no-base, structural author-only, binary sidecars, and the crash
// gate. These drive the full resolve through the public `ops::pull` entry point against a real store.
// =================================================================================================

/// Three single-file versions whose edits are on disjoint lines → a clean three-way merge.
fn clean_trio() -> (FileSet, FileSet, FileSet) {
    (
        &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")], // base
        &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")],  // mine (edited line 1)
        &[("SKILL.md", FileMode::Regular, b"line1\nline2\nTHEIRS\n")], // theirs (edited line 3)
    )
}

/// A clean three-way merge: an AUTO follower's bare sweep combines both edits into a draft-on-current —
/// `applied == observed`, `base == theirs`, no conflict record, publishable.
#[test]
fn auto_sweep_clean_merge_lands_draft_on_current() {
    let (base, mine, theirs) = clean_trio();
    let rig = Rig::new("clean");
    let (id, _name, genesis) = rig.adopt(base);
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Merged);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(mr.clean);
    assert_eq!(mr.theirs_version_id, to_hex(&v1.id));

    // Both edits are combined on disk; nothing is a conflict marker.
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"MINE\nline2\nTHEIRS\n"
    );
    assert!(
        !rig.conflict_exists(&id),
        "a clean merge writes no conflict record"
    );

    // draft-on-current: the pending update is consumed; the working tree reads as a draft on `current`.
    let s = rig.read_sync(&id);
    assert_eq!(s.applied, 1);
    assert_eq!(s.base_commit, to_hex(&v1.id));
}

/// The clean merge is a stable fixpoint: a re-pull with `current` unchanged is a no-op (never re-merged,
/// never clobbered); and when `current` moves again the merged draft is re-resolved, NEVER fast-forwarded
/// over — so the author's edit is never lost across rounds.
#[test]
fn clean_merge_is_a_stable_fixpoint_with_no_lost_update() {
    let (base, mine, theirs) = clean_trio();
    let rig = Rig::new("fixpoint");
    let (id, _name, genesis) = rig.adopt(base);
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Merged
    );

    // (1) Re-pull, `current` unchanged → UpToDate (the draft is not nagged, not re-merged, not clobbered).
    let again = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&again).action, PullAction::UpToDate);
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"MINE\nline2\nTHEIRS\n"
    );

    // (2) `current` moves to v2 (an edit on line 3, disjoint from MINE's line-1 edit with an unchanged
    // line 2 between them so diff3 merges cleanly) → the merged draft re-resolves, NOT a fast-forward.
    let v2files: &[(&str, FileMode, &[u8])] =
        &[("SKILL.md", FileMode::Regular, b"line1\nline2\nV2\n")];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane2 = FixturePlane::default();
    plane2.add_version(&id, &v1);
    plane2.add_version(&id, &v2);
    plane2.set_current(&id, served(WS, &id, v2.id, 2));
    let row =
        only(&pull_data(&rig.ctx(&plane2, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_ne!(
        row.action,
        PullAction::FastForwarded,
        "a fast-forward would clobber the merged draft (lost update)"
    );
    assert_eq!(row.action, PullAction::Merged);
    // MINE's original line-1 edit survived two merge rounds.
    let final_skill = std::fs::read(rig.placement().join("SKILL.md")).unwrap();
    assert!(
        final_skill.starts_with(b"MINE\n"),
        "lost update: {final_skill:?}"
    );
}

/// The TARGETED accept resolves a diverged draft exactly as the bare sweep does — the other arm of
/// the resolve-strategy table — and the in-memory preview called this trio clean before anything
/// ran (the prediction `publish`'s describe shows an author whose copy is behind `current`).
#[test]
fn a_targeted_accept_merges_a_diverged_draft_the_preview_called_clean() {
    let (base, mine, theirs) = clean_trio();
    let rig = Rig::new("accept-merge");
    let (id, _name, genesis) = rig.adopt(base);
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The prediction, over already-local bytes and writing none of them.
    let scanned = crate::scan::scan(&rig.placement()).unwrap();
    let preview = ops::merge_resolve::preview_merge(&rendered(base), &scanned, &rendered(theirs));
    assert_eq!(
        preview.verdict,
        topos_types::results::MergePreviewVerdict::Clean
    );
    assert!(preview.conflicts.is_empty());
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(mine)),
        "the preview wrote no byte"
    );

    // The targeted accept runs it.
    let accepted = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    assert_eq!(only(&accepted).action, PullAction::Merged);
    assert_eq!(
        std::fs::read(rig.placement().join("SKILL.md")).unwrap(),
        b"MINE\nline2\nTHEIRS\n"
    );
    assert!(!rig.conflict_exists(&id));
}

/// **The escape is `git merge -X ours`, and this is the test that says so.**
///
/// The team's version does THREE things at once: it rewrites the line this person also rewrote
/// (contested), it rewrites a line far away that this person left alone (uncontested), and it adds a
/// file that did not exist before (uncontested). `--keep-mine` must keep this person's wording on the
/// first and take BOTH of the others — which is exactly what a person gets by pulling, editing the
/// contested line back to their wording, and committing.
///
/// The shipped code committed the person's WHOLE folder instead (`-s ours`), so the far-away line
/// reverted and the new file was deleted — silently, and nowhere disclosed. That is the defect.
#[test]
fn keep_mine_keeps_my_side_of_the_collision_and_takes_the_rest() {
    let rig = Rig::new("escape");
    let base: FileSet = &[
        (
            "SKILL.md",
            FileMode::Regular,
            b"top\nmid1\nmid2\nmid3\nmid4\nmid5\nbottom\n",
        ),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    let (id, _name, genesis) = rig.adopt(base);
    // MINE: the top line, and nothing else.
    let mine: FileSet = &[
        (
            "SKILL.md",
            FileMode::Regular,
            b"TOP-mine\nmid1\nmid2\nmid3\nmid4\nmid5\nbottom\n",
        ),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    // THEIRS: the same top line (contested), the bottom line (uncontested), a new file, and a
    // rewrite of a file this person never touched.
    let theirs: FileSet = &[
        (
            "SKILL.md",
            FileMode::Regular,
            b"TOP-theirs\nmid1\nmid2\nmid3\nmid4\nmid5\nBOTTOM-theirs\n",
        ),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The merge stops FIRST — `--keep-mine` finishes a stopped merge, and there is nothing to
    // finish before one has stopped (see `keep_mine_refuses_wherever_no_merge_has_stopped`).
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    let data = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Merged);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(mr.clean);
    assert!(mr.drop_diff.is_some(), "the escape discloses what it drops");
    // WHICH exit finished it — the receipt cannot speak for both at once, and a `drop_diff` cannot
    // tell them apart because both carry one.
    assert_eq!(
        mr.resolved,
        Some(topos_types::results::MergeResolution::KeepMine)
    );

    // THE ASSERTION THIS WHOLE CHANGE EXISTS FOR.
    let resolved: FileSet = &[
        (
            "SKILL.md",
            FileMode::Regular,
            b"TOP-mine\nmid1\nmid2\nmid3\nmid4\nmid5\nBOTTOM-theirs\n",
        ),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(resolved)),
        "my wording on the contested line; everything else of theirs came with it"
    );
    // And the receipt NAMES what came along, so the fact is readable rather than inferred.
    assert_eq!(
        mr.took,
        vec![
            "SKILL.md".to_owned(),
            "ref/notes.md".to_owned(),
            "run.sh".to_owned()
        ],
        "{mr:?}"
    );

    assert!(!rig.conflict_exists(&id));
    let s = rig.read_sync(&id);
    // The base IS theirs — the commit's one parent — so the plane's lineage fence is satisfied and
    // publishing from here is an ordinary publish.
    assert_eq!(
        s.base_commit,
        to_hex(&v1.id),
        "the escape commits ON the team's version"
    );
    assert_eq!(s.applied, s.observed, "and takes it as applied");
    assert_eq!(s.observed, 1);
    assert!(!s.held);

    // The escape's own commit is a real 1-parent commit on `current` — the history stays a list.
    let m = mk_version(&[v1.id], resolved, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

/// The structural collisions, one table, each against the git command that produces the same
/// answer. Every one resolves — `--keep-mine` never deadlocks on a file — and every one resolves
/// the way git resolves it, so nothing here is a topos invention a person has to learn.
///
/// `git merge -X ours` is the citation for exactly TWO of these: it settles CONTENT, so a
/// both-added-different-content collision and a binary one come out ours. It settles nothing about
/// the file SET — a modify/delete, a delete/modify, and a disagreeing mode all stop that merge
/// dead, and what takes your side there is the per-file take-ours command. The rows are cited
/// accordingly.
///
/// And each row asserts the RECEIPT, not only the resulting files. A resolution that silently
/// dropped something the team changed produces exactly the file set the person asked for, so the
/// files alone can never catch it: what the row must also carry is the disclosure of what went —
/// `took` for what came over from them, `drop_diff` for what did not.
#[test]
fn keep_mine_settles_every_structural_collision_the_way_git_does() {
    // (label, base, mine, theirs, expected, what the drop disclosure must name)
    let cases: &[(&str, FileSet, FileSet, FileSet, FileSet, &str)] = &[
        (
            // Mine modified, theirs deleted. `-X ours` does not resolve this at all; taking our
            // side is `git checkout --ours doomed.md`, which keeps the file.
            "modify/delete",
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("doomed.md", FileMode::Regular, b"base\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("doomed.md", FileMode::Regular, b"MINE\n"),
            ],
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("doomed.md", FileMode::Regular, b"MINE\n"),
            ],
            "doomed.md",
        ),
        (
            // Mine deleted, theirs modified. `-X ours` does not resolve this either, and
            // `git checkout --ours` cannot: our side has no such file. Taking our side is
            // `git rm gone.md` — it stays gone.
            "delete/modify",
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("gone.md", FileMode::Regular, b"base\n"),
            ],
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("gone.md", FileMode::Regular, b"THEIRS\n"),
            ],
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            "gone.md",
        ),
        (
            // Both added the same path with different content — a CONTENT collision, so
            // `git merge -X ours` settles it on ours.
            "add/add",
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("new.md", FileMode::Regular, b"MINE\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("new.md", FileMode::Regular, b"THEIRS\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("new.md", FileMode::Regular, b"MINE\n"),
            ],
            "new.md",
        ),
        (
            // Identical content, disagreeing modes. Only the executable bit is in dispute, and
            // `-X ours` has no say over it; `git checkout --ours run.sh` restores our bit.
            "add/add, mode only",
            &[("SKILL.md", FileMode::Regular, b"keep\n")],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("run.sh", FileMode::Regular, b"#!/bin/sh\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("run.sh", FileMode::Executable, b"#!/bin/sh\n"),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("run.sh", FileMode::Regular, b"#!/bin/sh\n"),
            ],
            "run.sh",
        ),
        (
            // A binary file both sides changed — a CONTENT collision with no hunks to reconcile,
            // which `git merge -X ours` takes whole from ours.
            "binary",
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("logo.bin", FileMode::Regular, &[0xff, 0x00, 0x01]),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("logo.bin", FileMode::Regular, &[0xff, 0x00, 0x02]),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("logo.bin", FileMode::Regular, &[0xff, 0x00, 0x03]),
            ],
            &[
                ("SKILL.md", FileMode::Regular, b"keep\n"),
                ("logo.bin", FileMode::Regular, &[0xff, 0x00, 0x02]),
            ],
            "logo.bin",
        ),
    ];

    for (label, base, mine, theirs, want, dropped) in cases {
        let rig = Rig::new(&format!("escape-{}", label.replace(['/', ' ', ','], "-")));
        let (id, _name, genesis) = rig.adopt(base);
        write_tree(&rig.placement(), mine);
        let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        assert_eq!(
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
            PullAction::Conflicted,
            "{label}"
        );
        let escaped = pull_data(
            &rig.ctx(&plane, &foll),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .unwrap();
        let row = only(&escaped).clone();
        assert_eq!(row.action, PullAction::Merged, "{label}");
        assert_eq!(snapshot(&rig.placement()), Some(expect(want)), "{label}");
        assert!(!rig.conflict_exists(&id), "{label}");

        // THE RECEIPT. Which exit finished it, what came over from the team (nothing in any of
        // these — every one of them settles the whole collision on this person's side), and the
        // disclosure of what the team wrote that did NOT survive it, naming the file.
        let mr = row
            .merge
            .as_ref()
            .unwrap_or_else(|| panic!("{label}: a merge report"));
        assert_eq!(
            mr.resolved,
            Some(topos_types::results::MergeResolution::KeepMine),
            "{label}"
        );
        assert_eq!(
            mr.took,
            Vec::<String>::new(),
            "{label}: nothing here comes over from the team"
        );
        let dd = mr
            .drop_diff
            .as_deref()
            .unwrap_or_else(|| panic!("{label}: the exit discloses what it drops"));
        assert!(
            dd.contains(dropped),
            "{label}: the receipt never names the team's change it dropped ({dropped}):\n{dd}"
        );
        // And the rendered row says it in the one voice a person reads.
        let rendered = crate::render::pull_tty(&escaped, &[], &[], &[], &[], 0, 0);
        assert!(
            rendered.contains("kept your wording where you both changed the same lines"),
            "{label}: {rendered}"
        );
        assert!(
            !rendered.contains("took everything else the team changed"),
            "{label}: nothing was taken, so nothing may be claimed: {rendered}"
        );
    }
}

/// A hand-edited workbench is committed EXACTLY as the person wrote it — the merge is not re-run
/// over it and nothing of theirs is re-applied. They resolved it; second-guessing their tree would
/// be worse than not asking.
///
/// And the ROW says so. This person took the team's contested line and kept a change of their own
/// somewhere else — the exact tree a "kept your wording where you both changed the same lines"
/// headline would misdescribe, which is what the row said while both exits were told apart by a
/// `drop_diff` they both carry. It also claims no file as taken from the team: every byte here is
/// this person's, and topos never looked at any of it.
#[test]
fn a_hand_resolution_is_committed_exactly_as_written() {
    let rig = Rig::new("escape-reconciled");
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nline3\n")];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // The team's contested line, taken as it stands, plus a change of this person's own elsewhere.
    let reconciled: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nMINE3\n")];
    write_tree(&rig.conflict_copy(&id), reconciled);
    let row = only(
        &pull_data(
            &rig.ctx(&plane, &foll),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .unwrap(),
    )
    .clone();
    assert_eq!(row.action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(reconciled)));
    let mr = row.merge.as_ref().expect("a merge report");
    assert_eq!(
        mr.resolved,
        Some(topos_types::results::MergeResolution::ByHand)
    );
    assert!(
        mr.took.is_empty(),
        "nothing in a tree topos never examined is claimable as taken from the team: {:?}",
        mr.took
    );
    let s = rig.read_sync(&id);
    assert_eq!(s.base_commit, to_hex(&v1.id));
    assert_eq!(s.applied, s.observed);
}

/// The no-deadlock guarantee, at its narrowest: a hand resolution needs NEITHER the stored draft
/// nor the fork point, so an exit that always works must not begin by re-rendering them.
///
/// The recorded `draft_commit` here names an object that is not in the store. Reading it up front —
/// only ever to compute what the OTHER exit took from the team — turned a corrupt or pruned draft
/// into a failed escape on the one path that has a complete answer sitting on disk.
#[test]
fn a_hand_resolution_needs_neither_the_draft_nor_the_fork_point() {
    let rig = Rig::new("escape-lazy-draft");
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nline3\n")];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    let reconciled: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nMINE3\n")];
    write_tree(&rig.conflict_copy(&id), reconciled);
    // Point the record's draft at an object nothing ever wrote.
    let path = rig.layout().published(&sid(&id)).conflict;
    let mut cs = rig.conflict_state(&id);
    cs.draft_commit = "ab".repeat(32);
    cs.draft_digest = "cd".repeat(32);
    doc::write_doc(&rig.fs, &path, &cs).unwrap();

    let row = only(
        &pull_data(
            &rig.ctx(&plane, &foll),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .expect("the exit that always works, works"),
    )
    .clone();
    assert_eq!(row.action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(reconciled)));
    assert!(!rig.conflict_exists(&id));
}

/// A stopped merge is finished by `--keep-mine` whether or not anything still FOLLOWS the bundle.
///
/// The block lives in this machine's own record and the resolution in its own store; nothing about
/// either is the follow-state's to answer. Unfollowing a bundle that is mid-conflict used to leave
/// it wedged: the exit was routed through the followed-only sync, so it fell through to a read-only
/// "up to date" while `publish` stayed refused by a block nothing could clear.
#[test]
fn keep_mine_finishes_a_stopped_merge_on_a_row_nobody_follows() {
    let rig = Rig::new("escape-unfollowed");
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nline3\n")];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // The follow goes away with the block still standing.
    let unfollowed = InertFollow;
    let row = only(
        &pull_data(
            &rig.ctx(&plane, &unfollowed),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .expect("the escape is plane- and follow-independent"),
    )
    .clone();
    assert_eq!(row.action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    assert!(!rig.conflict_exists(&id), "the block is cleared");
    assert_eq!(rig.read_sync(&id).base_commit, to_hex(&v1.id));
}

/// Where `--keep-mine` leaves the bundle, seen from the NEXT update: an ordinary draft on the team's
/// version. Nothing new from the team is a no-op; a newer team version runs a real three-way merge
/// FROM THEIRS — the version the escape's commit is parented on.
///
/// The old shape of this test asserted the opposite (a merge re-run from the PRE-conflict base, with
/// the same conflict raised again forever). That was the loop: the decision never became durable, so
/// every sweep re-asked the question the person had already answered.
#[test]
fn after_keep_mine_the_draft_sits_on_theirs_and_merges_forward_from_it() {
    let rig = Rig::new("keepmine-forward");
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let (id, _name, genesis) = rig.adopt(base);
    // The same line, differently — a genuine conflict.
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nline3\n")];
    write_tree(&rig.placement(), mine);

    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The ordinary sweep conflicts.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    // Keep mine, leaving the workbench alone.
    let keep = ops::PullScope::One {
        store: ops::StoreScope::Here,
        name: "pr-describe".into(),
        workspace: None,
        mode: ops::TargetMode::KeepMine,
    };
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), keep).unwrap()).action,
        PullAction::Merged
    );
    assert!(!rig.conflict_exists(&id));
    assert_eq!(rig.read_sync(&id).base_commit, to_hex(&v1.id));

    // NOTHING NEW FROM THE TEAM: the decision stands and the sweep says so. The old behavior
    // re-raised the very conflict this exit resolved, on every single sweep.
    let quiet =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action;
    assert_eq!(quiet, PullAction::UpToDate, "the decision is durable");
    assert!(!rig.conflict_exists(&id));
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));

    // A NEWER TEAM VERSION: a real three-way merge, from THEIRS (v1) — the version this draft is
    // parented on. v2 touches a line nobody else did, so it merges clean and lands silently.
    let v2files: FileSet = &[("SKILL.md", FileMode::Regular, b"THEIRS\nline2\nTEAM3\n")];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane2 = FixturePlane::default();
    plane2.add_version(&id, &v1);
    plane2.add_version(&id, &v2);
    plane2.set_current(&id, served(WS, &id, v2.id, 2));
    let merged =
        only(&pull_data(&rig.ctx(&plane2, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(merged.action, PullAction::Merged);
    assert!(merged.merge.as_ref().is_some_and(|m| m.clean));
    assert_eq!(
        merged.merge.as_ref().map(|m| m.base_version_id.clone()),
        Some(to_hex(&v1.id)),
        "the merge's base is the version the escape committed on: {merged:?}"
    );
    // This person's line-1 choice survived; the team's new line-3 landed beside it.
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[(
            "SKILL.md",
            FileMode::Regular,
            b"MINE\nline2\nTEAM3\n"
        )]))
    );
    let s = rig.read_sync(&id);
    assert_eq!(s.base_commit, to_hex(&v2.id));
    assert_eq!(s.applied, s.observed);
}

/// `--keep-mine` FINISHES a stopped merge, and refuses EVERY way there is nothing to finish —
/// including on a row nobody follows.
///
/// Each shape had its own silent wrong answer: on an up-to-date copy and on a plain draft it
/// reported success and did nothing; on a clean copy that was merely behind it APPLIED the team's
/// version — the exact opposite of what the flag says; on a live divergence it committed the draft
/// over changes the merge was about to land. One check, before any of them.
///
/// The last two shapes are the ROUTING half, and they are the ones a followed-only fixture could
/// never catch: the refusal used to live inside the followed-sync path, so a tracked-but-unfollowed
/// row — a local path, a forge import, an unfollowed workspace bundle — never reached it and was
/// answered with a read-only "up to date" instead, `ok: true` and exit 0.
#[test]
fn keep_mine_refuses_wherever_no_merge_has_stopped() {
    let base: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nline3\n")];
    let mine: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nline3\n")];
    // The team changed line 3 only — nothing collides, so a merge would land both sides.
    let theirs: FileSet = &[("SKILL.md", FileMode::Regular, b"line1\nline2\nTEAM3\n")];
    let both: FileSet = &[("SKILL.md", FileMode::Regular, b"MINE\nline2\nTEAM3\n")];

    // (label, does the team have a newer version?, is there a local draft?, is the row followed?)
    let shapes: &[(&str, bool, bool, bool)] = &[
        ("up to date", false, false, true),
        ("a plain draft, nothing pending", false, true, true),
        ("behind, clean", true, false, true),
        ("diverged, merge never run", true, true, true),
        ("not followed, clean", false, false, false),
        ("not followed, drafted", false, true, false),
    ];
    for (label, pending, drafted, followed) in shapes {
        let rig = Rig::new(&format!("keepmine-{}", label.replace([' ', ','], "-")));
        let (id, name, genesis) = rig.adopt(base);
        let on_disk: FileSet = if *drafted { mine } else { base };
        if *drafted {
            write_tree(&rig.placement(), mine);
        }
        let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        if *pending {
            plane.add_version(&id, &v1);
            plane.set_current(&id, served(WS, &id, v1.id, 1));
        } else {
            plane.set_current(&id, served(WS, &id, genesis, 0));
        }
        let foll = follow(&id);
        let unfollowed = InertFollow;
        let fsrc: &dyn FollowSource = if *followed { &foll } else { &unfollowed };

        let err = pull_data(
            &rig.ctx(&plane, fsrc),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: "pr-describe".into(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .expect_err(label);
        assert!(
            matches!(&err, crate::error::ClientError::NoStoppedMerge { skill, .. } if skill == &name),
            "{label}: {err:?}"
        );
        // Nothing moved: no commit, no placement write, no record.
        assert_eq!(snapshot(&rig.placement()), Some(expect(on_disk)), "{label}");
        assert!(!rig.conflict_exists(&id), "{label}");
        assert_eq!(rig.read_sync(&id).base_commit, to_hex(&genesis), "{label}");

        // And the merge the refusal names does exactly what it promised.
        if *pending {
            assert_eq!(
                only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap())
                    .action,
                if *drafted {
                    PullAction::Merged
                } else {
                    PullAction::FastForwarded
                },
                "{label}"
            );
            assert_eq!(
                snapshot(&rig.placement()),
                Some(expect(if *drafted { both } else { theirs })),
                "{label}"
            );
        }
    }
}

/// A conflict blocks publish and the block PERSISTS — a bare re-sweep keeps reporting it (healing a
/// crashed materialize), and editing the working tree does NOT clear it (the guard is presence-based, not
/// a digest/marker scan). Only the escape (or a clean re-merge) clears it.
#[test]
fn conflict_blocks_and_persists_until_escaped() {
    let rig = Rig::new("persist");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Auto sweep → conflict (overlapping SKILL.md) → blocked.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(rig.conflict_exists(&id));

    // A bare re-sweep keeps reporting the block (does not silently clear or advance).
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(rig.conflict_exists(&id));

    // The author hand-resolves the marked-up copy — the block STILL stands (presence-based, not a
    // marker scan), and the re-sweep leaves the hand resolution exactly as written.
    let copy = rig.conflict_copy(&id);
    write_tree(
        &copy,
        &[
            ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
            ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
        ],
    );
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(
        rig.conflict_exists(&id),
        "an edit must not clear the conflict"
    );
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        b"# hand-resolved\n",
        "a re-sweep never clobbers the author's hand resolution"
    );

    // The escape resolves it: the block clears, the copy goes with it, and a publishable
    // draft-on-current results.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(!rig.conflict_exists(&id), "the escape clears the block");
    assert!(!copy.exists(), "the marked-up copy goes with the block");
}

/// A marked-up copy that goes missing while the block still stands is RE-RENDERED from the recorded
/// result on the next sweep (the copy is derived state; the store holds it). And a copy nobody has
/// touched still means "keep my version": the escape then commits the merge resolved to this
/// person's side, exactly as if the folder had been sitting there untouched all along.
#[test]
fn a_deleted_conflict_copy_is_re_rendered_and_still_reads_as_untouched() {
    let rig = Rig::new("copy-gone");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    let copy = rig.conflict_copy(&id);
    let tree = snapshot(&copy);
    std::fs::remove_dir_all(&copy).unwrap();

    // The next bare sweep still reports the block AND puts the workbench back, byte for byte.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert_eq!(
        snapshot(&copy),
        tree,
        "the copy is re-rendered from the store"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));

    // Untouched ⇒ the escape keeps the author's version.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(!copy.exists());
}

/// **A folder holding topos's OWN marked-up tree is never read as the person's work.** The
/// workbench is where markers go; a placement can still end up holding that exact tree — an
/// install that upgraded mid-merge, a person who copied the workbench over their agent folder —
/// and the escape reads the working tree from the placements. Committing what it found there would
/// publish the conflict markers as this person's version, under a row that says `merged`.
///
/// So the marked-up tree is not a draft: the escape falls back to the recorded draft snapshot (the
/// same answer [`super::super::ops::merge_resolve`] gives when the WORKBENCH is untouched), and the
/// re-disclosure gives that folder its own line instead of calling it newer edits of theirs.
#[test]
fn a_placement_holding_the_marker_tree_is_never_committed_as_the_persons_work() {
    let rig = Rig::new("marker-tree-placement");
    let (id, name, genesis) = rig.adopt(BASE);
    write_tree(&rig.placement(), MINE_OVER_BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // Plant it: the workbench tree, byte for byte (modes included), in the agent folder.
    fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let (src, dest) = (e.path(), to.join(e.file_name()));
            if src.is_dir() {
                copy_tree(&src, &dest);
            } else {
                std::fs::copy(&src, &dest).unwrap();
            }
        }
    }
    let copy = rig.conflict_copy(&id);
    std::fs::remove_dir_all(rig.placement()).unwrap();
    copy_tree(&copy, &rig.placement());
    assert_eq!(
        to_hex(&crate::scan::scan(&rig.placement()).unwrap().bundle_digest),
        rig.conflict_state(&id).conflicted_digest,
        "the placement really holds the marked-up tree"
    );

    // THE DISCLOSURE: that folder gets its own line — never "your newer edits".
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    assert_eq!(
        row.merge
            .as_ref()
            .expect("a merge report")
            .placements
            .iter()
            .map(|p| p.holds)
            .collect::<Vec<_>>(),
        vec![topos_types::results::ConflictHolds::MarkedUp]
    );
    let tty = crate::render::pull_tty(&data, &[], &[], &[], &[], 0, 0);
    let leaf = rig
        .placement()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    assert!(
        tty.lines().any(|l| l.starts_with("      ")
            && l.contains(&leaf)
            && l.ends_with("holds both versions marked up — run one of the ways out below")),
        "{tty}"
    );
    assert!(!tty.contains("newer edits"), "{tty}");

    // THE EXIT: `--keep-mine` commits the recorded DRAFT against the team's version — the ordinary
    // keep-mine result — and every folder converges on it. No markers reach disk, and no marked-up
    // tree reaches the store as a version.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: name.clone(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(!copy.exists(), "the workbench goes with the block");
}

/// The escape writes its committed bytes over EVERY managed placement, so copies that held
/// DIFFERENT edits are collapsed into one. The recorded-conflict entry runs before the work-tree
/// classification, so the typed competitor freeze never fires here — deliberately, since freezing
/// would deadlock the one exit that always works — and that makes the DISCLOSURE the whole
/// protection: the row names how many folders disagreed and hands back one runnable line per copy
/// that puts those exact bytes back.
#[test]
fn the_escape_discloses_the_divergent_copies_it_collapsed() {
    let rig = Rig::new("escape-collapse");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Sweep → conflict: the placement keeps MINE, the workbench holds the marked-up tree.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );

    // A SECOND managed folder carrying its OWN, different edit: neither copy's bytes are the
    // other's recorded baseline, so they are true competitors.
    let replica = rig.work.0.join("replica");
    let other: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# other\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    add_replica(&rig, &id, &replica, other);

    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    let row = only(&escaped);
    assert_eq!(row.action, PullAction::Merged);
    // The collapse really happened: the untouched workbench means "keep my version", and BOTH
    // folders now hold the resolution — the replica's own edit is gone from disk.
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert_eq!(snapshot(&replica), Some(expect(KEPT_OVER_V1)));

    // …and the row says so, in the receipt's own destination convention.
    let note = row.note.as_deref().expect("the collapse is disclosed");
    let lines: Vec<&str> = note.lines().collect();
    assert_eq!(
        lines[0], "overwrote different edits in 2 folders — restore a copy:",
        "{note}"
    );
    assert_eq!(
        lines.len(),
        3,
        "one recovery line per collapsed copy: {note}"
    );
    // Each recovery line is runnable AS PRINTED and names the folder it came from — and the
    // version it offers is one the store really holds, so the go-back it spells resolves.
    let versions = rig.open_store(&id).list_versions().unwrap();
    let real = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    };
    for (line, dir) in lines[1..].iter().zip([rig.placement(), replica.clone()]) {
        let named = line
            .split_once("   (was in ")
            .and_then(|(_, rest)| rest.strip_suffix(')'))
            .unwrap_or_else(|| panic!("a named folder: {line}"));
        assert_eq!(real(std::path::Path::new(named)), real(&dir), "{line}");
        let hash = line
            .split_once("pr-describe@")
            .and_then(|(_, rest)| rest.split_once("   "))
            .map(|(h, _)| h)
            .unwrap_or_else(|| panic!("a go-back token: {line}"));
        assert!(line.starts_with("  topos update -g pr-describe@"), "{line}");
        assert_eq!(hash.len(), 12, "{line}");
        assert!(
            versions.iter().any(|v| to_hex(v).starts_with(hash)),
            "the offered version is in the store: {line}"
        );
    }
}

/// **A `--dest` reset of ONE copy does not conclude a stopped merge — and the `--keep-mine` the
/// receipt names must then FINISH it, never refuse.** The narrowed reset advances the durable
/// documents to exactly the values a LANDED exit leaves behind — `sync.work_hash` to the reset
/// copy's digest (= base = the conflict's theirs), `map.applied_commit` to `lock.base_commit`
/// (= the conflict's `current_commit`), the map-level `materialized_sha` mirror to the same digest
/// — while another copy still holds the un-merged draft. Any recovery that INFERS "an exit already
/// landed" from those documents deletes the live record and its workbench, refuses
/// `NoStoppedMerge`, and leaves the surviving draft unblocked for `publish` (the record's absence
/// IS the publish guard). Liveness is a recorded fact (the `concluded` mark), never a document
/// comparison.
#[test]
fn a_narrowed_reset_leaves_the_merge_stopped_and_the_escape_still_finishes_it() {
    let rig = Rig::new("narrow-reset-live");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Sweep → a recorded stopped merge; then a SECOND managed copy holding the same un-merged
    // draft (one draft in two folders — never competitors), which is what the narrowed reset
    // leaves standing.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    let replica = rig.work.0.join("replica");
    add_replica(&rig, &id, &replica, mine);
    let copy = rig.conflict_copy(&id);

    // The REAL narrowed reset, apply arm: the FIRST copy back to the team's version (the first,
    // because the map-level `materialized_sha` mirrors placement 0 — the exact document shape the
    // deleted inference misread as a landed exit). Named by its recorded spelling.
    let recorded_first = doc::read_map(&rig.fs, &rig.layout().published(&sid(&id)).map)
        .unwrap()
        .unwrap()
        .placements[0]
        .clone();
    let sel = ops::Selection::one(None, Some(&recorded_first));
    ops::reset(
        &rig.ctx(&plane, &foll),
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &sel,
    )
    .unwrap();
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the named copy is reset to theirs"
    );
    assert_eq!(
        snapshot(&replica),
        Some(expect(mine)),
        "the copy nobody named keeps the draft"
    );
    assert!(
        rig.conflict_exists(&id),
        "a copy still holds the merge, so the record — the publish guard — must stand"
    );
    assert!(copy.exists(), "and the workbench with it");

    // The `--keep-mine` the receipt names FINISHES the stopped merge: the surviving copy's side
    // wins the collided lines, every managed folder converges on the resolution, and only that
    // real conclusion takes the record and the workbench with it.
    let escaped = pull_data(&rig.ctx(&plane, &foll), keep_mine_scope(name)).unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert_eq!(snapshot(&replica), Some(expect(KEPT_OVER_V1)));
    assert!(
        !rig.conflict_exists(&id),
        "publish unblocks only with the real conclusion"
    );
    assert!(!copy.exists());
}

/// **After a narrowed reset, the block's re-disclosure tells PER-FOLDER truth — and the reset's own
/// receipt says the merge is still stopped.** Both surfaces used to speak for the whole set from one
/// fact they never checked: the row promised "your agents are unaffected — N folders still hold your
/// version" over a set that no longer did, and the reset receipt said what it took and stopped,
/// never saying whether the decision was over. This drives the REAL state — a `--dest` reset of one
/// copy of three, with a third edited on afterwards — so every claim is measured against disk.
#[test]
fn a_narrowed_reset_leaves_per_folder_truth_on_both_surfaces() {
    let rig = Rig::new("narrow-reset-truth");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The stop, then two more managed copies of the same un-merged draft.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    let untouched = rig.work.0.join("untouched");
    let worked_on = rig.work.0.join("worked-on");
    add_replica(&rig, &id, &untouched, mine);
    add_replica(&rig, &id, &worked_on, mine);

    // Reset the FIRST copy only, and keep working in the third while the merge stands.
    let recorded_first = doc::read_map(&rig.fs, &rig.layout().published(&sid(&id)).map)
        .unwrap()
        .unwrap()
        .placements[0]
        .clone();
    let outcome = ops::reset(
        &rig.ctx(&plane, &foll),
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::one(None, Some(&recorded_first)),
    )
    .unwrap();
    let ops::ResetOutcome::Applied(items) = outcome else {
        panic!("the `--yes` arm applies");
    };
    // THE RESET RECEIPT (D2): the merge it met is still standing, and the receipt says so and
    // points at the surface that answers about it.
    assert_eq!(
        items[0].merge,
        Some(topos_types::results::ResetMergeOutcome::StillStopped),
        "two copies still hold the merge"
    );
    let receipt = crate::render::reset_applied_tty(&items);
    assert!(
        receipt.ends_with(&format!(
            "  the merge on '{name}' is still stopped (see: topos list {name})"
        )),
        "{receipt}"
    );
    write_tree(
        &worked_on,
        &[
            ("SKILL.md", FileMode::Regular, b"# mine, again\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
        ],
    );

    // THE RE-DISCLOSURE (D1): three folders, three different answers, each read off the folder.
    use topos_types::results::ConflictHolds;
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    let real = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    };
    let mut held: Vec<(String, ConflictHolds)> = row
        .merge
        .as_ref()
        .expect("a merge report")
        .placements
        .iter()
        .map(|p| (real(std::path::Path::new(&p.dir)), p.holds))
        .collect();
    held.sort_by(|a, b| a.0.cmp(&b.0));
    let mut want = vec![
        (real(&rig.placement()), ConflictHolds::Theirs),
        (real(&untouched), ConflictHolds::Yours),
        (real(&worked_on), ConflictHolds::NewerEdits),
    ];
    want.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(held, want, "{:?}", row.merge);

    // The `--json` half of the same row offers the same two exits, spelled for the row's own
    // scope (this is the machine's store, so `-g`). The TTY named them and the envelope did not.
    let argv = |exit: &str| {
        ["topos", "update", "-g", name.as_str(), exit, "--json"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        crate::render::conflict_next_actions(&data)
            .into_iter()
            .map(|a| a.argv)
            .collect::<Vec<_>>(),
        vec![argv("--keep-mine"), argv("--reset")]
    );

    // And the receipt says the three of them one at a time — never the aggregate sentence, which
    // is false of two of these folders.
    let tty = crate::render::pull_tty(&data, &[], &[], &[], &[], 0, 0);
    assert!(
        !tty.contains("your agents are unaffected"),
        "the aggregate promise is false here: {tty}"
    );
    for (dir, said) in [
        (rig.placement(), "holds the team's version"),
        (untouched.clone(), "still holds your version"),
        (worked_on.clone(), "holds your newer edits"),
    ] {
        let leaf = dir.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            tty.lines()
                .any(|l| l.starts_with("      ") && l.contains(&leaf) && l.ends_with(said)),
            "{leaf} → {said}: {tty}"
        );
    }

    // The reset that DOES end it — every remaining copy settled — says so instead, and names no
    // pointer: there is nothing left to decide.
    let ops::ResetOutcome::Applied(last) = ops::reset(
        &rig.ctx(&plane, &foll),
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap() else {
        panic!("the `--yes` arm applies");
    };
    assert_eq!(
        last[0].merge,
        Some(topos_types::results::ResetMergeOutcome::Concluded)
    );
    assert!(!rig.conflict_exists(&id), "the record is gone with it");
    assert!(
        crate::render::reset_applied_tty(&last).ends_with(
            "  that was the last copy holding the merge — the team's version stands everywhere"
        ),
        "{}",
        crate::render::reset_applied_tty(&last)
    );
}

/// `update --reset` in the RECORDED-conflict state resolves it the team's way: the author's draft is
/// snapshotted and discarded, theirs lands on the placement, and the conflict record is CLEARED —
/// with the marked-up copy it named — so publish is not left refused by a divergence that no longer
/// exists, and the next sweep reads the skill current instead of re-disclosing a stale block.
#[test]
fn reset_clears_the_recorded_conflict_block() {
    let rig = Rig::new("reset-clears");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Auto sweep → conflict (overlapping SKILL.md) → blocked.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    assert!(rig.conflict_exists(&id));
    let copy = rig.conflict_copy(&id);
    assert!(copy.exists(), "the marked-up copy is written");

    // The loss-led discard (`--yes`): theirs restored, the block gone, the copy gone with it.
    ops::reset(
        &rig.ctx(&plane, &foll),
        &[name],
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert!(!rig.conflict_exists(&id), "the reset clears the block");
    assert!(!copy.exists(), "and the marked-up copy with it");
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the placement holds the team's bytes after the reset"
    );

    // The next sweep reads the skill current — never a re-disclosed stale conflict.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::UpToDate
    );
}

/// Unrelated histories (no renderable base) fall back to a 2-way manual choice — never a silent merge:
/// MINE is kept on disk, a 2-way diff is disclosed, and publish is blocked until the author resolves.
///
/// The workbench is a WORKBENCH here too: it holds this person's files WITH the team's beside them as
/// `.topos-theirs` siblings, so the by-hand merge the row asks for can actually be done in one folder.
/// Those siblings are topos's own scaffolding — they never become published bundle content — and the
/// exit, `diff`, and `--reset` all work afterwards.
#[test]
fn no_base_falls_back_to_two_way_never_silent() {
    let rig = Rig::new("nobase");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# independent\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    // Sever the recorded base so it cannot be rendered (an unrelated/pruned-base history).
    rig.patch_lock(&id, |l| {
        l.base_commit = "f".repeat(64);
        l.bundle_digest = "e".repeat(64);
    });

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(!mr.clean);
    assert!(mr.drop_diff.is_some(), "a 2-way diff is disclosed");
    // MINE is never silently overwritten by theirs.
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    assert!(rig.conflict_exists(&id));

    // BOTH SIDES ARE IN THE WORKBENCH: this person's files at their own paths, the team's beside
    // them. Without them the folder invites a merge while holding only one of the two things to
    // merge.
    let copy = rig.conflict_copy(&id);
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        b"# independent\n"
    );
    assert_eq!(
        std::fs::read(copy.join("SKILL.md.topos-theirs")).unwrap(),
        V1[0].2,
        "the team's version is beside this person's"
    );
    assert!(
        copy.join("ref/notes.md.topos-theirs").exists(),
        "every file of theirs, including ones this person does not have"
    );
    // And nothing of the sort reached a folder an agent reads.
    assert!(!rig.placement().join("SKILL.md.topos-theirs").exists());

    // THE EXIT: a hand merge in that folder commits, and topos's own siblings are stripped out of
    // it — a `.topos-theirs` file must never become published bundle content.
    let resolved: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# reconciled\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
        ("SKILL.md.topos-theirs", FileMode::Regular, V1[0].2),
    ];
    write_tree(&copy, resolved);
    let escaped = only(
        &pull_data(
            &rig.ctx(&plane, &foll),
            ops::PullScope::One {
                store: ops::StoreScope::Here,
                name: name.clone(),
                workspace: None,
                mode: ops::TargetMode::KeepMine,
            },
        )
        .unwrap(),
    )
    .clone();
    assert_eq!(escaped.action, PullAction::Merged);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[
            ("SKILL.md", FileMode::Regular, b"# reconciled\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
        ])),
        "the sibling is scaffolding, never content"
    );
    assert!(!rig.conflict_exists(&id));
    assert!(!copy.exists());

    // AFTERWARDS, the ordinary verbs work: the base renders (it is the team's version now), so
    // `diff` answers and `--reset` restores it.
    let diffed = ops::diff(
        &rig.ctx(&plane, &foll),
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .expect("diff reads the draft against a renderable base");
    assert!(diffed.diff.contains("reconciled"), "{}", diffed.diff);
    ops::reset(
        &rig.ctx(&plane, &foll),
        &[name],
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .expect("the reset restores the team's version");
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
}

/// Structural author-only: the merge code is unreachable from a clean follower state. A behind-clean pull
/// fast-forwards (never merges); a draft with no pending update is a no-op (never merges); neither writes
/// a conflict record nor produces a `Merged`/`Conflicted` outcome.
#[test]
fn merge_unreachable_from_clean_follower_states() {
    // BEHIND (clean): no local edit; a pending update fast-forwards, it does NOT enter the merge.
    {
        let rig = Rig::new("reach-behind");
        let (id, _name, genesis) = rig.adopt(BASE); // placement == base (no edit)
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        let row =
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
        assert_eq!(row.action, PullAction::FastForwarded);
        assert!(row.merge.is_none());
        assert!(!rig.conflict_exists(&id));
    }
    // DRAFT (no pending): a local edit but `current` unchanged → a no-op; never the merge.
    {
        let rig = Rig::new("reach-draft");
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(
            &rig.placement(),
            &[("SKILL.md", FileMode::Regular, b"# draft\n")],
        );
        let v0 = mk_version(&[genesis], BASE, "d_pub", "v0"); // not used as a move
        let _ = v0;
        let mut plane = FixturePlane::default();
        // `current` is the genesis the client already has applied → nothing pending.
        plane.set_current(&id, served(WS, &id, genesis, 0));
        let foll = follow(&id);
        let row =
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
        assert!(
            matches!(row.action, PullAction::UpToDate),
            "a draft with no pending update is a no-op, got {:?}",
            row.action
        );
        assert!(row.merge.is_none());
        assert!(!rig.conflict_exists(&id));
    }
}

/// A binary (non-UTF-8) file diverging three ways is never line-merged: theirs is kept at the path
/// and mine in a `.topos-mine` sibling — both inside the CONFLICT COPY, never in a placement, so a
/// `.topos-mine` file can never become publishable bundle content (`publish` ships a placement's
/// bytes). The copy scans back to the recorded conflict digest (the sidecar round-trips through the
/// scanner — the untouched signal stays valid).
#[test]
fn binary_conflict_keeps_both_sides_via_sidecar() {
    let rig = Rig::new("binary");
    // 0xFF is never a valid UTF-8 lead byte → genuinely binary content (so it is never line-merged).
    let base: &[(&str, FileMode, &[u8])] = &[("logo.bin", FileMode::Regular, &[0xffu8, 1, 2])];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: &[(&str, FileMode, &[u8])] = &[("logo.bin", FileMode::Regular, &[0xffu8, 9, 9])];
    write_tree(&rig.placement(), mine);
    let theirs_files: &[(&str, FileMode, &[u8])] =
        &[("logo.bin", FileMode::Regular, &[0xffu8, 7, 7])];
    let v1 = mk_version(&[genesis], theirs_files, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::Conflicted);
    // The placement keeps MINE whole — no `.topos-mine` sibling, nothing of theirs. That is what
    // makes the sibling unpublishable: `publish` ships a PLACEMENT's bytes.
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    // theirs kept at the path, mine in the sidecar — both inside the conflict copy.
    let copy = rig.conflict_copy(&id);
    assert_eq!(
        std::fs::read(copy.join("logo.bin")).unwrap(),
        &[0xffu8, 7, 7]
    );
    assert_eq!(
        std::fs::read(copy.join("logo.bin.topos-mine")).unwrap(),
        &[0xffu8, 9, 9]
    );
    // The copy scans back to the recorded conflict digest (sidecars survive the scanner).
    let cs: topos_types::persisted::ConflictState =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid(&id)).conflict)
            .unwrap()
            .unwrap();
    let scanned = crate::scan::scan(&copy).unwrap();
    assert_eq!(to_hex(&scanned.bundle_digest), cs.conflicted_digest);
}

/// The release-blocker crash gate: fault every fs op during an auto conflict resolve and assert (a)
/// the agent-readable placement holds the author's OWN complete bytes at EVERY fault — never
/// markers, never a torn tree, never theirs (the guarantee is structural: the conflict path writes
/// no placement at all, so no crash window can put dangerous bytes in a folder an agent reads); (b)
/// a marked-up copy on disk always has its guard record beside it (a marker tree is never
/// publishable); and (c) a clean re-run always converges to the blocked conflict state, with the
/// complete marker tree in the sidecar copy.
#[test]
fn resolve_crash_gate_converges_and_never_writes_markers_into_an_agent_folder() {
    let mine: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    // Capture the completed conflict copy + op count from a clean run.
    let (conflict_tree, n_ops) = {
        let rig = Rig::new("cg-count");
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), mine);
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        let fs = FaultFs::new(0);
        pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed).unwrap();
        (snapshot(&rig.conflict_copy(&id)), fs.ops_attempted())
    };
    assert!(n_ops > 4, "expected several durable ops, got {n_ops}");
    assert!(conflict_tree.is_some(), "the clean run writes the copy");

    for fail_at in 1..=n_ops {
        let rig = Rig::new(&format!("cg-{fail_at}"));
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), mine);
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        let copy_root = rig.layout().home().join("conflicts").join("pr-describe");

        // Fault the Nth op (may error mid-resolve).
        let fs = FaultFs::new(fail_at);
        let _ = pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed);

        // THE SAFETY PROPERTY: whatever the fault did, the folder an agent reads still holds the
        // author's own complete bytes.
        assert_eq!(
            snapshot(&rig.placement()),
            Some(expect(mine)),
            "fail_at={fail_at}: the agent folder must never hold anything but the author's version"
        );
        // A marked-up copy on disk always has its guard record: the record is written + fsynced
        // before the copy, so this holds at every fault.
        if copy_root.exists() {
            assert!(
                rig.conflict_exists(&id),
                "fail_at={fail_at}: a conflict copy exists without its guard record"
            );
        }

        // A clean re-run converges: blocked conflict, the complete marker tree in the copy,
        // applied == observed — and the agent folder still untouched.
        let row =
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
        assert_eq!(
            row.action,
            PullAction::Conflicted,
            "fail_at={fail_at}: did not converge to a blocked conflict"
        );
        assert!(
            rig.conflict_exists(&id),
            "fail_at={fail_at}: no guard after converge"
        );
        assert_eq!(
            snapshot(&rig.conflict_copy(&id)),
            conflict_tree,
            "fail_at={fail_at}: the conflict copy did not converge to the complete marker tree"
        );
        assert_eq!(
            snapshot(&rig.placement()),
            Some(expect(mine)),
            "fail_at={fail_at}: converging must not touch the agent folder either"
        );
        assert_eq!(rig.read_sync(&id).applied, 1);
    }
}

/// The same gate over the way OUT. The escape commits its resolution, marks the record CONCLUDED,
/// converges the placements, and clears the record last — so a crash mid-exit leaves either a
/// live block (unmarked, nothing placed) or a marked conclusion to finish, and the workbench
/// folder holding a by-hand merge is the only copy of bytes nothing else in the system has
/// written down. Faulting every fs op of `--keep-mine`, both exits, asserts:
///
/// (a) the agent folder always holds ONE complete bundle — the author's own version or the
///     committed resolution, never markers, never a torn tree;
/// (b) while the record still stands — marked or not — the workbench is byte-identical to how
///     the person left it (the clear removes the record FIRST, the folder after — the order that
///     makes a re-run safe);
/// (c) a clean re-run FINISHES a standing record to `Merged`, whether the crash left it unmarked
///     (a live block, escaped afresh) or marked (the crashed exit's own conclusion, finished
///     idempotently — where the deleted document-pair inference refused `NoStoppedMerge` with the
///     record still standing); `NoStoppedMerge` remains only for faults past the record's
///     removal, where the merge had already concluded whole — and the placement holds the
///     expected resolution either way.
#[test]
fn escape_crash_gate_keeps_one_coherent_bundle_and_never_eats_the_hand_merge() {
    let mine: FileSet = MINE_OVER_BASE;
    let hand: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    // Both exits: an UNTOUCHED workbench (keep my version) and a hand-merged one.
    for by_hand in [false, true] {
        let want: FileSet = if by_hand { hand } else { KEPT_OVER_V1 };
        // A clean run first, to size the fault sweep.
        let n_ops = {
            let rig = Rig::new(&format!("eg-count-{by_hand}"));
            let (id, name, genesis) = rig.adopt(BASE);
            write_tree(&rig.placement(), mine);
            let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
            let mut plane = FixturePlane::default();
            plane.add_version(&id, &v1);
            plane.set_current(&id, served(WS, &id, v1.id, 1));
            let foll = follow(&id);
            pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
            if by_hand {
                write_tree(&rig.conflict_copy(&id), hand);
            }
            let fs = FaultFs::new(0);
            pull_data(
                &rig.ctx_fs(&fs, &plane, &foll),
                keep_mine_scope(name.clone()),
            )
            .unwrap();
            fs.ops_attempted()
        };
        assert!(n_ops > 4, "expected several durable ops, got {n_ops}");

        for fail_at in 1..=n_ops {
            let rig = Rig::new(&format!("eg-{by_hand}-{fail_at}"));
            let (id, name, genesis) = rig.adopt(BASE);
            write_tree(&rig.placement(), mine);
            let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
            let mut plane = FixturePlane::default();
            plane.add_version(&id, &v1);
            plane.set_current(&id, served(WS, &id, v1.id, 1));
            let foll = follow(&id);
            pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
            let copy = rig.conflict_copy(&id);
            if by_hand {
                write_tree(&copy, hand);
            }
            let untouched = snapshot(&copy);

            // Fault the Nth op (may error mid-exit).
            let fs = FaultFs::new(fail_at);
            let _ = pull_data(
                &rig.ctx_fs(&fs, &plane, &foll),
                keep_mine_scope(name.clone()),
            );

            // (a) whatever the fault did, the folder an agent reads holds one complete bundle.
            let on_disk = snapshot(&rig.placement());
            assert!(
                on_disk == Some(expect(mine)) || on_disk == Some(expect(want)),
                "by_hand={by_hand} fail_at={fail_at}: the agent folder holds neither the author's \
                 own version nor the resolution: {on_disk:?}"
            );
            // (b) the record still stands ⇒ the workbench is exactly as the person left it.
            if rig.conflict_exists(&id) {
                assert_eq!(
                    snapshot(&copy),
                    untouched,
                    "by_hand={by_hand} fail_at={fail_at}: the workbench moved while the block stood"
                );
            }

            // (c) a clean re-run finishes it. A STANDING record — the live unmarked block, or the
            // marked conclusion a crash stranded — must re-run to `Merged`; the refusal is honest
            // only once the record's removal already landed (the merge concluded whole, and the
            // fault hit the workbench cleanup after it).
            let record_stood = rig.conflict_exists(&id);
            match pull_data(&rig.ctx(&plane, &foll), keep_mine_scope(name.clone())) {
                Ok(d) => assert_eq!(
                    only(&d).action,
                    PullAction::Merged,
                    "by_hand={by_hand} fail_at={fail_at}"
                ),
                Err(crate::error::ClientError::NoStoppedMerge { .. }) => assert!(
                    !record_stood,
                    "by_hand={by_hand} fail_at={fail_at}: a standing record must re-run to Merged, \
                     never be refused away"
                ),
                Err(e) => panic!("by_hand={by_hand} fail_at={fail_at}: {e:?}"),
            }
            assert!(
                !rig.conflict_exists(&id),
                "by_hand={by_hand} fail_at={fail_at}: the block outlived its resolution"
            );
            assert_eq!(
                snapshot(&rig.placement()),
                Some(expect(want)),
                "by_hand={by_hand} fail_at={fail_at}: did not converge to the resolution"
            );
        }
    }
}

/// The `--reset` half of the same gate: this exit DISCARDS, so what it must never do under a fault
/// is leave a folder holding half of anything, or take the workbench with it before the record.
/// The reset marks the record CONCLUDED only once every copy has proven settled, so a standing
/// record after a fault is either still unmarked (a live block, whatever the placements hold) or a
/// marked conclusion whose clear the next run finishes — and in BOTH shapes the workbench must be
/// intact (the removal runs through the record-first clear, which spares an edited folder). A clean
/// re-run always converges on the team's version with no block and no workbench left.
#[test]
fn reset_crash_gate_converges_and_never_takes_the_workbench_before_the_record() {
    let mine: FileSet = MINE_OVER_BASE;
    let n_ops = {
        let rig = Rig::new("rg-count");
        let (id, name, genesis) = reset_conflict_rig(&rig, mine);
        let (mut plane, foll) = (FixturePlane::default(), follow(&id));
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let fs = FaultFs::new(0);
        ops::reset(
            &rig.ctx_fs(&fs, &plane, &foll),
            &[name],
            true,
            ops::StoreScope::Here,
            &ops::Selection::default(),
        )
        .unwrap();
        fs.ops_attempted()
    };
    assert!(n_ops > 4, "expected several durable ops, got {n_ops}");

    for fail_at in 1..=n_ops {
        let rig = Rig::new(&format!("rg-{fail_at}"));
        let (id, name, genesis) = reset_conflict_rig(&rig, mine);
        let (mut plane, foll) = (FixturePlane::default(), follow(&id));
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let copy = rig.conflict_copy(&id);
        let marked = snapshot(&copy);

        let fs = FaultFs::new(fail_at);
        let _ = ops::reset(
            &rig.ctx_fs(&fs, &plane, &foll),
            std::slice::from_ref(&name),
            true,
            ops::StoreScope::Here,
            &ops::Selection::default(),
        );

        // One complete bundle in the agent folder at every fault: the author's draft, or the
        // team's version the reset restores.
        let on_disk = snapshot(&rig.placement());
        assert!(
            on_disk == Some(expect(mine)) || on_disk == Some(expect(V1)),
            "fail_at={fail_at}: the agent folder holds neither side whole: {on_disk:?}"
        );
        // The record goes FIRST, so a standing record — marked concluded or not — means the
        // workbench is still intact.
        if rig.conflict_exists(&id) {
            assert_eq!(
                snapshot(&copy),
                marked,
                "fail_at={fail_at}: the workbench went before the record it belongs to"
            );
        }

        // A clean re-run converges: the team's version, no block, no workbench.
        ops::reset(
            &rig.ctx(&plane, &foll),
            std::slice::from_ref(&name),
            true,
            ops::StoreScope::Here,
            &ops::Selection::default(),
        )
        .unwrap_or_else(|e| panic!("fail_at={fail_at}: the re-run must converge: {e:?}"));
        assert_eq!(
            snapshot(&rig.placement()),
            Some(expect(V1)),
            "fail_at={fail_at}"
        );
        assert!(!rig.conflict_exists(&id), "fail_at={fail_at}");
        // A crash BETWEEN the two removals leaves the folder standing — documented litter, never
        // loss, and the next conflict for this bundle simply takes the next free name. What must
        // hold is that the residue is topos's OWN partial copy of the marked-up tree and nothing
        // else: no byte a person wrote can end up stranded there.
        if let Some(left) = snapshot(&copy) {
            let full = marked
                .clone()
                .expect("the marked-up copy is written at conflict time");
            for (path, bytes) in &left {
                assert!(
                    full.iter().any(|(p, b)| p == path && b == bytes),
                    "fail_at={fail_at}: the leftover holds something topos never wrote: {path}"
                );
            }
        }
    }
}

/// A blocked bundle, ready for a reset: adopt, edit, and let the sweep stop on the overlap.
fn reset_conflict_rig(rig: &Rig, mine: FileSet) -> (String, String, [u8; 32]) {
    let (id, name, genesis) = rig.adopt(BASE);
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    (id, name, genesis)
}
