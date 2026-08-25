//! The conflict record's lifecycle: the review-driven escape regressions, the workbench folder
//! named by parsing rather than by trusting, and the record's own liveness across sweeps.

use std::path::Path;

use topos_core::digest::{FileMode, to_hex};
use topos_types::results::PullAction;

use crate::ctx::Ctx;
use crate::fs_seam::RealFs;
use crate::plane::{InertFollow, InertPlane};
use crate::sidecar::Layout;
use crate::{doc, ops};

use super::rig::*;

// --- review-driven regression tests ---

/// **The "keep my version" exit.** Leaving the conflict folder alone and running `--keep-mine`
/// commits the merge with this person's side kept on the contested file, never the raw marker tree
/// — otherwise the markers would become a publishable bundle. The folder goes with the block.
#[test]
fn escape_of_unedited_conflict_commits_the_resolution_not_markers() {
    let rig = Rig::new("escape-unedited");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    // Auto sweep → conflict (overlapping SKILL.md) → the markers are in the CONFLICT COPY, and the
    // placement still holds MINE.
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted
    );
    let copy = rig.conflict_copy(&id);
    assert!(
        std::fs::read_to_string(copy.join("SKILL.md"))
            .unwrap()
            .contains("<<<<<<<")
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));

    // Escape WITHOUT touching the folder → commits the resolution, not the markers; clears the
    // block and removes the folder.
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
    assert!(!rig.conflict_exists(&id), "escape clears the block");
    assert!(!copy.exists(), "and removes the marked-up copy");
    // The placement holds this person's `SKILL.md` and the team's other changes — no markers
    // anywhere.
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(
        !std::fs::read_to_string(rig.placement().join("SKILL.md"))
            .unwrap()
            .contains("<<<<<<<"),
        "the escape must not commit unresolved markers"
    );
    // The committed escape is a 1-parent commit on `current` carrying that tree — the markers never
    // entered the publishable lineage.
    let m = mk_version(&[v1.id], KEPT_OVER_V1, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

/// Hand-resolving the CONFLICT FOLDER and then running `--keep-mine` commits those bytes — the
/// author's resolution — onto `current`, and writes them to every managed placement.
#[test]
fn escape_of_edited_conflict_commits_the_resolution() {
    let rig = Rig::new("escape-edited");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The author hand-resolves the marked-up copy (removes markers) and then escapes. The
    // placement is deliberately left alone — the folder is the resolution surface now.
    let copy = rig.conflict_copy(&id);
    let resolved: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&copy, resolved);
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
    assert!(!rig.conflict_exists(&id));
    assert!(!copy.exists(), "the resolved copy is consumed and removed");
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(resolved)),
        "the escape commits the author's hand resolution and places it"
    );
    // Publishable: the resolution is a real 1-parent commit on `current`.
    let m = mk_version(&[v1.id], resolved, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

// --- the workbench folder is named by parsing, never by trusting ---

/// A minimal PROJECT store holding one blocked bundle: the store tree, a `lock.json`, and
/// (optionally) the `conflict.json` that names the workbench folder. Enough for the ONE act these
/// tests exercise — clearing the block, which is the act a receipt tells the reader to run.
struct HostileCheckout {
    layout: Layout,
    sp: crate::sidecar::SkillPaths,
}

impl HostileCheckout {
    /// `lock.name` / `lock.skill_id` are the two UNTRUSTED strings a clone controls; `record` is
    /// the `conflict.json` `copy_dir` it can also commit (`None` writes no record at all, so the
    /// lock's own strings are what the removal has to name the folder from).
    fn plant(project: &Path, name: &str, skill_id: &str, record: Option<&str>) -> Self {
        let layout = crate::sidecar::project_store_layout(project);
        let sp = layout.published(&sid("topos_conflict1"));
        std::fs::create_dir_all(&sp.store).unwrap();
        let lock = topos_types::persisted::Lock {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            skill_id: skill_id.to_owned(),
            name: name.to_owned(),
            base_commit: "0".repeat(64),
            bundle_digest: "0".repeat(64),
            files: Vec::new(),
        };
        doc::write_doc(&RealFs, &sp.lock, &lock).unwrap();
        if let Some(copy_dir) = record {
            let cs = topos_types::persisted::ConflictState {
                schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
                base_commit: "0".repeat(64),
                base_digest: "0".repeat(64),
                current_commit: "1".repeat(64),
                current_digest: "1".repeat(64),
                draft_commit: "2".repeat(64),
                draft_digest: "2".repeat(64),
                result_commit: "3".repeat(64),
                conflicted_digest: "3".repeat(64),
                copy_dir: Some(copy_dir.to_owned()),
                reason: topos_types::persisted::ConflictReason::ThreeWay,
                concluded: None,
                paths: Vec::new(),
            };
            doc::write_doc(&RealFs, &sp.conflict, &cs).unwrap();
        }
        Self { layout, sp }
    }
}

/// **A hostile checkout must not be able to aim a recursive delete out of its own store.**
///
/// `clear_conflict` is what `--keep-mine` and `--reset` both call — the two commands a conflict
/// receipt tells the reader to run — so a clone that ships `.topos/state/<user>/conflicts` as a
/// SYMLINK pointing at a home directory, plus a `conflict.json` naming a plain component inside
/// it, used to get `~/Documents` deleted recursively: the removal was path-based, and the kernel
/// resolved that symlinked intermediate component normally.
///
/// The removal now descends the same way the WRITE does — a held handle on the store's own
/// `conflicts/` directory, opened `O_NOFOLLOW` — so the swapped component is met as itself and
/// refused. Nothing outside the store is touched.
#[test]
fn a_symlinked_conflicts_component_deletes_nothing_outside_the_store() {
    let rig = Rig::new("hostile-symlink");
    let project = rig.work.0.join("checkout");
    let victim = rig.work.0.join("victim-home");
    let precious = victim.join("Documents");
    std::fs::create_dir_all(precious.join("taxes")).unwrap();
    std::fs::write(precious.join("taxes/2025.pdf"), b"irreplaceable\n").unwrap();

    // The clone's committed state: the store, a record naming a plainly SAFE component, and the
    // `conflicts` component itself swapped for a link to the victim's home.
    let planted = HostileCheckout::plant(
        &project,
        "pr-describe",
        "topos_conflict1",
        Some("Documents"),
    );
    std::fs::create_dir_all(planted.layout.home()).unwrap();
    std::os::unix::fs::symlink(&victim, planted.layout.conflicts_dir()).unwrap();

    let (p, f) = (InertPlane, InertFollow);
    let ctx = rig.ctx_at(planted.layout.clone(), &p, &f);
    let cleared = ops::merge_resolve::clear_conflict(
        &ctx,
        &planted.sp,
        ops::merge_resolve::Workbench::Unread,
    );

    assert!(
        precious.join("taxes/2025.pdf").exists(),
        "nothing outside the store may be deleted"
    );
    assert!(victim.exists() && precious.exists());
    assert!(
        cleared.is_err(),
        "a `conflicts` component that is not a real directory must refuse, not be followed"
    );
}

/// The same rule for the OTHER door into the join: the fallbacks the removal used to derive a
/// folder name from when no record could be read. A clone commits a `lock.json` whose display name
/// sanitizes to nothing (all fullwidth) and whose `skill_id` — a raw string on disk, not the
/// validated newtype — climbs out of the store with `..`. With no `conflict.json` at all, that raw
/// id used to be joined straight onto `conflicts/` and the resulting path deleted recursively.
///
/// The whole ladder is gone: with no readable record nothing names a folder, so the removal has
/// nothing to act on and does nothing at all.
#[test]
fn a_hostile_lock_skill_id_cannot_traverse_out_of_the_store() {
    let rig = Rig::new("hostile-traversal");
    let project = rig.work.0.join("checkout");
    let precious = project.join("src");
    std::fs::create_dir_all(&precious).unwrap();
    std::fs::write(precious.join("main.rs"), b"fn main() {}\n").unwrap();

    // `<project>/.topos/state/<user>/conflicts/../../../../src` == `<project>/src`.
    let planted = HostileCheckout::plant(&project, "ＡＢＣ", "../../../../src", None);
    std::fs::create_dir_all(planted.layout.conflicts_dir()).unwrap();

    let (p, f) = (InertPlane, InertFollow);
    let ctx = rig.ctx_at(planted.layout.clone(), &p, &f);
    ops::merge_resolve::clear_conflict(&ctx, &planted.sp, ops::merge_resolve::Workbench::Unread)
        .unwrap();

    assert!(
        precious.join("main.rs").exists(),
        "an unvalidated on-disk string must never reach a path join"
    );
    assert!(
        !planted.sp.conflict.exists(),
        "the block itself still clears"
    );
}

/// **With no readable record, remove nothing.**
///
/// Two bundles can legitimately carry the same display name — two workspaces, or a workspace copy
/// beside a local one — and the workbench folder is keyed by that name. So a removal that derives
/// its target from the name, as this one did whenever no record could be read, deletes the OTHER
/// bundle's live hand merge; `--reset` reached that derivation unconditionally, and the quiet
/// sweep defensively. Git never re-derives a deletion target from a user-facing name.
#[test]
fn a_clear_with_no_readable_record_never_names_a_folder_from_the_bundle_name() {
    let rig = Rig::new("no-record-no-removal");
    // The OTHER bundle's live workbench, under the name the two share.
    let live = rig.layout().home().join("conflicts").join("pr-describe");
    std::fs::create_dir_all(&live).unwrap();
    std::fs::write(live.join("SKILL.md"), b"# my hand merge\n").unwrap();

    // THIS bundle: the same display name, and a `conflict.json` that cannot be read.
    let sp = rig.layout().published(&sid("topos_twin1"));
    std::fs::create_dir_all(&sp.store).unwrap();
    doc::write_doc(
        &rig.fs,
        &sp.lock,
        &topos_types::persisted::Lock {
            schema_version: topos_types::PERSISTED_SCHEMA_VERSION,
            skill_id: "topos_twin1".to_owned(),
            name: "pr-describe".to_owned(),
            base_commit: "0".repeat(64),
            bundle_digest: "0".repeat(64),
            files: Vec::new(),
        },
    )
    .unwrap();
    std::fs::write(&sp.conflict, b"{ not json at all").unwrap();

    let (p, f) = (InertPlane, InertFollow);
    let ctx = rig.ctx(&p, &f);
    ops::merge_resolve::clear_conflict(&ctx, &sp, ops::merge_resolve::Workbench::Unread).unwrap();

    assert_eq!(
        std::fs::read(live.join("SKILL.md")).unwrap(),
        b"# my hand merge\n",
        "another bundle's live hand merge must survive"
    );
    assert!(
        !sp.conflict.exists(),
        "the block itself still clears — only the folder is spared"
    );
}

/// **An unscannable workbench folder must refuse, never destroy the hand resolution in it.**
///
/// The folder is the ONLY copy of a hand merge — it sits outside the placement map, so the
/// materializer's snapshot rail never sees it. `--keep-mine` used to fold every scan failure
/// into "the folder is absent": it committed the ORIGINAL draft, wrote it over every placement,
/// deleted the folder, and reported `Merged`. The scanner rejects a tree holding a symlink (as
/// here), a non-regular file, a non-UTF-8 name, or no files at all — every one of them a plausible
/// state for someone mid-merge.
#[test]
fn an_unreadable_conflict_folder_refuses_instead_of_destroying_the_hand_resolution() {
    let rig = Rig::new("unreadable-copy");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The hand merge, plus one thing the scanner refuses — a symlink to a note kept elsewhere.
    let copy = rig.conflict_copy(&id);
    write_tree(
        &copy,
        &[
            ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ],
    );
    let elsewhere = rig.work.0.join("notes.md");
    std::fs::write(&elsewhere, b"scratch\n").unwrap();
    std::os::unix::fs::symlink(&elsewhere, copy.join("notes.md")).unwrap();

    let err = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .expect_err("an unreadable workbench folder must refuse");
    match &err {
        crate::error::ClientError::ConflictCopyUnreadable { skill, reason, .. } => {
            assert_eq!(skill, "pr-describe");
            assert!(reason.contains("symlink"), "{reason}");
        }
        other => panic!("expected the unreadable-workbench refusal, got {other:?}"),
    }
    // NOTHING moved: the hand resolution is still on disk, the block still stands, and the
    // placement still holds the author's own version.
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        b"# hand-resolved\n",
        "the only copy of the hand merge must survive"
    );
    assert!(rig.conflict_exists(&id), "the block still stands");
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));

    // Remove the offending entry and the same command resolves normally — the refusal is a state
    // to fix, never a dead end.
    std::fs::remove_file(copy.join("notes.md")).unwrap();
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
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[
            ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ]))
    );
}

