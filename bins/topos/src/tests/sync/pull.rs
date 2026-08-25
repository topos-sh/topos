//! The pull/apply engine: fast-forward, first receive, go-back/resume, the crash-after-swap
//! heal, wire-level refusals, the sweep's plane-down circuit breaker, and the per-op
//! durability bound.

use std::path::{Path, PathBuf};

use topos_core::digest::{FileMode, to_hex};
use topos_types::results::PullAction;

use crate::fs_seam::{FsOps, RealFs};
use crate::ops;
use crate::plane::{
    FollowContext, InertFollow, InertPlane, KnownCurrent, PlaneError, PlaneSource, PointerFetch,
};

use super::rig::*;

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

#[test]
fn clean_follower_auto_fast_forwards() {
    let rig = Rig::new("ff");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();

    let row = only(&data);
    assert_eq!(row.action, PullAction::FastForwarded);
    assert_eq!(row.applied, 1);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "new bytes placed"
    );
    // The executable bit is part of the consent-bound digest and must survive.
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(rig.placement().join("run.sh"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
    let s = rig.read_sync(&id);
    assert_eq!(s.applied, s.observed);
    assert_eq!(s.base_commit, to_hex(&v1.id));
}

/// **A manifest row IS the consent.** A bundle this machine's recipe demands but has never
/// received places its first bytes on the BARE sweep — no offer, no second command — and the row
/// reads `installed`, naming the folder it landed in. The next bare sweep then has nothing to do.
/// THE SERVER'S CURRENT IS THE TRUTH for `diff` too. A revert run elsewhere moves `current`
/// without touching this store's base, and a bare diff that compared the copy against the cached
/// base answered "no changes" over a copy that differs from what the team runs. The left side is
/// the LIVE current, read from the plane; its id is what the answer names.
#[test]
fn a_bare_diff_reads_against_the_live_current_not_the_cached_base() {
    let rig = Rig::new("diff-live");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);

    // The copy is untouched; against the base it applied, there is nothing to show.
    let same = ops::diff(
        &rig.ctx(&plane, &foll),
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    assert!(same.diff.is_empty(), "{}", same.diff);
    assert_eq!(same.version_id, to_hex(&v1.id));

    // A revert (run from another machine) moves `current` back onto the genesis bytes. No sweep
    // has run here since — the cached base still says v1.
    let restored = mk_version(&[v1.id], BASE, "d_rev", "topos: revert");
    plane.add_version(&id, &restored);
    plane.set_current(&id, served(WS, &id, restored.id, 2));
    let diffed = ops::diff(
        &rig.ctx(&plane, &foll),
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    assert_eq!(
        diffed.version_id,
        to_hex(&restored.id),
        "the left side is the live current"
    );
    assert!(
        diffed.diff.contains("+# v1") && diffed.diff.contains("ref/notes.md"),
        "the real difference against what the team runs: {}",
        diffed.diff
    );
    assert_eq!(
        rig.read_sync(&id).base_commit,
        to_hex(&v1.id),
        "a read verb moved nothing"
    );
}

/// **A RESET PREVIEW IS MEASURED AGAINST THE VERSION IT RESTORES.** `--reset` re-materializes the
/// version this copy applied (`lock.base_commit`), and the describe's header names it. The body
/// under that header used to be the bare `diff` read — the copy against the workspace's LIVE
/// current — so the moment the two came apart (a revert landed on the server, a lock pins an
/// older version) the preview listed changes `--yes` would never make, directly beneath a header
/// naming the version it does land. `topos diff` is untouched: measuring YOUR change against what
/// the team runs now is exactly what that verb is for.
#[test]
fn a_reset_preview_reads_against_the_version_it_restores_not_the_live_current() {
    let rig = Rig::new("reset-preview-base");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::FastForwarded
    );

    // A revert run elsewhere puts the genesis bytes back as `current`. No sweep has run here
    // since, so this copy still HOLDS v1 — and a reset here restores v1, not the revert.
    let restored = mk_version(&[v1.id], BASE, "d_rev", "topos: revert");
    plane.add_version(&id, &restored);
    plane.set_current(&id, served(WS, &id, restored.id, 2));

    // …and the copy is edited: one file rewritten over v1, the rest of v1 left alone.
    write_tree(
        &rig.placement(),
        &[
            ("SKILL.md", FileMode::Regular, b"# mine\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
            ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
        ],
    );

    let ctx = rig.ctx(&plane, &foll);
    let ops::ResetOutcome::Described { items, .. } = ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        false,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap() else {
        panic!("without `--yes` the reset describes");
    };
    assert_eq!(items[0].to_version, to_hex(&v1.id), "v1 is what lands");
    let body = &items[0].drop_diff;
    assert!(
        body.contains("-# mine") && body.contains("+# v1"),
        "the edit goes away and v1's line comes back: {body}"
    );
    assert!(
        !body.contains("ref/notes.md") && !body.contains("echo v0"),
        "the live current's bytes are not what `--yes` puts back: {body}"
    );

    // `topos diff` over the very same state still reads against the live current.
    let d = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    assert_eq!(
        d.version_id,
        to_hex(&restored.id),
        "the server's current is the truth for `diff`"
    );
    assert!(
        d.diff.contains("ref/notes.md"),
        "the real difference against what the team runs: {}",
        d.diff
    );
}

/// By the merge model an unmodified copy fast-forwards, and it does so even when the served
/// current UNDOES the version it holds (a revert restored older bytes). What the receipt then owes
/// is the disclosure: which version it replaced, and the one command that brings it back. An
/// ordinary forward move — a version that builds on the one held — says nothing of the kind.
#[test]
fn a_fast_forward_that_undoes_the_held_version_says_so_and_names_the_way_back() {
    let rig = Rig::new("ff-undone");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    // The ordinary move: v1 builds on the genesis bytes — a plain fast-forward, no note.
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::FastForwarded);
    assert_eq!(row.note, None, "{row:?}");

    // A revert restores the genesis bytes on top of v1: the copy (= v1, unmodified) is replaced,
    // and the receipt says with what, and how v1 comes back.
    let restored = mk_version(&[v1.id], BASE, "d_rev", "topos: revert");
    plane.add_version(&id, &restored);
    plane.set_current(&id, served(WS, &id, restored.id, 2));
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(row.action, PullAction::FastForwarded);
    // The way back is spelled SHORT — the way every listing prints a version, and the form
    // `revert --to` resolves against the workspace's history in the scope it stands in.
    let full = to_hex(&v1.id);
    let (was, now) = (&full[..12], &to_hex(&restored.id)[..12]);
    assert_eq!(
        row.note.as_deref(),
        Some(
            format!(
                "replaced your copy (= version {was}) with current {now} — {was} stays in \
                 history: topos revert {name} --to {was}"
            )
            .as_str()
        ),
        "{row:?}"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(BASE)));
    // The receipt prints the fact under the row.
    let tty = crate::render::pull_tty(&data, &[], &[], &[], &[], 0, 0);
    assert!(
        tty.contains(&format!("replaced your copy (= version {was})")),
        "{tty}"
    );
}