/// `.topos-mine` siblings are topos's own compare-and-resolve scaffolding, so they never become
/// bundle content. Keeping them out of the placements holds only while the block stands: the
/// escape COMMITS the workbench folder and writes it to every placement, and `publish` ships a
/// placement's bytes — so the moment the author edits anything and escapes, an unstripped sibling
/// would ship to the team. It is stripped instead of refused, so resolving a binary conflict the
/// obvious way (fix the file, leave the aid alone) is not a dead end.
#[test]
fn a_hand_resolution_never_commits_topos_mine_scaffolding() {
    let rig = Rig::new("mine-strip");
    let base: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 1, 2])];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 9, 9])];
    write_tree(&rig.placement(), mine);
    let theirs: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 7, 7])];
    let v1 = mk_version(&[genesis], theirs, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // The binary conflict kept both sides in the workbench. The author resolves the real file and
    // leaves the aid exactly where topos put it.
    let copy = rig.conflict_copy(&id);
    assert!(copy.join("logo.bin.topos-mine").exists());
    std::fs::write(copy.join("logo.bin"), [0xffu8, 4, 4]).unwrap();

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
    // The placement — which is what `publish` ships — holds the resolution and nothing else.
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[("logo.bin", FileMode::Regular, &[0xffu8, 4, 4])])),
        "the .topos-mine sibling must not become bundle content"
    );
    assert!(!rig.placement().join("logo.bin.topos-mine").exists());
    // And the committed version carries the same file set, so nothing can publish it later either.
    let committed: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 4, 4])];
    let m = mk_version(&[v1.id], committed, DEVICE, "topos: merge escape");
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