#[test]
fn a_never_received_bundle_installs_on_the_bare_sweep() {
    let rig = Rig::new("first-receive");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    // Roll the sidecar back to the never-received baseline a brand-new arrival gets: nothing
    // applied, the all-zero base, and no bytes on disk yet.
    std::fs::remove_dir_all(rig.placement()).unwrap();
    rig.patch_sync(&id, |s| {
        s.observed = 0;
        s.observed_version_id = zero_hex();
        s.applied = 0;
        s.base_commit = zero_hex();
        s.work_hash = zero_hex();
    });
    rig.patch_lock(&id, |l| {
        l.base_commit = zero_hex();
        l.bundle_digest = zero_hex();
    });

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // ONE bare sweep. The bytes are on disk when it returns.
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::Installed,
        "a first receive installs on the bare sweep"
    );
    assert_eq!(row.applied, 1);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the first bytes landed without a second command"
    );
    assert_eq!(
        row.destinations.len(),
        1,
        "an installed row names where the bytes went: {:?}",
        row.destinations
    );
    assert!(
        row.destinations[0].ends_with("pr-describe"),
        "{:?}",
        row.destinations
    );
    assert_eq!(rig.read_sync(&id).applied, 1);

    // Nothing is left waiting on a person: the next bare sweep has nothing to do.
    let again = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&again).action, PullAction::UpToDate);
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
}

/// The in-memory merge PREVIEW over an OVERLAPPING edit predicts the conflict and names the
/// conflicting path, while running nothing: no markers on disk, no conflict record. (The preview
/// is what `publish`'s describe shows an author whose copy is behind `current`.)
#[test]
fn the_merge_preview_names_a_conflicting_path_without_running_the_merge() {
    let rig = Rig::new("preview");
    let (id, _name, _genesis) = rig.adopt(BASE);
    // The same overlap the auto-sweep conflict test uses: SKILL.md edited on both sides.
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my local edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);
    let mine = crate::scan::scan(&rig.placement()).unwrap();

    let preview = ops::merge_resolve::preview_merge(&rendered(BASE), &mine, &rendered(V1));
    assert_eq!(
        preview.verdict,
        topos_types::results::MergePreviewVerdict::Conflicted
    );
    assert_eq!(preview.conflicts, vec!["SKILL.md".to_owned()]);
    // Predicted, never run: the placement still holds the author's bytes (no markers) and no
    // conflict record exists.
    let skill = std::fs::read_to_string(rig.placement().join("SKILL.md")).unwrap();
    assert!(!skill.contains("<<<<<<<"), "{skill}");
    assert!(!rig.conflict_exists(&id));
}

/// **A folder an agent reads always holds one coherent, complete bundle.** An AUTO follower's bare
/// sweep RESOLVES a diverged draft; here the local edit overlaps theirs' edit to `SKILL.md`, so the
/// merge conflicts — and the conflict writes NOTHING into the agent-readable placement. The folder
/// keeps the author's own version, byte for byte, exactly as it stood before the update. The
/// complete marked-up tree (markers carrying BOTH sides, the other files merged clean) goes to the
/// scope's own workbench, `~/.topos/conflicts/<name>/`, where a person resolves it by hand.
#[test]
fn a_conflict_marks_up_a_sidecar_copy_and_leaves_every_agent_folder_alone() {
    let rig = Rig::new("diverge");
    let (id, _name, genesis) = rig.adopt(BASE);
    // Edit SKILL.md (overlaps theirs' SKILL.md edit → a conflict) and leave run.sh at base.
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my local edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);
    let before = snapshot(&rig.placement());

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);

    // Resolved (not merely surfaced): a conflict, with a merge report listing the conflicting path.
    assert_eq!(row.action, PullAction::Conflicted);
    let mr = row.merge.as_ref().expect("a merge report");
    assert!(!mr.clean);
    assert_eq!(mr.theirs_version_id, to_hex(&v1.id));
    assert!(mr.conflicts.iter().any(|c| c.path == "SKILL.md"));

    // THE POINT: the placement is byte-identical to what stood there before the update — the
    // author's own version, no markers, no half-state, nothing an agent could read and act on.
    assert_eq!(
        snapshot(&rig.placement()),
        before,
        "a conflict must not write into a folder an agent reads"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(edited)));

    // The COMPLETE conflict tree is in the sidecar, at the path the record names: SKILL.md has
    // diff3 markers carrying BOTH sides; the non-overlapping files are merged clean (run.sh →
    // theirs, the new ref/notes.md → theirs).
    let copy = rig.conflict_copy(&id);
    assert_eq!(
        copy,
        rig.layout().home().join("conflicts").join("pr-describe"),
        "the copy is keyed by the bundle NAME, under the scope's own store"
    );
    let skill = std::fs::read_to_string(copy.join("SKILL.md")).unwrap();
    assert!(
        skill.contains("<<<<<<<") && skill.contains(">>>>>>>"),
        "{skill}"
    );
    assert!(
        skill.contains("my local edit") && skill.contains("# v1"),
        "the edit must survive inside the markers: {skill}"
    );
    assert_eq!(
        std::fs::read(copy.join("run.sh")).unwrap(),
        b"#!/bin/sh\necho v1\n"
    );
    assert!(copy.join("ref/notes.md").exists());
    // The row carries both halves of that truth, so the receipt can name them without re-deriving
    // either: the folder that still holds the author's version, and the folder the record names.
    let real = |p: &std::path::Path| {
        std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .display()
            .to_string()
    };
    assert_eq!(
        mr.placements
            .iter()
            .map(|p| (real(std::path::Path::new(&p.dir)), p.holds))
            .collect::<Vec<_>>(),
        vec![(
            real(&rig.placement()),
            topos_types::results::ConflictHolds::Yours
        )],
        "the untouched placement is named on the row, and says what it holds"
    );
    assert_eq!(
        mr.copy_dir
            .as_deref()
            .map(|d| real(std::path::Path::new(d))),
        Some(real(&copy))
    );
    assert_eq!(
        row.scope.as_deref(),
        Some("person"),
        "the machine's own store — its exits are spelled with `-g`"
    );
    // The copy IS the recorded conflict tree — the digest the escape reads as "untouched".
    let scanned = crate::scan::scan(&copy).unwrap();
    assert_eq!(
        to_hex(&scanned.bundle_digest),
        rig.conflict_state(&id).conflicted_digest
    );

    // Never clobbered: the pre-merge draft is snapshotted into the sidecar store (recoverable).
    let draft = mk_version(&[genesis], edited, DEVICE, "topos: draft snapshot");
    let store = topos_gitstore::Store::open(&rig.layout().published(&sid(&id)).store).unwrap();
    assert!(
        store.list_versions().unwrap().contains(&draft.id),
        "the diverged draft must be snapshotted by the merge"
    );

    // A durable conflict record blocks publish; the pending update is consumed into the (blocked)
    // draft, and the lock now names the team's version as the base a `--reset` restores.
    assert!(rig.conflict_exists(&id));
    let s = rig.read_sync(&id);
    assert_eq!(s.applied, 1);
    assert_eq!(s.base_commit, to_hex(&v1.id));
}