/// An exit marks the record CONCLUDED, converges its placements, and clears `conflict.json` LAST,
/// so a crash in that beat leaves a fully resolved bundle still carrying the MARKED record — that
/// is the one shape a real crash leaves, and both arms must FINISH it rather than read it as a
/// live block. Read live, the leftover does damage in both directions: a sweep re-settles
/// `work_hash` to the pre-escape draft (naming bytes that are nowhere on disk) and re-discloses a
/// block on a resolved bundle, and `--keep-mine` sees the already-removed workbench folder as
/// "untouched" and commits the ORIGINAL DRAFT over the resolution the placements hold. The mark —
/// never a document comparison — is what tells the leftover from a live block.
#[test]
fn a_record_that_outlived_its_resolution_is_cleared_not_re_blocked() {
    let rig = Rig::new("record-outlived");
    let (id, _name, genesis) = rig.adopt(BASE);
    let mine: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), mine);
    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);
    pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();

    // Hand-resolve, escape — and then put the record back MARKED, which is exactly the on-disk
    // state a crash between the escape's placement write and its record removal leaves (the mark
    // goes down before any placement moves).
    let record = rig.conflict_state(&id);
    let copy = rig.conflict_copy(&id);
    let resolved: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# hand-resolved\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&copy, resolved);
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
    let after_escape = rig.read_sync(&id);
    let crashed = topos_types::persisted::ConflictState {
        concluded: Some(topos_types::persisted::ConcludedExit::Escape),
        ..record.clone()
    };
    doc::write_doc(
        &rig.fs,
        &rig.layout().published(&sid(&id)).conflict,
        &crashed,
    )
    .unwrap();

    // (a) the marked leftover under `--keep-mine`: the crashed exit's own command re-runs it to
    // completion, idempotently — the content-addressed conclusion is the SAME commit, the
    // materializer heals the already-landed placement with no second swap, and the record clears
    // with the real finish. Nothing re-commits the pre-escape draft over the hand resolution.
    let finished = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::KeepMine,
        },
    )
    .unwrap();
    assert_eq!(
        only(&finished).action,
        PullAction::Merged,
        "a marked-Escape leftover finishes, never refuses"
    );
    assert!(!rig.conflict_exists(&id));
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(resolved)),
        "the hand resolution must survive a leftover record"
    );
    assert_eq!(
        rig.read_sync(&id).work_hash,
        after_escape.work_hash,
        "the finish re-lands the escape's own state"
    );

    // (b) the same marked leftover under a bare sweep, after the team publishes AGAIN. The sweep
    // FIRST finishes the marked conclusion — its row IS the finished merge — and only a LATER
    // sweep raises a fresh block against the version that actually moved. What must never happen
    // is the STALE record's re-disclosure, which would re-settle `work_hash` to the pre-escape
    // draft and send the reader to a workbench describing bytes that are nowhere on disk. The
    // discriminator is the MARK, not whether a block stands.
    //
    // The team publishes AGAIN first, so a block really is raised. Without a pending update the
    // second sweep has nothing to merge, and the assertions about the FRESH record below never run
    // — the situation they check has to be created for them to check anything at all.
    let v2files: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# v2\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v2.id, 2));
    doc::write_doc(
        &rig.fs,
        &rig.layout().published(&sid(&id)).conflict,
        &crashed,
    )
    .unwrap();
    let first =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(
        first.action,
        PullAction::Merged,
        "the sweep finishes the marked conclusion before anything else"
    );
    assert!(
        !rig.conflict_exists(&id),
        "the finished conclusion takes the leftover record with it"
    );
    let row =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "v2 contests the same line the hand resolution rewrote"
    );
    let now = rig.read_sync(&id);
    assert_eq!(now.work_hash, after_escape.work_hash);
    assert_ne!(
        now.work_hash, record.draft_digest,
        "the docs must not name the pre-escape draft — nothing on disk holds it"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(resolved)));
    let fresh = doc::read_doc::<topos_types::persisted::ConflictState>(
        &rig.fs,
        &rig.layout().published(&sid(&id)).conflict,
    )
    .unwrap()
    .expect("the new divergence raises a block of its own");
    assert_eq!(
        fresh.draft_digest, after_escape.work_hash,
        "a re-raised block describes the bytes on disk, never the pre-escape draft"
    );
    assert_ne!(
        fresh.result_commit, record.result_commit,
        "…and it is a fresh merge, not the leftover re-disclosed"
    );
    assert_eq!(
        fresh.current_commit,
        to_hex(&v2.id),
        "…against the version that actually moved"
    );
}