/// The blocked row promises the reader that a named folder still holds their version, so it may
/// only name a folder it LOOKED AT. The placement map is what was recorded when the block was
/// raised; a folder deleted since is still in it, and reading the promise off the map alone keeps
/// asserting a directory that is gone. The row already owns the words for holding nothing, and
/// both of its exits put the bytes back.
#[test]
fn a_re_disclosed_block_never_names_a_folder_that_is_no_longer_there() {
    let rig = Rig::new("gone");
    let (id, _name, genesis) = rig.adopt(BASE);
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my local edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // The block is raised, and the folder that holds this person's version is named on it.
    let raised = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&raised);
    assert_eq!(row.action, PullAction::Conflicted);
    assert_eq!(
        row.merge.as_ref().expect("a merge report").placements.len(),
        1
    );

    // The person deletes it while the merge stands. The record is untouched — nothing about the
    // merge changed — but the promise is no longer true.
    std::fs::remove_dir_all(rig.placement()).unwrap();

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "the block itself is unaffected"
    );
    assert!(
        row.merge
            .as_ref()
            .expect("a merge report")
            .placements
            .is_empty(),
        "{:?}",
        row.merge
    );
    let tty = crate::render::pull_tty(&data, &[], &[], &[], &[], 0, 0);
    assert!(
        tty.contains(
            "    no agent folder holds this skill right now — either way out below puts it back\n"
        ),
        "{tty}"
    );
    assert!(!tty.contains("your agents are unaffected"), "{tty}");
}

#[test]
fn go_back_then_resume() {
    let rig = Rig::new("goback");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Fast-forward to v1.
    let ctx = rig.ctx(&plane, &foll);
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    // Go back to genesis: old bytes installed, `held` set, the floor (`observed`) untouched.
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: name.clone(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(genesis)),
        },
    )
    .unwrap();
    assert_eq!(only(&data).action, PullAction::Held);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "old bytes restored"
    );
    let s = rig.read_sync(&id);
    assert!(s.held, "held set");
    assert_eq!(s.observed, 1, "floor NOT lowered");
    assert_eq!(s.applied, 0, "applied dropped to the old gen");

    // A held skill is NOT auto-fast-forwarded by the sweep.
    let ctx = rig.ctx(&plane, &foll);
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "hold suppresses auto-FF"
    );

    // A bare explicit `pull <skill>` resumes (clears the hold) and fast-forwards back to v1.
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "resumed to v1"
    );
    assert!(!rig.read_sync(&id).held);
}

#[test]
fn pull_name_fallback_reaches_a_skill_literally_named_with_a_hex_at_suffix() {
    let rig = Rig::new("atname");
    // Adopt a skill whose NAME looks exactly like a go-back target (a name is a directory basename —
    // only the skill ID charset forbids `@`).
    let dir = rig.work.0.join("docs@abcdef12");
    write_tree(&dir, BASE);
    let inert_p = InertPlane;
    let inert_f = InertFollow;
    let added = ops::add(&rig.ctx(&inert_p, &inert_f), &dir).unwrap();
    assert_eq!(added.name, "docs@abcdef12");

    // The go-back parse tries the pre-@ name (`docs`) first, finds no tracked skill, and retries the
    // WHOLE argument as the name — the skill is reachable, never shadowed by the suffix parse.
    let out = crate::app::pull_with_name_fallback(
        &rig.ctx(&inert_p, &inert_f),
        Some("docs@abcdef12".to_owned()),
        false,
        ops::StoreScope::Here,
    )
    .unwrap();
    assert_eq!(out.data.skills.len(), 1, "the @-named skill resolved");

    // Neither interpretation tracked → the typed NoSuchSkill names the FULL argument the user typed.
    let err = match crate::app::pull_with_name_fallback(
        &rig.ctx(&inert_p, &inert_f),
        Some("nope@abcdef12".to_owned()),
        false,
        ops::StoreScope::Here,
    ) {
        Ok(_) => panic!("an untracked name must not resolve"),
        Err(e) => e,
    };
    assert!(
        matches!(&err, crate::error::ClientError::NoSuchSkill { name } if name == "nope@abcdef12"),
        "got {err:?}"
    );
}

#[test]
fn pull_name_fallback_keeps_the_go_back_primary() {
    // The go-back interpretation still wins when the pre-@ name IS tracked — same shape as
    // `go_back_then_resume`, but driven through the app-level fallback entry point.
    let rig = Rig::new("atgoback");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    let out = crate::app::pull_with_name_fallback(
        &rig.ctx(&plane, &foll),
        Some(format!("{name}@{}", to_hex(&genesis))),
        false,
        ops::StoreScope::Here,
    )
    .unwrap();
    assert_eq!(out.data.skills[0].action, PullAction::Held);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "the go-back landed the old bytes"
    );
}