/// **The regression pin for the deleted inference.** An UNMARKED record whose durable documents
/// happen to satisfy the retired landed-exit pair — `map.applied_commit == current_commit` and
/// `map.materialized_sha == sync.work_hash`, with a `copy_dir` on the record — is a LIVE stopped
/// merge and must be treated as one: re-disclosed by the sweep, never cleared, and FINISHED (not
/// refused) by `--keep-mine`. A narrowed `--dest` reset produces exactly this document shape while
/// another copy still holds the merge, which is why liveness is the recorded mark and never a
/// document comparison.
#[test]
fn an_unmarked_record_matching_the_old_landed_pair_is_still_live() {
    let rig = Rig::new("old-pair-live");
    let (id, _name, genesis) = rig.adopt(BASE);
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

    // Plant exactly the pair the deleted inference read as "an exit already landed".
    let cs = rig.conflict_state(&id);
    assert!(cs.copy_dir.is_some() && cs.concluded.is_none());
    let work_hash = rig.read_sync(&id).work_hash;
    rig.patch_map(&id, |m| {
        m.applied_commit = cs.current_commit.clone();
        m.materialized_sha = work_hash.clone();
    });

    // The sweep re-discloses the live block — record and workbench both stand.
    let copy = rig.conflict_copy(&id);
    let workbench = snapshot(&copy);
    assert_eq!(
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted,
        "an unmarked record is live whatever the other documents say"
    );
    assert!(rig.conflict_exists(&id), "never cleared on a document pair");
    assert_eq!(
        snapshot(&copy),
        workbench,
        "the workbench survives untouched"
    );

    // …and the escape still FINISHES it, because the merge really is stopped.
    let escaped = pull_data(
        &rig.ctx(&plane, &foll),
        keep_mine_scope("pr-describe".into()),
    )
    .unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(!rig.conflict_exists(&id));
}