#[test]
fn go_back_resolves_a_unique_short_prefix_and_refuses_a_no_match() {
    // Same shape as `go_back_then_resume`, but the target rides as a pasted 12-char short form — the
    // exact string every TTY surface renders — resolved against the skill's recorded history.
    let rig = Rig::new("gobackprefix");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let ctx = rig.ctx(&plane, &foll);
    pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: name.clone(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Prefix(to_hex(&genesis)[..12].into())),
        },
    )
    .unwrap();
    assert_eq!(only(&data).action, PullAction::Held);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "the short prefix installed the same bytes the full id would"
    );

    // A prefix matching nothing in the recorded history is the SAME typed error an unknown full id
    // reports — never a fabricated floor, never a silent name fallback.
    let ctx = rig.ctx(&plane, &foll);
    let err = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Prefix("ffffffffffff".into())),
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "UNKNOWN_GOBACK_VERSION");
}

#[test]
fn server_restore_backward_move_applies() {
    // The served record IS the sync target: after a DB restore the plane re-serves an EARLIER
    // (generation, version_id) than the client last observed. That is a legitimate team rollback now — the
    // client silently applies TOWARD it (a clean follower always applies), never refusing it as a downgrade.
    let rig = Rig::new("restore");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(
        &id,
        &Version {
            id: genesis,
            fetched: crate::plane::FetchedVersion {
                parents: Vec::new(),
                author: DEVICE.to_owned(),
                message: "topos: publish".to_owned(),
                files: BASE
                    .iter()
                    .map(|(p, m, b)| crate::plane::FetchedFile {
                        path: (*p).to_owned(),
                        mode: *m,
                        bytes: b.to_vec(),
                    })
                    .collect(),
            },
        },
    );
    // The client had applied v1 @ (1,2); the plane is then restored and re-serves genesis @ (1,1).
    plane.set_current(&id, served(WS, &id, v1.id, 2));
    let foll = follow(&id);
    {
        let ctx = rig.ctx(&plane, &foll);
        pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    }
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));

    // The restore: the served target moves BACKWARD to genesis @ (1,1). The client applies toward it.
    plane.set_current(&id, served(WS, &id, genesis, 1));
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "applied the restored (earlier) target — a legitimate team rollback"
    );
    let s = rig.read_sync(&id);
    assert_eq!(s.observed, 1);
    assert_eq!(s.observed_version_id, to_hex(&genesis));
    assert_eq!(s.applied, s.observed);
}

#[test]
fn mis_scoped_pointer_is_a_wire_error() {
    // A served record scoped to ANOTHER workspace (even for the same skill id) is a malformed response, not
    // the sync target — a targeted pull surfaces it as a wire-validation error, and nothing is applied.
    let rig = Rig::new("xws");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served("w_other", &id, v1.id, 1)); // wrong workspace scope

    let foll = follow(&id);
    let ctx = rig.ctx(&plane, &foll);
    let err = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "CORRUPT_STATE",
        "a mis-scoped record is a wire error"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(BASE)), "untouched");
    assert_eq!(rig.read_sync(&id).observed, 0, "target not advanced");
}

#[test]
fn crash_after_swap_heals_without_false_divergence() {
    // The bytes were swapped to v1 but `applied` never advanced (a crash between the swap and the sync
    // write). The next pull must HEAL forward (advance `applied`), never show a false DIVERGED panel.
    let rig = Rig::new("heal");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");

    // Simulate the post-swap, pre-commit state: placement holds v1 bytes; sync says observed=(1,1) naming
    // v1 but applied still (0,0).
    write_tree(&rig.placement(), V1);
    rig.patch_sync(&id, |s| {
        s.observed = 1;
        s.observed_version_id = to_hex(&v1.id);
        s.applied = 0;
        // base/work still describe genesis (the docs never advanced).
    });

    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::FastForwarded,
        "healed, not merged as a false divergence"
    );
    assert!(row.merge.is_none(), "a heal runs no merge");
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
    assert_eq!(rig.read_sync(&id).applied, 1);
}

#[test]
fn an_accept_applies_a_version_that_moved_during_the_pull() {
    // The plane advances to v2 between the sweep that landed v1 and the targeted accept. The accept
    // applies v2 in that same call: the row demanding the bundle already consented, so a version
    // this call discovered is not held back for a second command.
    let rig = Rig::new("moved");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2files: &[(&str, FileMode, &[u8])] = &[("SKILL.md", FileMode::Regular, b"# v2\n")];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    // The sweep lands v1.
    {
        let ctx = rig.ctx(&plane, &foll);
        let d = pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
        assert_eq!(only(&d).action, PullAction::FastForwarded);
    }
    // The plane moves to v2 before the targeted accept runs.
    plane.set_current(&id, served(WS, &id, v2.id, 2));
    let ctx = rig.ctx(&plane, &foll);
    let d = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    let row = only(&d);
    assert_eq!(
        row.action,
        PullAction::FastForwarded,
        "a version discovered during the accept is applied, not deferred"
    );
    assert_eq!(row.applied, 2);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(v2files)),
        "v2's bytes are on disk"
    );
}

#[test]
fn go_back_snapshots_an_unsaved_draft_before_overwriting() {
    // The never-clobber rail applies to go-back too: an explicit `pull <skill>@<old>` over an EDITED
    // placement must snapshot the draft into the sidecar store FIRST, so the unsaved edits stay recoverable.
    let rig = Rig::new("goback-draft");
    let (id, name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    // Fast-forward to v1 (so v1 is in the store + recorded; the placement is clean at v1).
    {
        let ctx = rig.ctx(&plane, &foll);
        pull_data(&ctx, ops::PullScope::AllFollowed).unwrap();
    }
    // Edit the placement → an unsaved local draft on top of v1.
    let edited: &[(&str, FileMode, &[u8])] = &[
        ("SKILL.md", FileMode::Regular, b"# my unsaved edit\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&rig.placement(), edited);
    // The draft snapshot the engine must make: a commit on the current base (v1) carrying the edited bytes.
    let draft = mk_version(&[v1.id], edited, DEVICE, "topos: draft snapshot");

    // Go back to genesis.
    let ctx = rig.ctx(&plane, &foll);
    let data = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(genesis)),
        },
    )
    .unwrap();
    assert_eq!(only(&data).action, PullAction::Held);
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "old bytes installed"
    );
    // CRITICAL: the unsaved draft was snapshotted into the store BEFORE the overwrite — it is recoverable.
    let store = topos_gitstore::Store::open(&rig.layout().published(&sid(&id)).store).unwrap();
    assert!(
        store.list_versions().unwrap().contains(&draft.id),
        "the unsaved draft must be snapshotted before a go-back overwrites it"
    );
}