/// A `--reset` marks the record CONCLUDED only once every copy has PROVEN settled, so a marked-Reset
/// record describes exactly one state: the reset's placements landed and the clear did not. The next
/// bare sweep FINISHES it — the record and the untouched workbench go, the team's version stays on
/// disk, the discarded draft survives as a content-addressed store snapshot — and the sweep then
/// reads an ordinary current bundle. An edit made between the mark and the recovery is an ordinary
/// draft on the reset state: the clear still lands, and the edit is left exactly where it is.
#[test]
fn a_marked_reset_record_is_finished_by_the_next_sweep() {
    let rig = Rig::new("marked-reset-finish");
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

    // The workbench as topos wrote it, kept aside so the crash state can be re-planted byte for byte
    // (the copy carries the modes, so the restored folder still reads as UNTOUCHED).
    let cs = rig.conflict_state(&id);
    let copy = rig.conflict_copy(&id);
    let kept = rig.work.0.join("kept-workbench");
    copy_tree(&copy, &kept);

    // A REAL full reset first: the placements land, the settle proof passes, the mark goes down and
    // the clear takes the record with it. Re-planting that record marked is then exactly the state a
    // crash between those last two writes leaves — and the ONLY state a marked-Reset record can be in.
    ops::reset(
        &rig.ctx(&plane, &foll),
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(snapshot(&rig.placement()), Some(expect(V1)));
    assert!(!rig.conflict_exists(&id));
    let marked = topos_types::persisted::ConflictState {
        concluded: Some(topos_types::persisted::ConcludedExit::Reset),
        ..cs
    };
    let replant = || {
        copy_tree(&kept, &copy);
        doc::write_doc(
            &rig.fs,
            &rig.layout().published(&sid(&id)).conflict,
            &marked,
        )
        .unwrap();
    };

    replant();
    let row =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(
        row.action,
        PullAction::UpToDate,
        "the sweep finishes the reset and reads the bundle as current"
    );
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the team's version stands"
    );
    assert!(
        !rig.conflict_exists(&id),
        "the finished reset clears the block"
    );
    assert!(!copy.exists(), "and the untouched workbench with it");
    // The draft is never lost: the reset's snapshot rail committed it on the post-conflict base
    // (the team's version — what the conflict made the draft's recorded base).
    let stored = mk_version(&[v1.id], MINE_OVER_BASE, DEVICE, "topos: draft snapshot");
    assert!(
        rig.open_store(&id)
            .list_versions()
            .unwrap()
            .contains(&stored.id),
        "the discarded draft survives as a store snapshot"
    );

    // The same leftover, with the person back at work in the folder: the finish is a CLEAR and
    // nothing else, so an edit made after the mark is never overwritten and never lost.
    replant();
    let after: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# after the reset\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
    ];
    write_tree(&rig.placement(), after);
    let row =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(row.action, PullAction::UpToDate);
    assert!(
        !rig.conflict_exists(&id),
        "the clear lands over a re-edited folder too"
    );
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(after)),
        "an edit made after the mark is an ordinary draft — left exactly where it is"
    );
}

/// **A reset that cannot settle every copy leaves the merge LIVE.** The reset WRITES the current
/// plan's placements while the settled proof READS every recorded copy — so a copy the plan omits
/// (here one recorded for a harness this machine does not place into) keeps its edits through the
/// reset and the proof fails. Concluding the record before that proof would strand it: no later run
/// re-plans the omitted copy either, so nothing could ever settle it, and `--keep-mine` — which
/// routes a marked record through the finisher — could no longer end the merge. So an unproven reset
/// writes no mark at all: the record stays a LIVE stopped merge, re-disclosed by the sweep, with
/// both ways out still open.
#[test]
fn a_reset_that_cannot_settle_every_copy_leaves_the_merge_live() {
    let rig = Rig::new("reset-unsettled-live");
    let (id, name, genesis) = rig.adopt(BASE);
    let mine: FileSet = MINE_OVER_BASE;
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
    let copy = rig.conflict_copy(&id);

    // Two copies holding the same un-merged draft: the adopted folder, which every plan manages,
    // and a replica recorded for an agent this machine does not pick. The plan names the first;
    // the second is outside it (`placement::managed_indices`: frozen in place, never written,
    // never deleted).
    let replica = rig.work.0.join("replica");
    add_replica(&rig, &id, &replica, mine);
    rig.patch_map(&id, |m| {
        m.placement_state[1].agent = Some("cursor".to_owned());
    });
    crate::agents_pick::write_pick(
        &crate::agents_pick::machine_path(&rig.layout()),
        &["claude-code"],
    );
    let ctx = Ctx {
        roots: Some(crate::ctx::AgentRoots {
            home: rig.home.0.clone(),
            cwd: None,
        }),
        ..rig.ctx(&plane, &foll)
    };

    // The FULL reset — no selector, so it means "every copy" — but it can only reach the copy the
    // plan names.
    ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(V1)),
        "the planned copy is back at the team's version"
    );
    assert_eq!(
        snapshot(&replica),
        Some(expect(mine)),
        "the copy outside the plan is untouched — which is what the proof then reads"
    );
    assert!(
        rig.conflict_exists(&id),
        "a copy still holds the merge, so the record — the publish guard — must stand"
    );
    assert!(
        rig.conflict_state(&id).concluded.is_none(),
        "and it stands as a LIVE record: an unproven reset concludes nothing"
    );
    assert!(copy.exists(), "the workbench stands with it");

    // Live means live: the sweep re-discloses the block…
    assert_eq!(
        only(&pull_data(&ctx, ops::PullScope::AllFollowed).unwrap()).action,
        PullAction::Conflicted,
        "an unmarked record is re-disclosed, never finished away"
    );
    assert!(rig.conflict_exists(&id));

    // …and the `--keep-mine` the receipt names still FINISHES the merge, taking the record and the
    // workbench with the real conclusion.
    let escaped = pull_data(&ctx, keep_mine_scope(name)).unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert!(
        !rig.conflict_exists(&id),
        "the escape ends what the reset could not"
    );
    assert!(!copy.exists());
}

/// Copy a whole tree, modes included — how a by-hand fixture re-plants a folder exactly as topos
/// wrote it.
fn copy_tree(from: &Path, to: &Path) {
    let _ = std::fs::remove_dir_all(to);
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let dest = to.join(e.file_name());
        if e.path().is_dir() {
            copy_tree(&e.path(), &dest);
        } else {
            std::fs::copy(e.path(), &dest).unwrap();
        }
    }
}