/// A plane that returns a structurally-malformed response (a corrupt/forged record or bytes).
struct MalformedPlane;
impl PlaneSource for MalformedPlane {
    fn get_current(&self, _: &str, _: Option<KnownCurrent>) -> Result<PointerFetch, PlaneError> {
        Err(PlaneError::Malformed("corrupt current record".into()))
    }
    fn fetch_version(
        &self,
        _: &str,
        _: [u8; 32],
    ) -> Result<crate::plane::FetchedVersion, PlaneError> {
        Err(PlaneError::Malformed("corrupt version bytes".into()))
    }
}

#[test]
fn malformed_plane_response_is_a_wire_error() {
    // A structurally-malformed served response cannot be the sync target — a targeted pull surfaces it as a
    // wire-validation error (content addressing is the integrity story; a garbled body is simply refused).
    let rig = Rig::new("malformed");
    let (id, name, _genesis) = rig.adopt(BASE);
    let plane = MalformedPlane;
    let foll = follow(&id);
    let ctx = rig.ctx(&plane, &foll);
    let err = pull_data(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap_err();
    assert_eq!(
        err.code(),
        "CORRUPT_STATE",
        "a malformed response is a wire error"
    );
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(BASE)),
        "nothing applied"
    );
    assert_eq!(rig.read_sync(&id).observed, 0);
}

// ---------------------------------------------------------------------------------------------
// The sweep's plane-down circuit breaker + the machine-visible per-skill warnings.
// ---------------------------------------------------------------------------------------------

/// A counting transport whose `get_current` always fails at the given level — the breaker's oracle
/// (every network call the sweep makes is a counter tick).
#[derive(Default)]
struct CountingDownPlane {
    /// `true` ⇒ connect-level (`Unreachable`, trips the breaker); `false` ⇒ HTTP-level (`Unavailable`).
    connect_level: bool,
    gets: std::cell::Cell<u32>,
    lists: std::cell::Cell<u32>,
}
impl PlaneSource for CountingDownPlane {
    fn get_current(
        &self,
        _skill_id: &str,
        _known: Option<KnownCurrent>,
    ) -> Result<PointerFetch, PlaneError> {
        self.gets.set(self.gets.get() + 1);
        Err(if self.connect_level {
            PlaneError::Unreachable("connect refused".into())
        } else {
            PlaneError::Unavailable("HTTP 500".into())
        })
    }
    fn fetch_version(
        &self,
        _skill_id: &str,
        _version_id: [u8; 32],
    ) -> Result<crate::plane::FetchedVersion, PlaneError> {
        Err(PlaneError::Unavailable("HTTP 500".into()))
    }
    fn list_open_proposals(&self, _skill_id: &str) -> Result<Vec<[u8; 32]>, PlaneError> {
        self.lists.set(self.lists.get() + 1);
        Ok(Vec::new())
    }
}

/// A follow source listing the SAME skill N times — the cheapest way to drive an N-skill sweep against
/// one adopted sidecar (each pass takes and releases the per-skill lock sequentially).
fn follow_n(skill_id: &str, n: usize) -> FixtureFollow {
    FixtureFollow {
        entries: (0..n)
            .map(|_| {
                (
                    skill_id.to_owned(),
                    FollowContext {
                        workspace_id: WS.to_owned(),
                        review_required: false,
                        following: true,
                    },
                )
            })
            .collect(),
    }
}

#[test]
fn sweep_breaker_trips_on_first_connect_failure_and_skips_all_remaining_network_calls() {
    let rig = Rig::new("breaker");
    let (id, _name, _genesis) = rig.adopt(BASE);
    let plane = CountingDownPlane {
        connect_level: true,
        ..Default::default()
    };
    let foll = follow_n(&id, 3);

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // Every skill still gets a local-state row (the engine falls through to the local drive)...
    assert_eq!(out.data.skills.len(), 3);
    // ...but the plane was dialed exactly ONCE: the first connect-level failure tripped the breaker,
    // and the remaining sweep passes + the proposals count made ZERO further network calls.
    assert_eq!(
        plane.gets.get(),
        1,
        "one connect timeout, not one per skill"
    );
    assert_eq!(
        plane.lists.get(),
        0,
        "the proposals count is skipped once the breaker tripped"
    );
    assert_eq!(out.data.proposals_awaiting, 0);
}

#[test]
fn sweep_breaker_never_trips_on_an_http_level_failure() {
    let rig = Rig::new("nobreak");
    let (id, _name, _genesis) = rig.adopt(BASE);
    let plane = CountingDownPlane {
        connect_level: false,
        ..Default::default()
    };
    let foll = follow_n(&id, 3);

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // An HTTP 500 means the plane ANSWERED — per-skill isolation, no breaker: all three are dialed,
    // and the proposals count still runs.
    assert_eq!(out.data.skills.len(), 3);
    assert_eq!(plane.gets.get(), 3);
    assert_eq!(plane.lists.get(), 3);
}

#[test]
fn go_back_is_plane_independent_and_spends_no_network_call() {
    let rig = Rig::new("gbnonet");
    let (id, name, genesis) = rig.adopt(BASE);
    let plane = CountingDownPlane {
        connect_level: true,
        ..Default::default()
    };
    let foll = follow(&id);

    // A go-back to the adopted genesis (recorded locally) must complete with the plane fully down —
    // and make ZERO network calls (including the proposals count, which is documented plane-independent).
    let out = ops::pull(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name,
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(genesis)),
        },
    )
    .unwrap();
    assert_eq!(out.data.skills.len(), 1);
    assert_eq!(plane.gets.get(), 0, "go-back never dials the plane");
    assert_eq!(plane.lists.get(), 0, "no proposals GET on the go-back path");
    assert_eq!(out.data.proposals_awaiting, 0);
}