/// **Every record this build writes NAMES its workbench** — the one field every read, write and
/// removal of the marked-up copy keys on. A recording path that forgot it would raise a block
/// whose markers nothing names: no folder on the disclosure, and none for the escape to read. So
/// it is pinned here rather than left to the two recording sites' good behaviour.
#[test]
fn every_conflict_record_this_build_writes_names_its_workbench() {
    // The three-way conflict.
    {
        let rig = Rig::new("copydir-threeway");
        let (id, _name, genesis) = rig.adopt(BASE);
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
        assert!(rig.conflict_state(&id).copy_dir.is_some());
    }
    // The 2-way no-base fallback.
    {
        let rig = Rig::new("copydir-nobase");
        let (id, _name, genesis) = rig.adopt(BASE);
        write_tree(&rig.placement(), MINE_OVER_BASE);
        rig.patch_lock(&id, |l| {
            l.base_commit = "f".repeat(64);
            l.bundle_digest = "e".repeat(64);
        });
        let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
        let mut plane = FixturePlane::default();
        plane.add_version(&id, &v1);
        plane.set_current(&id, served(WS, &id, v1.id, 1));
        let foll = follow(&id);
        assert_eq!(
            only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).action,
            PullAction::Conflicted
        );
        assert!(rig.conflict_state(&id).copy_dir.is_some());
    }
}

/// **A record naming NO folder names nothing — and the escape still finishes.** The field is
/// optional on the document and an unparseable value reads the same way, so the state is reachable
/// from a hostile checkout as well as from a truncated write. Nothing is derived from the bundle's
/// name to fill the gap: the block stays LIVE, the row names no folder, no folder under
/// `conflicts/` is written or removed, and `--keep-mine` concludes from what the placements hold —
/// the exit that must never deadlock.
#[test]
fn a_record_that_names_no_workbench_is_live_names_no_folder_and_still_escapes() {
    let rig = Rig::new("no-workbench-named");
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

    // Strip the one field that names a folder, leaving the folder itself on disk: a record that
    // names nothing must not write into, scan, or REMOVE anything under `conflicts/`.
    let copy = rig.conflict_copy(&id);
    let workbench = snapshot(&copy);
    let cs = rig.conflict_state(&id);
    doc::write_doc(
        &rig.fs,
        &rig.layout().published(&sid(&id)).conflict,
        &topos_types::persisted::ConflictState {
            copy_dir: None,
            ..cs
        },
    )
    .unwrap();

    // The sweep re-discloses a live block whose row names no folder.
    let row =
        only(&pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap()).clone();
    assert_eq!(row.action, PullAction::Conflicted);
    let mr = row.merge.as_ref().expect("the block is re-disclosed");
    assert_eq!(mr.copy_dir, None, "a record naming no folder names none");
    assert!(
        !mr.placements.is_empty(),
        "…and the placements it does name still hold this person's own version"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(MINE_OVER_BASE)));
    assert_eq!(
        snapshot(&copy),
        workbench,
        "nothing is written into a folder no record names"
    );

    // …and the escape concludes from the copies the folders hold.
    let escaped = pull_data(&rig.ctx(&plane, &foll), keep_mine_scope(name)).unwrap();
    assert_eq!(only(&escaped).action, PullAction::Merged);
    assert_eq!(snapshot(&rig.placement()), Some(expect(KEPT_OVER_V1)));
    assert!(!rig.conflict_exists(&id));
    assert_eq!(
        snapshot(&copy),
        workbench,
        "…and it removes no folder it never named"
    );
}

/// **`--keep-mine` commits the folder as it stands.** Publishing is blocked while a merge is
/// stopped, so "keep working, finish it later" is the expected sequence — and the exit used to
/// render the draft as of when the merge STOPPED and write it over every copy, reverting
/// everything done since with no word on the receipt. Git concludes a stopped merge from the
/// working tree; so does this.
#[test]
fn keep_mine_commits_the_folder_as_it_stands_not_the_conflict_time_snapshot() {
    let rig = Rig::new("keep-mine-live");
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

    // Kept working while the merge sat stopped: the contested file moved on, and a file the team
    // never touched was added.
    let later: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine, thought about\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
        ("notes.md", FileMode::Regular, b"kept working\n"),
    ];
    write_tree(&rig.placement(), later);

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
    // The work done since is IN, on top of the team's other changes.
    assert_eq!(
        snapshot(&rig.placement()),
        Some(expect(&[
            ("SKILL.md", FileMode::Regular, b"# mine, thought about\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
            ("notes.md", FileMode::Regular, b"kept working\n"),
            ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
        ])),
        "the exit commits the folder as it stands"
    );
    // And the committed version is that same tree — nothing to publish later that the folder
    // does not hold.
    let m = mk_version(
        &[v1.id],
        &[
            ("SKILL.md", FileMode::Regular, b"# mine, thought about\n"),
            ("notes.md", FileMode::Regular, b"kept working\n"),
            ("ref/notes.md", FileMode::Regular, b"new in v1\n"),
            ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v1\n"),
        ],
        DEVICE,
        "topos: merge escape",
    );
    assert!(rig.open_store(&id).list_versions().unwrap().contains(&m.id));
}