#[test]
fn sweep_surfaces_an_isolated_per_skill_failure_as_an_envelope_warning() {
    let rig = Rig::new("warn");
    let (id, _name, _genesis) = rig.adopt(BASE);
    let plane = FixturePlane::default(); // serves nothing → the healthy skill reads NotFound → UpToDate
    let foll = FixtureFollow {
        entries: vec![
            // A followed id with NO sidecar docs — the sweep must isolate it, not abort.
            (
                "topos_missing".to_owned(),
                FollowContext {
                    workspace_id: WS.to_owned(),
                    review_required: false,
                    following: true,
                },
            ),
            (
                id.clone(),
                FollowContext {
                    workspace_id: WS.to_owned(),
                    review_required: false,
                    following: true,
                },
            ),
        ],
    };

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The healthy skill still produced its row (isolation)...
    assert_eq!(out.data.skills.len(), 1);
    assert_eq!(out.data.skills[0].action, PullAction::UpToDate);
    // ...and the failed one is machine-visible: one stable-shape warning naming the code + the skill.
    assert_eq!(out.warnings.len(), 1);
    let w = &out.warnings[0];
    assert!(
        w.text.contains("topos_missing"),
        "the warning names the failed skill: {w:?}"
    );
    // The CODE rides its own field now — never the prose a person reads.
    let code = w.code.clone().expect("the stable error code");
    assert!(
        code.starts_with(char::is_uppercase) && !code.contains(' '),
        "the code is one SCREAMING_SNAKE token: {w:?}"
    );
    assert!(
        !w.text.starts_with(&code),
        "the text never repeats the code: {w:?}"
    );
}