/// An accept RESOLVES against the version this very call discovered. `current` sits at v2 (whose
/// parent v1 the client never saw), the author's copy is edited, and the accept merges against v2 —
/// the version it raised — rather than deferring it to a second command.
#[test]
fn an_accept_resolves_against_a_version_raised_in_the_same_pull() {
    let rig = Rig::new("raised");
    let (id, _name, genesis) = rig.adopt(BASE);
    let edited: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# mine\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v0\n"),
    ];
    write_tree(&rig.placement(), edited);
    let foll = follow(&id);

    let v1 = mk_version(&[genesis], V1, "d_pub", "v1");
    let v2files: FileSet = &[
        ("SKILL.md", FileMode::Regular, b"# v2\n"),
        ("run.sh", FileMode::Executable, b"#!/bin/sh\necho v2\n"),
    ];
    let v2 = mk_version(&[v1.id], v2files, "d_pub", "v2");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.add_version(&id, &v2);
    plane.set_current(&id, served(WS, &id, v2.id, 2));

    let row = pull_data(
        &rig.ctx(&plane, &foll),
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "pr-describe".into(),
            workspace: None,
            mode: ops::TargetMode::AcceptPending,
        },
    )
    .unwrap();
    let row = only(&row);
    // Both sides edited SKILL.md, so the resolution conflicts — the point is that it RAN, against
    // the version the same call discovered.
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "an accept resolves a version raised in the same call, it does not defer it"
    );
    let mr = row.merge.as_ref().expect("a merge report");
    assert_eq!(mr.theirs_version_id, to_hex(&v2.id));
    assert_eq!(rig.read_sync(&id).observed, 2);
}

/// A `.topos-mine` sidecar must be disambiguated against the kernel's collision rule (NFC + case-fold),
/// not just exact bytes — otherwise a publisher-added path that case-folds to the sidecar name wedges the
/// resolution into a `Corrupt` digest error instead of a clean conflict.
#[test]
fn sidecar_avoids_case_fold_collision_with_a_real_path() {
    let rig = Rig::new("sidecar-collide");
    let base: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 1, 2])];
    let (id, _name, genesis) = rig.adopt(base);
    let mine: FileSet = &[("logo.bin", FileMode::Regular, &[0xffu8, 9, 9])];
    write_tree(&rig.placement(), mine);
    // theirs changes the binary AND adds a path that ASCII-case-folds to `logo.bin.topos-mine`.
    let theirs_files: FileSet = &[
        ("logo.bin", FileMode::Regular, &[0xffu8, 7, 7]),
        (
            "LOGO.BIN.TOPOS-MINE",
            FileMode::Regular,
            b"real theirs file\n",
        ),
    ];
    let v1 = mk_version(&[genesis], theirs_files, "d_pub", "v1");
    let mut plane = FixturePlane::default();
    plane.add_version(&id, &v1);
    plane.set_current(&id, served(WS, &id, v1.id, 1));
    let foll = follow(&id);

    let data = pull_data(&rig.ctx(&plane, &foll), ops::PullScope::AllFollowed).unwrap();
    let row = only(&data);
    assert_eq!(
        row.action,
        PullAction::Conflicted,
        "the binary conflict must resolve cleanly, not error on a digest collision"
    );
    // In the CONFLICT COPY: theirs at the path, theirs' real file kept, and mine's sidecar
    // DISAMBIGUATED away from the collision. (The placement, as always, keeps mine untouched.)
    let copy = rig.conflict_copy(&id);
    assert_eq!(
        std::fs::read(copy.join("logo.bin")).unwrap(),
        &[0xffu8, 7, 7]
    );
    assert!(copy.join("LOGO.BIN.TOPOS-MINE").exists());
    assert_eq!(
        std::fs::read(copy.join("logo.bin.topos-mine-1")).unwrap(),
        &[0xffu8, 9, 9],
        "the sidecar was disambiguated to avoid the case-fold collision"
    );
    assert_eq!(snapshot(&rig.placement()), Some(expect(mine)));
    // The written tree scans back to the recorded conflict digest (no kernel rejection).
    let cs: topos_types::persisted::ConflictState =
        doc::read_doc(&rig.fs, &rig.layout().published(&sid(&id)).conflict)
            .unwrap()
            .unwrap();
    let scanned = crate::scan::scan(&copy).unwrap();
    assert_eq!(to_hex(&scanned.bundle_digest), cs.conflicted_digest);
}