#[test]
fn a_wedged_skills_sweep_failure_surfaces_in_its_topos_log() {
    let rig = Rig::new("wedgelog");
    let (id, name, _genesis) = rig.adopt(BASE);
    // Wedge the tracked skill: a corrupt sync.json makes every sweep of it fail. lock.json + the store
    // stay intact, so `log` still resolves the skill.
    std::fs::write(rig.layout().published(&sid(&id)).sync, b"{not json").unwrap();
    let plane = FixturePlane::default();
    let foll = follow(&id);

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert!(out.data.skills.is_empty(), "the wedged skill has no row");
    assert_eq!(out.warnings.len(), 1);

    // The REAL read path: `topos log <skill>` filters on the first-class skill_id field, so the wedged
    // skill's error event surfaces in its own log.
    let nosess = |_s: &crate::sessions::Session| -> ops::SessionTransports {
        unreachable!("local log builds no session transports")
    };
    let connectors = ops::LogConnectors { session: &nosess };
    let log = ops::log(
        &rig.ctx(&plane, &foll),
        &connectors,
        &name,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let errors: Vec<_> = log
        .events
        .iter()
        .filter(|e| e.get("action").and_then(|v| v.as_str()) == Some("error"))
        .collect();
    assert_eq!(
        errors.len(),
        1,
        "the wedged skill's failure is in its log: {:?}",
        log.events
    );
    assert_eq!(
        errors[0].get("skill_id").and_then(|v| v.as_str()),
        Some(id.as_str())
    );
    assert_eq!(errors[0].get("verb").and_then(|v| v.as_str()), Some("pull"));

    // The TTY renderer's error arm renders it readably (verb + code).
    let text = crate::render::log_tty(&log);
    assert!(text.contains("error  pull ["), "{text}");
}

#[test]
fn sweep_refuses_a_traversal_follow_id_as_a_warning_never_a_join() {
    let rig = Rig::new("hostileid");
    let (_id, _name, _genesis) = rig.adopt(BASE);
    let plane = FixturePlane::default();
    let foll = FixtureFollow {
        entries: vec![(
            "../../evil".to_owned(),
            FollowContext {
                workspace_id: WS.to_owned(),
                review_required: false,
                following: true,
            },
        )],
    };

    let out = ops::pull(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The hostile id never reaches a path join: no row, one warning, and nothing appears at the
    // would-be escape target beside the home.
    assert!(out.data.skills.is_empty());
    assert_eq!(out.warnings.len(), 1);
    assert!(
        out.warnings[0].code.as_deref() == Some("CORRUPT_STATE"),
        "{:?}",
        out.warnings
    );
    assert!(
        !rig.home.0.parent().unwrap().join("evil").exists(),
        "no directory materialized outside the home"
    );
}

// ---------------------------------------------------------------------------------------------
// The per-op durability bound: a pull fsyncs the fetched version's objects + ref — and ONLY those —
// before any doc records the applied version (the fetch-then-record contract).
// ---------------------------------------------------------------------------------------------

/// Wraps [`RealFs`] and records every mutating op (label + the affected path) in call order, so a test
/// can pin WHAT a pull made durable, that the set is bounded (no historical object re-synced), and that
/// the store fsyncs precede the doc writes recording the result. Reads/locks are not recorded.
struct RecordingFs {
    inner: RealFs,
    ops: std::cell::RefCell<Vec<(&'static str, PathBuf)>>,
}
impl RecordingFs {
    fn new() -> Self {
        Self {
            inner: RealFs,
            ops: std::cell::RefCell::new(Vec::new()),
        }
    }
    fn record(&self, label: &'static str, path: &Path) {
        self.ops.borrow_mut().push((label, path.to_path_buf()));
    }
    fn ops(&self) -> Vec<(&'static str, PathBuf)> {
        self.ops.borrow().clone()
    }
}
impl FsOps for RecordingFs {
    fn write_temp(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.record("write_temp", path);
        self.inner.write_temp(path, bytes)
    }
    fn fsync_file(&self, path: &Path) -> std::io::Result<()> {
        self.record("fsync_file", path);
        self.inner.fsync_file(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.record("rename", to);
        self.inner.rename(from, to)
    }
    fn fsync_dir(&self, dir: &Path) -> std::io::Result<()> {
        self.record("fsync_dir", dir);
        self.inner.fsync_dir(dir)
    }
    fn rename_dir_noreplace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.record("rename_dir_noreplace", to);
        self.inner.rename_dir_noreplace(from, to)
    }
    fn create_dir_all(&self, dir: &Path) -> std::io::Result<()> {
        self.record("create_dir_all", dir);
        self.inner.create_dir_all(dir)
    }
    fn append_fsync(&self, path: &Path, line: &[u8]) -> std::io::Result<()> {
        self.record("append_fsync", path);
        self.inner.append_fsync(path, line)
    }
    fn remove_file(&self, path: &Path) -> std::io::Result<()> {
        self.record("remove_file", path);
        self.inner.remove_file(path)
    }
    fn remove_dir_all(&self, path: &Path) -> std::io::Result<()> {
        self.record("remove_dir_all", path);
        self.inner.remove_dir_all(path)
    }
    fn write_staged(&self, path: &Path, bytes: &[u8], executable: bool) -> std::io::Result<()> {
        self.record("write_staged", path);
        self.inner.write_staged(path, bytes, executable)
    }
    fn write_private(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
        self.record("write_private", path);
        self.inner.write_private(path, bytes)
    }
    fn open_dir_handle(&self, dir: &Path) -> std::io::Result<crate::fs_seam::DirHandle> {
        self.inner.open_dir_handle(dir)
    }
    fn rename_at(
        &self,
        h: &crate::fs_seam::DirHandle,
        from: &str,
        to: &str,
    ) -> std::io::Result<()> {
        self.record("rename_at", &h.path().join(to));
        self.inner.rename_at(h, from, to)
    }
    fn rename_at_noreplace_src(
        &self,
        h: &crate::fs_seam::DirHandle,
        from: &str,
        to: &str,
        src: &crate::fs_seam::DirHandle,
    ) -> std::io::Result<()> {
        self.record("rename_at_noreplace_src", &h.path().join(to));
        self.inner.rename_at_noreplace_src(h, from, to, src)
    }
    fn exchange_at(&self, h: &crate::fs_seam::DirHandle, a: &str, b: &str) -> std::io::Result<()> {
        self.record("exchange_at", &h.path().join(b));
        self.inner.exchange_at(h, a, b)
    }
    fn exchange_at_src(
        &self,
        h: &crate::fs_seam::DirHandle,
        a: &str,
        b: &str,
        src: &crate::fs_seam::DirHandle,
    ) -> std::io::Result<()> {
        self.record("exchange_at_src", &h.path().join(b));
        self.inner.exchange_at_src(h, a, b, src)
    }
    fn create_dir_nofollow(
        &self,
        base: &Path,
        dir: &Path,
    ) -> std::io::Result<crate::fs_seam::DirHandle> {
        self.record("create_dir_nofollow", dir);
        self.inner.create_dir_nofollow(base, dir)
    }
    fn create_dir_nofollow_at(
        &self,
        base: &crate::fs_seam::DirHandle,
        dir: &Path,
    ) -> std::io::Result<crate::fs_seam::DirHandle> {
        self.record("create_dir_nofollow_at", dir);
        self.inner.create_dir_nofollow_at(base, dir)
    }
    fn remove_dir_all_at(&self, h: &crate::fs_seam::DirHandle, leaf: &str) -> std::io::Result<()> {
        self.record("remove_dir_all_at", &h.path().join(leaf));
        self.inner.remove_dir_all_at(h, leaf)
    }
    fn create_dir_at(&self, h: &crate::fs_seam::DirHandle, leaf: &str) -> std::io::Result<()> {
        self.record("create_dir_at", &h.path().join(leaf));
        self.inner.create_dir_at(h, leaf)
    }
    fn write_new_at(
        &self,
        h: &crate::fs_seam::DirHandle,
        leaf: &str,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        self.record("write_new_at", &h.path().join(leaf));
        self.inner.write_new_at(h, leaf, bytes)
    }
    fn write_staged_at(
        &self,
        h: &crate::fs_seam::DirHandle,
        leaf: &str,
        bytes: &[u8],
        executable: bool,
    ) -> std::io::Result<()> {
        self.record("write_staged_at", &h.path().join(leaf));
        self.inner.write_staged_at(h, leaf, bytes, executable)
    }
    fn rename_file_noreplace(&self, from: &Path, to: &Path) -> std::io::Result<()> {
        self.record("rename_file_noreplace", to);
        self.inner.rename_file_noreplace(from, to)
    }
    fn read_opt(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.inner.read_opt(path)
    }
    fn read_opt_nofollow(&self, path: &Path) -> std::io::Result<Option<Vec<u8>>> {
        self.inner.read_opt_nofollow(path)
    }
    fn read_dir(&self, dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        self.inner.read_dir(dir)
    }
    fn exists(&self, path: &Path) -> bool {
        self.inner.exists(path)
    }
    fn canonicalize(&self, path: &Path) -> std::io::Result<PathBuf> {
        self.inner.canonicalize(path)
    }
    fn path_kind(&self, path: &Path) -> std::io::Result<Option<crate::fs_seam::PathKind>> {
        self.inner.path_kind(path)
    }
    fn private_perms_ok(&self, path: &Path) -> std::io::Result<bool> {
        self.inner.private_perms_ok(path)
    }
    fn lock_exclusive(&self, path: &Path) -> std::io::Result<crate::fs_seam::LockGuard> {
        self.inner.lock_exclusive(path)
    }
    fn try_lock_exclusive(
        &self,
        path: &Path,
    ) -> std::io::Result<Option<crate::fs_seam::LockGuard>> {
        self.inner.try_lock_exclusive(path)
    }
}

/// Every loose object file currently under `<store>/objects/` (the shard walk).
fn store_loose_objects(store_dir: &Path) -> std::collections::HashSet<PathBuf> {
    let mut out = std::collections::HashSet::new();
    let objects = store_dir.join("objects");
    for shard in std::fs::read_dir(&objects).unwrap().flatten() {
        let p = shard.path();
        if p.is_dir() {
            for f in std::fs::read_dir(&p).unwrap().flatten() {
                if f.path().is_file() {
                    out.insert(f.path());
                }
            }
        }
    }
    out
}

/// V2 in the genesis → v1 → v2 chain — every file's bytes differ from BOTH earlier generations, so the
/// three versions share no blobs and the era sets below partition cleanly.
const V2: &[(&str, FileMode, &[u8])] = &[
    ("SKILL.md", FileMode::Regular, b"# v2\n"),
    ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v2\n"),
    ("ref/notes.md", FileMode::Regular, b"new in v2\n"),
];

#[test]
fn pull_fsyncs_exactly_the_fetched_version_plus_its_direct_parent() {
    // Chain genesis → v1 → v2. Land v1 with a plain pull, then record a pull of v2 and pin its
    // durability frontier: the fetched version's own writes PLUS its direct parent's set (present ≠
    // durable, so a present v1 is re-fsynced — no-ops when it already was) — and NOTHING beyond:
    // grandparent-era (genesis) objects are never re-fsynced when the parent was present.
    let rig = Rig::new("fsyncset");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2 = mk_version(&[v1.id], V2, "d_pub", "v2");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let store_dir = rig.layout().published(&sid(&id)).store;
    let genesis_era = store_loose_objects(&store_dir);
    assert!(!genesis_era.is_empty(), "adopt left genesis objects");

    // Land v1 first (not the pull under test) — v2's direct parent becomes present + recorded.
    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);
    let after_v1 = store_loose_objects(&store_dir);
    let v1_era: Vec<&PathBuf> = after_v1
        .iter()
        .filter(|p| !genesis_era.contains(*p))
        .collect();
    assert!(!v1_era.is_empty(), "the v1 pull wrote v1's objects");

    // The recorded pull: v2 arrives; its direct parent v1 is already present.
    plane.set_current(&id, served(WS, &id, v2.id, 2));
    let fs = RecordingFs::new();
    let data = pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);

    let ops_log = fs.ops();
    let store_fsyncs: Vec<(usize, &PathBuf)> = ops_log
        .iter()
        .enumerate()
        .filter(|(_, (label, p))| *label == "fsync_file" && p.starts_with(&store_dir))
        .map(|(i, (_, p))| (i, p))
        .collect();
    let synced: std::collections::HashSet<&PathBuf> =
        store_fsyncs.iter().map(|&(_, p)| p).collect();

    // (a) COMPLETE: every loose object the fetch wrote — and v2's version ref — was fsynced before the
    // pull returned (the crash-safety contract: reachable ⇒ durable before recorded).
    let new: Vec<PathBuf> = store_loose_objects(&store_dir)
        .into_iter()
        .filter(|p| !after_v1.contains(p))
        .collect();
    assert!(!new.is_empty(), "the fetch wrote v2's objects");
    for p in &new {
        assert!(synced.contains(p), "fetched object {p:?} was not fsynced");
    }
    let v2_ref = store_dir.join("refs/topos/versions").join(to_hex(&v2.id));
    assert!(synced.contains(&v2_ref), "v2's version ref was not fsynced");

    // (b) PARENT INCLUDED: the direct parent's whole era was re-fsynced too — a present parent may sit
    // in the crash window between its write and its fsync, and this pull records a child naming it.
    for p in &v1_era {
        assert!(
            synced.contains(*p),
            "direct-parent object {p:?} was not re-fsynced — present was treated as durable"
        );
    }
    let v1_ref = store_dir.join("refs/topos/versions").join(to_hex(&v1.id));
    assert!(
        synced.contains(&v1_ref),
        "v1's version ref was not re-fsynced"
    );

    // (c) BOUNDED: nothing beyond the fetched version + its direct parent — no grandparent-era
    // (genesis) object or ref was re-fsynced, because the present parent's arm returns before walking
    // ITS parents. The per-pull durability set stays bounded, never the store's lifetime history.
    for p in &genesis_era {
        assert!(
            !synced.contains(p),
            "grandparent-era object {p:?} was re-fsynced — the durability set is unbounded"
        );
    }
    let genesis_ref = store_dir.join("refs/topos/versions").join(to_hex(&genesis));
    assert!(
        !synced.contains(&genesis_ref),
        "the grandparent's version ref was re-fsynced"
    );

    // (d) ORDERED: every store fsync precedes the first doc write that records the applied version
    // (map/lock are written only by the post-swap doc commit; sync.json's floor raise is earlier by
    // design and names no local bytes).
    let last_store_fsync = store_fsyncs.iter().map(|&(i, _)| i).max().unwrap();
    let first_apply_doc = ops_log
        .iter()
        .enumerate()
        .find(|(_, (label, p))| {
            *label == "write_temp"
                && p.file_name()
                    .is_some_and(|f| f.to_string_lossy().starts_with("map.json"))
        })
        .map(|(i, _)| i)
        .expect("the apply committed its docs");
    assert!(
        last_store_fsync < first_apply_doc,
        "a store fsync ({last_store_fsync}) landed after the doc commit began ({first_apply_doc})"
    );
}

#[test]
fn pull_fsyncs_a_present_but_unrecorded_parent() {
    // The crash window itself: a prior pull wrote v1's objects + ref but died BEFORE its fsync and
    // before any doc recorded it — v1 is present-and-renderable yet recorded nowhere and possibly not
    // durable. A pull of its child v2 must fsync v1's whole set too (never fetching it — it IS present),
    // not just v2's own writes.
    let rig = Rig::new("fsyncparent");
    let (id, _name, genesis) = rig.adopt(BASE);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2 = mk_version(&[v1.id], V2, "d_pub", "v2");

    // Simulate the crash: commit v1 straight into the sidecar store — no fsync, no doc record.
    {
        let store = rig.open_store(&id);
        let import: Vec<topos_gitstore::ImportFile<'_>> = v1
            .fetched
            .files
            .iter()
            .map(|f| topos_gitstore::ImportFile {
                path: &f.path,
                mode: f.mode,
                bytes: &f.bytes,
            })
            .collect();
        let tree = store.write_bundle(&import).unwrap();
        store
            .commit(
                v1.id,
                &[genesis],
                &tree,
                &v1.fetched.author,
                &v1.fetched.message,
            )
            .unwrap();
    }

    // The plane serves ONLY v2 — the pull must not need to fetch the present parent.
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v2.id, 1));
    let foll = follow(&id);

    let store_dir = rig.layout().published(&sid(&id)).store;
    let fs = RecordingFs::new();
    let data = pull_data(&rig.ctx_fs(&fs, &plane, &foll), ops::PullScope::AllFollowed).unwrap();
    assert_eq!(only(&data).action, PullAction::FastForwarded);

    let synced: std::collections::HashSet<PathBuf> = fs
        .ops()
        .into_iter()
        .filter(|(label, p)| *label == "fsync_file" && p.starts_with(&store_dir))
        .map(|(_, p)| p)
        .collect();

    // v1's entire durability set (ref + commit + trees + blobs) was fsynced by the pull of v2, closing
    // the window where a doc records a child whose parent lineage could vanish on power loss.
    let v1_set = rig.open_store(&id).version_durability(&v1.id).unwrap();
    assert!(!v1_set.files.is_empty(), "v1 names a durability set");
    for p in &v1_set.files {
        assert!(
            synced.contains(p),
            "present-but-unrecorded parent path {p:?} was not fsynced"
        );
    }
}
