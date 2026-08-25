//! Everything between a decision and the bytes. The manifest's own writer lock and the
//! compare-and-swap that refuses a file which moved under a built plan; the ONE candidate set a
//! bare name gathers before any decision (row vs set, set vs set, feed vs set); the visited-store
//! index's union under its own lock; what a pasted subtree URL records; a prior placement that is
//! memory and not permission; the forge refresh's lock and its stash-as-park; and the `-a`
//! selection the row carries into the next update.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::ctx::Ctx;
use crate::fs_seam::RealFs;
use crate::ops;

use super::rig::*;

// =================================================================================================
// Manifest mutations serialize on the file's own writer lock.
// =================================================================================================

#[test]
fn two_manifest_edits_through_the_locked_path_both_land() {
    // The lock is about SERIALIZATION, not detection: what must hold is that two edits of one file
    // — each a full read-modify-write — leave BOTH rows standing.
    let rig = Rig::new("manifest-lock");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    for name in ["alpha", "beta"] {
        match ops::add_reference(
            &ctx,
            &connect(&plane, &dir),
            None,
            &format!("@{WS_NAME}/{name}"),
            true,
            false,
            &Default::default(),
            None,
        )
        .unwrap()
        {
            ops::AddRefOutcome::Applied { .. } => {}
            ops::AddRefOutcome::Described { .. } => panic!("a workspace ref applies immediately"),
        }
    }
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("alpha"), "the first row survived: {text}");
    assert!(text.contains("beta"), "the second row landed: {text}");
    // The whole file still parses — a lock that let two writers interleave would not guarantee it.
    crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap_or_else(|e| panic!("{e}: {text}"));
}

#[test]
fn a_copy_edited_between_the_scan_and_the_delete_is_snapshotted_before_it_goes() {
    // The retiring sweep scans every placement, snapshots the edited ones, and only THEN deletes.
    // An edit that lands in that gap was captured by nothing — so the delete re-scans and
    // snapshots what is actually there. Nothing unsnapshotted disappears.
    let rig = Rig::new("clean-race");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let before = store_versions(&rig.layout(), "s_deploy");

    // The recipe stops asking for it — the next sweep retires the placement.
    rig.write_global("[skills]\n");
    // The racer edits the (clean) copy between the sweep's scan and its delete.
    let racing = placed.clone();
    // The seam: the retiring loop probes `exists(dir)` immediately before it acts on the dir —
    // AFTER the scan that classified every placement as clean. An edit landing there was captured
    // by no snapshot, so only a fresh read at the retirement can save it.
    let fs = crate::fs_seam::HookFs::before_nth_exists(&placed, 1, move || {
        std::fs::write(racing.join("SKILL.md"), b"# raced edit\n").unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    sweep(&ctx, &plane, &dir);
    assert!(!placed.exists(), "the undemanded placement is retired");
    assert_eq!(
        store_versions(&rig.layout(), "s_deploy"),
        before + 1,
        "the raced edit was committed into the store before the dir went"
    );
}

#[test]
fn a_copy_edited_in_the_instant_before_the_park_is_snapshotted_too() {
    // The sharper seam: an edit landing between the LAST check and the mutation itself. A
    // verify-then-delete has nothing left to check with there; park-then-verify does — the rename
    // takes the tree out of reach and the read that decides happens on bytes nobody can still be
    // writing. The hook fires immediately before that rename.
    let rig = Rig::new("clean-park-race");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let before = store_versions(&rig.layout(), "s_deploy");

    rig.write_global("[skills]\n");
    let racing = placed.clone();
    let fs = crate::fs_seam::HookFs::before_first_move_of(&placed, move || {
        std::fs::write(racing.join("SKILL.md"), b"# raced at the park\n").unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    sweep(&ctx, &plane, &dir);
    assert!(!placed.exists(), "the undemanded placement is retired");
    assert_eq!(
        store_versions(&rig.layout(), "s_deploy"),
        before + 1,
        "the edit that landed in the last instant was parked, read, and committed"
    );
    // No park is left behind once its bytes are safe.
    let leftovers: Vec<String> = std::fs::read_dir(placed.parent().unwrap())
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with(".topos-retiring-"))
                .collect()
        })
        .unwrap_or_default();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn a_copy_edited_in_the_instant_before_the_swap_is_snapshotted_too() {
    // The materializer's own park: an update lands new bytes over a clean copy, and the edit
    // arrives after the pre-swap re-stat — the one window a stat can never close. The swap PARKS
    // the old tree instead of deleting it, so the bytes that were really there are read and
    // committed before anything is dropped.
    let rig = Rig::new("swap-park-race");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n"
    ));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let before = store_versions(&rig.layout(), "s_deploy");

    // v2 is served (a real pointer move — the next generation); the copy on disk is CLEAN, so the
    // sweep will overwrite it.
    let mut moved = delivered("s_deploy", "deploy", &v2);
    moved.generation = 2;
    plane.serves(vec![moved]);
    let racing = placed.clone();
    // The materializer operates on the CANONICAL dir (a symlinked temp prefix is resolved), which
    // is the path the swap actually names.
    let swapped = placed.canonicalize().unwrap();
    let fs = crate::fs_seam::HookFs::before_first_move_of(&swapped, move || {
        std::fs::write(racing.join("SKILL.md"), b"# raced at the swap\n").unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    sweep(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the new bytes landed"
    );
    assert_eq!(
        store_versions(&rig.layout(), "s_deploy"),
        before + 2,
        "the served version AND the raced edit are both in the store"
    );
}

// =================================================================================================
// A bare name that several LINES answer to is refused — one candidate set, gathered before any
// decision (row vs set, set vs set, feed vs set).
// =================================================================================================

#[test]
fn a_bare_name_a_row_and_a_set_both_answer_to_is_refused_not_guessed() {
    // ROW vs SET. Dropping the row leaves the channel delivering `deploy`; splitting the channel
    // leaves the row delivering it. Picking either silently is a removal that does not remove.
    let rig = Rig::new("ambig-row-set");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &d);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &d)],
        vec![channel("backend", &[("s_deploy", "deploy")])],
    );
    rig.write_global(&format!(
        "[skills]\ndeploy = \"github:o/r\"\n\n[channels]\n\"{HOST}/{WS_NAME}/backend\" = \
         \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = ways_out(&err);
    assert!(msg.contains("github.com/o/r/deploy"), "{msg}");
    assert!(msg.contains("channels/backend"), "{msg}");

    // The EXACT spelling is an answer, never an ambiguity — otherwise the refusal would be a dead
    // end (a set member has no spelling of its own).
    let one = "github.com/o/r/deploy".to_owned();
    assert!(matches!(
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &[one],
            None,
            true,
            &Default::default()
        )
        .unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("github:o/r"), "{text}");
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/backend\"")),
        "the set line stands: {text}"
    );
}

#[test]
fn a_bare_name_two_sets_carry_is_refused_not_split_at_random() {
    // SET vs SET — the fault the old first-match resolution hid completely: whichever channel
    // expanded first got split, and the other went on delivering the bundle.
    let rig = Rig::new("ambig-set-set");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &d);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &d)],
        vec![
            channel("backend", &[("s_deploy", "deploy")]),
            channel("platform", &[("s_deploy", "deploy")]),
        ],
    );
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/backend\" = \"latest\"\n\
         \"{HOST}/{WS_NAME}/platform\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = ways_out(&err);
    assert!(msg.contains("channels/backend"), "{msg}");
    assert!(msg.contains("channels/platform"), "{msg}");
    // And the candidates are PASTE-READY. A set member has no spelling of its own, so listing the
    // two references alone would be a dead end — each candidate is the EXACT `--via` invocation
    // that selects that one line's rewrite.
    assert!(
        msg.contains(&format!(
            "{HOST}/{WS_NAME}/deploy --via {HOST}/{WS_NAME}/channels/backend"
        )),
        "{msg}"
    );
    assert!(
        msg.contains(&format!(
            "{HOST}/{WS_NAME}/deploy --via {HOST}/{WS_NAME}/channels/platform"
        )),
        "{msg}"
    );
    // Each is a whole COMMAND — `topos remove <reference> --via <line>`, the verb that refused.
    assert!(
        msg.lines()
            .filter(|l| l.trim_start().starts_with("topos remove "))
            .count()
            == 2,
        "one runnable line per candidate: {msg}"
    );
    // And the refusal SENTENCE no longer inlines them: a paste-ready invocation buried in prose
    // read as prose, and the `--via` form's token boundary vanished into the comma list.
    let sentence = err.to_string();
    assert!(!sentence.contains("--via"), "{sentence}");
    // Neither line was touched.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/backend\""))
            && text.contains(&format!("\"{HOST}/{WS_NAME}/platform\"")),
        "{text}"
    );
}

#[test]
fn a_via_reference_splits_exactly_the_line_it_names() {
    // The ANSWER the set-versus-set refusal offers. `--via` names the line, so the removal is a
    // SELECTION, not a search: that one line's member-minus-one rewrite lands, and the other line
    // — never the target — stands whole, still delivering what it carries.
    let rig = Rig::new("via-picks");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &d)
        .with_version("s_beta", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &d),
            catalog_entry("s_beta", "beta", &b),
        ],
        vec![
            channel("backend", &[("s_deploy", "deploy"), ("s_beta", "beta")]),
            channel("platform", &[("s_deploy", "deploy")]),
        ],
    );
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/backend\" = \"latest\"\n\
         \"{HOST}/{WS_NAME}/platform\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let via = format!("{HOST}/{WS_NAME}/channels/backend");

    // A split is GATED whichever way it was selected — it rewrites a curated line — so the bare
    // run describes, and the re-run it prints carries the `--via` that made it unambiguous.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        Some(&via),
        false,
        &Default::default(),
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { yes_argv, .. } => {
            assert!(yes_argv.iter().any(|a| a == "--via"), "{yes_argv:?}");
            assert!(yes_argv.contains(&via), "{yes_argv:?}");
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    // Nothing was written by the describe.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/backend\"")),
        "{text}"
    );

    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        Some(&via),
        true,
        &Default::default(),
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)), "{out:?}");

    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap();
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference.contains("channels/backend")),
        "the NAMED line split: {text}"
    );
    assert!(
        doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/beta")),
        "its surviving member got its own row: {text}"
    );
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/deploy")),
        "the removed member gets no row: {text}"
    );
    assert!(
        doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/channels/platform")),
        "the line `--via` did NOT name is untouched: {text}"
    );
}

#[test]
fn a_via_that_names_no_line_or_no_member_refuses_typed() {
    // `--via` is a selection, so each MISS is its own typed refusal — never a fall-through to the
    // arms the flag does not select (a bare resolution would happily answer something else, and
    // the person would watch a removal land somewhere they did not name).
    let rig = Rig::new("via-misses");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let o = one_file(b"# other\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &d)
        .with_version("s_other", &o);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &d),
            catalog_entry("s_other", "other", &o),
        ],
        vec![
            channel("backend", &[("s_deploy", "deploy")]),
            channel("platform", &[("s_other", "other")]),
        ],
    );
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/backend\" = \"latest\"\n\
         \"{HOST}/{WS_NAME}/platform\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let backend = format!("{HOST}/{WS_NAME}/channels/backend");

    // (i) A line this file does not carry — named, so the refusal names it back.
    let absent = format!("{HOST}/{WS_NAME}/channels/nosuch");
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        Some(&absent),
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err:?}");
    assert!(err.to_string().contains(&absent), "{err}");

    // (ii) A real line the token does not come from. `other` IS removable here — the platform
    // line carries it — which is exactly why the flag must refuse rather than quietly split that
    // other line; the refusal lists the named line's current members so the retry is obvious.
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["other".into()],
        Some(&backend),
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err:?}");
    let msg = err.to_string();
    assert!(msg.contains("'other'"), "{msg}");
    assert!(msg.contains(&backend), "{msg}");
    assert!(msg.contains("current members: deploy"), "{msg}");

    // Both misses wrote NOTHING.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/backend\""))
            && text.contains(&format!("\"{HOST}/{WS_NAME}/platform\"")),
        "{text}"
    );
}

#[test]
fn a_bare_name_the_feed_and_a_repo_set_both_deliver_is_refused() {
    // FEED vs SET: the feed's `"off"` switch used to answer first, so a repo set carrying the same
    // name never even got looked at.
    let rig = Rig::new("ambig-feed-set");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &d);
    plane.serves(vec![delivered("s_deploy", "deploy", &d)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &d)], Vec::new());
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/deploy/SKILL.md", b"# repo deploy\n")],
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // The repo import is a real tracked member of the home store (the set's expansion reads it).
    gate_add(&ctx, &plane, &dir, &git, "o/r/deploy");
    // Both lines stand: the workspace feed AND the repo set.
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n\n[skills]\ndeploy = \"github:o/r\"\n"
    ));

    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = ways_out(&err);
    assert!(msg.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{msg}");
    assert!(msg.contains("github.com/o/r/deploy"), "{msg}");
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("\"off\""), "no switch was written: {text}");
}

// =================================================================================================
// A manifest that moves between the decision and the write refuses — no plan built from bytes
// that are gone.
// =================================================================================================

#[test]
fn a_manifest_edited_between_the_decision_and_the_write_refuses() {
    // The set split is the sharpest case: its survivor rows are computed FROM the file (which
    // members already have their own row), so a row someone else adds in between would be
    // overwritten by a line rebuilt from the older reading.
    let rig = Rig::new("changed-underfoot");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
        ],
        vec![channel("backend", &[("s_a", "alpha"), ("s_b", "beta")])],
    );
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let line = format!("\"{HOST}/{WS_NAME}/backend\" = \"latest\"\n");
    rig.write_global(&format!("[channels]\n{line}"));

    // The racer: an outside editor writes beta's own row AFTER the arms were resolved and before
    // they are re-proven (topos's own writers serialize on the file's lock; a person's editor does
    // not).
    let racing = manifest.clone();
    let raced = format!("[channels]\n{line}\n[skills]\n\"{HOST}/{WS_NAME}/beta\" = \"latest\"\n");
    let fs = crate::fs_seam::HookFs::before_nth_read(&manifest, 2, move || {
        std::fs::write(&racing, &raced).unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["alpha".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_CHANGED", "{err:?}");

    // NOTHING was written: the racer's row stands, the set line stands, no split landed.
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/backend\"")),
        "{text}"
    );
    assert!(text.contains(&format!("{HOST}/{WS_NAME}/beta")), "{text}");
    assert!(
        !text.contains(&format!("{HOST}/{WS_NAME}/alpha")),
        "no stale survivor row was written: {text}"
    );

    // Re-run against the file as it now is: the split honours beta's own row and applies.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    assert!(matches!(
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &["alpha".into()],
            None,
            true,
            &Default::default()
        )
        .unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        !text.contains(&format!("\"{HOST}/{WS_NAME}/backend\"")),
        "the line split: {text}"
    );
    assert!(text.contains(&format!("{HOST}/{WS_NAME}/beta")), "{text}");
}

#[test]
fn the_reproof_and_the_editor_read_one_document() {
    // The STRUCTURAL half of the refusal above. The reproof is only worth anything if it holds for
    // the EXACT document instance the write then emits — proving against one read and editing a
    // second leaves a window where an outside edit is proven against but not edited (or edited but
    // not proven against). So the apply path reads the manifest ONCE and hands that text to both;
    // the only read after it is the editor write's own PRE-RENAME COMPARE (the CAS — proven at
    // the boundary by the test below). The witness is an absence: after the plan's read there is
    // the apply read and the compare read, never a fourth.
    let rig = Rig::new("one-read");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
        ],
        vec![channel("backend", &[("s_a", "alpha"), ("s_b", "beta")])],
    );
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/backend\" = \"latest\"\n"
    ));

    // The tripwire sits on the read that would OPEN that window — the fourth. It never fires.
    let fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = std::sync::Arc::clone(&fired);
    let fs = crate::fs_seam::HookFs::before_nth_read(&manifest, 4, move || {
        flag.store(true, Ordering::Relaxed);
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["alpha".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)), "{out:?}");
    assert!(
        !fired.load(Ordering::Relaxed),
        "the apply path reads the manifest ONCE for the reproof AND the editor, plus the write's \
         one pre-rename compare — a fourth read IS the window"
    );

    // And the split it proved is the split it wrote.
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        !text.contains(&format!("\"{HOST}/{WS_NAME}/backend\"")),
        "the line split: {text}"
    );
    assert!(text.contains(&format!("{HOST}/{WS_NAME}/beta")), "{text}");
}

#[test]
fn an_edit_landing_at_the_write_rename_boundary_is_refused_by_the_compare_and_swap() {
    // The reproof (the test two above) closes the decision half; this closes the WRITE half. The
    // outside edit lands at the LATEST catchable instant — after the arms were re-proven and the
    // editor's document was STAGED (the temp is already on disk when the hook fires: the proof
    // this injection sits at the write/rename boundary, not earlier), immediately before the
    // pre-rename compare. The compare-and-swap must refuse: typed MANIFEST_CHANGED, the staged
    // document discarded, the outside writer's bytes untouched on disk.
    let rig = Rig::new("cas-boundary");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
        ],
        vec![channel("backend", &[("s_a", "alpha"), ("s_b", "beta")])],
    );
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let line = format!("\"{HOST}/{WS_NAME}/backend\" = \"latest\"\n");
    rig.write_global(&format!("[channels]\n{line}"));

    // Read 1 = the plan/arms, read 2 = the apply's one document (reproof + editor), read 3 = the
    // write's pre-rename compare. The racer fires immediately before read 3.
    let tmp = crate::atomic::temp_path(&manifest);
    let staged_when_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let staged_flag = std::sync::Arc::clone(&staged_when_fired);
    let racing = manifest.clone();
    let raced = format!("[channels]\n{line}\n[skills]\n\"{HOST}/{WS_NAME}/beta\" = \"latest\"\n");
    let tmp_probe = tmp.clone();
    let fs = crate::fs_seam::HookFs::before_nth_read(&manifest, 3, move || {
        staged_flag.store(tmp_probe.exists(), Ordering::Relaxed);
        std::fs::write(&racing, &raced).unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["alpha".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_CHANGED", "{err:?}");
    assert!(
        staged_when_fired.load(Ordering::Relaxed),
        "the staged temp must already exist when the edit lands — the injection sits at the \
         write/rename boundary, after every earlier rail has passed"
    );

    // NOTHING was overwritten: the outside writer's document stands byte-for-byte, and the
    // staged temp was discarded.
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}/backend\"")),
        "{text}"
    );
    assert!(text.contains(&format!("{HOST}/{WS_NAME}/beta")), "{text}");
    assert!(!tmp.exists(), "the staged document was discarded");

    // The re-run reads the file as it now is and applies (honouring beta's own row).
    let ctx = rig.ctx_at(Some(&rig.work.0));
    assert!(matches!(
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &["alpha".into()],
            None,
            true,
            &Default::default()
        )
        .unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
}

#[test]
fn a_manifest_birth_racing_an_outside_writer_refuses_manifest_exists() {
    // The BIRTH half of the outside-writer window: `add -g` on a machine with NO global file
    // stages the materialized seed, and an outside editor lands its own file at the last
    // catchable instant — after the absence check, immediately before the no-replace rename. A
    // birth is a claim the file does not exist; the exclusive create refuses typed
    // MANIFEST_EXISTS, the outside document stands byte-for-byte, and the staged seed is
    // discarded — never an overwrite.
    let rig = Rig::new("birth-race");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    assert!(!manifest.exists(), "the race needs a birth, not an edit");
    let tmp = crate::atomic::temp_path(&manifest);
    let outside = "# an outside editor's file\n[skills]\nmine = \"./mine\"\n";
    let racing = manifest.clone();
    let fs = crate::fs_seam::HookFs::before_first_move_of(&tmp, move || {
        std::fs::write(&racing, outside).unwrap();
    });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let err = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &Default::default(),
        None,
    ) {
        Err(e) => e,
        Ok(_) => panic!("the racing birth must refuse, never land over the outside file"),
    };
    assert_eq!(err.code(), "MANIFEST_EXISTS", "{err:?}");
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        outside,
        "the outside document stands byte-for-byte"
    );
    assert!(!tmp.exists(), "the staged birth seed was discarded");
}

// =================================================================================================
// The visited-store index unions under its own lock.
// =================================================================================================

#[test]
fn the_visited_store_index_unions_under_its_own_lock() {
    // Two sweeps from two checkouts are the ordinary case. Each one's union is built from the
    // bytes it read, so without a lock across read → union → write the second writer's document
    // simply does not contain the first writer's checkout, and that checkout's holdings drop out
    // of every later applied report.
    let rig = Rig::new("visited-lock");
    let a = project("visited-lock-a", "[skills]\n");
    let b = project("visited-lock-b", "[skills]\n");
    crate::sidecar::ensure_project_store(&rig.fs, &a.0).unwrap();
    crate::sidecar::ensure_project_store(&rig.fs, &b.0).unwrap();

    // Run one records checkout A.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    assert_eq!(
        crate::visited_stores::recall_and_record(&ctx, std::slice::from_ref(&a.0)).len(),
        1
    );

    // Run two records checkout B — and a would-be concurrent writer probing the lock at the moment
    // between the read and the write finds it HELD.
    let lock_path = rig.layout().visited_stores_lock_file();
    let free_in_the_window = std::cell::Cell::new(true);
    let fs =
        crate::fs_seam::HookFs::before_nth_create_dir_all(&rig.layout().state_dir(), 1, || {
            let taken = crate::fs_seam::FsOps::try_lock_exclusive(&RealFs, &lock_path)
                .map(|g| g.is_none())
                .unwrap_or(true);
            free_in_the_window.set(!taken);
        });
    let ctx = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let layouts = crate::visited_stores::recall_and_record(&ctx, std::slice::from_ref(&b.0));
    assert!(
        !free_in_the_window.get(),
        "the index's writer lock is held across its whole read-union-write"
    );
    assert_eq!(layouts.len(), 2, "both checkouts survive the union");

    // …and the document itself holds both, for every later report.
    let doc: crate::visited_stores::VisitedStores =
        crate::doc::read_doc(&rig.fs, &rig.layout().visited_stores_path())
            .unwrap()
            .unwrap();
    assert!(doc.stores.contains(&a.0.display().to_string()), "{doc:?}");
    assert!(doc.stores.contains(&b.0.display().to_string()), "{doc:?}");
}

// =================================================================================================
// A pasted subtree URL records a SKILL row — and nothing is granted when the row would refuse.
// =================================================================================================

#[test]
fn a_subtree_url_records_a_skill_row_carrying_the_literal_path() {
    // `/tree/<ref>/<path>` canonicalizes to the REPO, and a repo-set row cannot legally carry
    // `subdir` or `version` — so the write refused AFTER the origin had been trusted. The path
    // names one skill: it records the 4-segment row whose fields take both.
    let rig = Rig::new("tree-url");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("tools/find-skills/SKILL.md", b"# find\n"),
            ("skills/other/SKILL.md", b"# other\n"),
        ],
    ));
    let url = "https://github.com/o/r/tree/main/tools/find-skills";
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        url,
        true,
        true,
        &Default::default(),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied { .. } => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap_or_else(|e| panic!("{e}: {text}"));
    let row = doc
        .rows
        .iter()
        .find(|r| r.reference == "github.com/o/r/find-skills")
        .unwrap_or_else(|| panic!("the subtree records a skill row: {text}"));
    match &row.value {
        crate::manifest::document::EntryValue::Fields(f) => {
            assert_eq!(f.subdir.as_deref(), Some("tools/find-skills"), "{text}");
        }
        other => panic!("the literal path rides the subdir field, got {other:?}: {text}"),
    }
    assert!(
        rig.home
            .0
            .join(".claude/skills/find-skills/SKILL.md")
            .exists(),
        "the selected subtree landed"
    );
}

#[test]
fn a_subtree_url_naming_several_skills_writes_nothing() {
    // The row is PROVEN before anything lands: a subtree that names no single skill refuses with
    // the names, and no manifest file is born for a row that cannot legally exist.
    let rig = Rig::new("tree-url-many");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n"),
            ("skills/beta/SKILL.md", b"# beta\n"),
        ],
    ));
    let err = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "https://github.com/o/r/tree/main/skills",
        true,
        true,
        &Default::default(),
        None,
    ) {
        Err(e) => e,
        Ok(_) => panic!("a subtree naming several skills names none of them"),
    };
    assert_eq!(err.code(), "AMBIGUOUS_SKILL", "{err:?}");
    assert!(
        !rig.layout()
            .home()
            .join(crate::manifest::MANIFEST_FILE)
            .exists(),
        "and no file was born for a row that cannot exist"
    );
}

// =================================================================================================
// A prior placement record is a memory, not a permission.
// =================================================================================================

#[test]
fn a_prior_placement_that_no_longer_resolves_inside_the_checkout_is_refused() {
    // The record was written when `.agents` was a plain directory; the checkout has since
    // committed it as a symlink out of the tree. A lexical `starts_with` still says "inside" —
    // only the containment proof catches it, and it must catch it on REUSED paths too.
    let rig = Rig::new("prior-escape");
    let proj = project("prior-escape-proj", "[skills]\n");
    let outside = Scratch::new("prior-escape-outside");
    std::fs::create_dir_all(outside.0.join("skills/deploy")).unwrap();
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".claude")).unwrap();
    let recorded = proj.0.join(".claude/skills/deploy");

    let prior = topos_types::persisted::PlacementMap {
        schema_version: topos_types::PLACEMENT_MAP_SCHEMA_VERSION,
        placements: vec![recorded.display().to_string()],
        applied_commit: "b".repeat(64),
        placement_state: vec![topos_types::persisted::PlacementState {
            kind: topos_types::persisted::PlacementKind::Native,
            agent: Some("claude-code".to_owned()),
            materialized_sha: Some("a".repeat(64)),
            pre_existing_sha: None,
            swap_capability: topos_types::persisted::SwapCapability::AtomicExchange,
            adopted_source: false,
            claim: None,
        }],
        materialized_sha: "a".repeat(64),
        harness: None,
        harness_slug: None,
    };

    let ctx = rig.ctx_at(Some(&proj.0));
    let plan = crate::placement::project_plan(
        &ctx,
        &proj.0,
        "topos_deadbeef",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: Some(WS_NAME),
        },
        Some(&prior),
        None,
    );
    assert!(
        crate::message::legacy_lines(&plan.refused)
            .into_iter()
            .any(|r| r.starts_with("PLACEMENT_ESCAPES_PROJECT")),
        "the escaping record is refused: {:?}",
        plan.refused
    );
    assert!(
        plan.dirs().all(|t| !t.dir.starts_with(&outside.0)),
        "the symlink is never followed: {:?}",
        plan.targets
    );
}

// =================================================================================================
// The forge refresh: under the skill's own lock, and its stash is a PARK it reads before deleting.
// =================================================================================================

#[test]
fn a_forge_refresh_holds_the_lock_and_keeps_an_edit_that_lands_at_the_stash() {
    // The refresh reads the map, classifies every placement, then moves those dirs and the sidecar
    // record aside — a sequence a second writer must not cross, and a classification that is only
    // a claim about a directory anyone could still be editing. So: the lock is held for the whole
    // replacement, and the stash is re-read AS A PARK before anything is dropped.
    let rig = Rig::new("refresh-park");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/deploy/SKILL.md", b"# deploy v1\n")],
    ));
    gate_add(&ctx, &plane, &dir, &git, "o/r/deploy");
    let placed = rig.home.0.join(".claude/skills/deploy");
    assert!(placed.join("SKILL.md").exists());
    let imports = crate::ops::forge_imports(&ctx);
    let sid = imports
        .first()
        .map(|i| i.sid.clone())
        .expect("the import is tracked");
    let recorded = std::fs::read_to_string(
        rig.layout()
            .home()
            .join("skills")
            .join(sid.as_str())
            .join("origin.json"),
    )
    .unwrap();

    // The source moved — the refresh is what this update would run.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/deploy/SKILL.md", b"# deploy v2\n")],
    ));
    let lock_path = rig.layout().lock_file(&sid);
    let lock_free = std::cell::Cell::new(true);
    let racing = placed.canonicalize().unwrap();
    let editing = racing.clone();
    let fs = crate::fs_seam::HookFs::before_first_move_of(&racing, || {
        // (a) the replacement runs under the skill's writer lock…
        let taken = crate::fs_seam::FsOps::try_lock_exclusive(&RealFs, &lock_path)
            .map(|g| g.is_none())
            .unwrap_or(true);
        lock_free.set(!taken);
        // (b) …and the person's edit lands in the last instant before the stash.
        std::fs::write(editing.join("SKILL.md"), b"# my local edit\n").unwrap();
    });
    let hooked = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let out = ops::manifest_update(
        &hooked,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();

    assert!(
        !lock_free.get(),
        "the whole refresh replacement runs under the skill's writer lock"
    );
    // The refresh stood down, and said so as a DECISION — not a failure. Nothing broke, nothing
    // was lost, and no retry answers it: the person picks, and until they do the run is still a
    // clean one.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(out.decisions.len(), 1, "{:?}", out.decisions);
    assert_eq!(out.decisions[0].name, "deploy");
    assert_eq!(
        out.decisions[0].line,
        "github.com/o/r has a newer version, and taking it would overwrite your edits. Keep them \
         by doing nothing, or run 'topos update -g deploy --reset' to discard them and take the \
         new version."
    );
    // ONE way out, and it is the only one that runs: a skill imported from a repository reaches
    // people who have no workspace at all, and `topos publish` refuses them at the login step.
    // Keeping the edits needs no command — nothing was overwritten.
    assert_eq!(
        out.decisions[0].detail,
        vec!["to discard them:   topos update -g deploy --reset".to_owned()]
    );
    assert!(
        !out.decisions[0]
            .ways_out
            .iter()
            .any(|w| w.iter().any(|t| t == "publish")),
        "{:?}",
        out.decisions[0].ways_out
    );
    // The whole receipt block, as a person reads it: one row, its way out, and a summary that
    // counts the row under the answer it is waiting for.
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    let expected_block = concat!(
        "deploy   github.com/o/r has a newer version, and taking it would overwrite your edits. ",
        "Keep them by doing nothing, or run 'topos update -g deploy --reset' to discard them and ",
        "take the new version.\n",
        "    to discard them:   topos update -g deploy --reset\n",
    );
    assert!(tty.contains(expected_block), "{tty}");
    assert!(!tty.contains("topos publish"), "{tty}");
    assert!(tty.contains("waiting on you"), "{tty}");
    // Neither internal code survives: not the refusal's, and not the source-moved disclosure's —
    // the row already says the source has a newer version, in words a person can act on.
    assert!(!tty.contains("INVALID_ARGUMENT"), "{tty}");
    assert!(!tty.contains("GIT_UPDATED"), "{tty}");
    assert!(!tty.contains('~'), "{tty}");
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# my local edit\n",
        "the edit that arrived after the scan is restored, never deleted"
    );
    assert_eq!(
        std::fs::read_to_string(
            rig.layout()
                .home()
                .join("skills")
                .join(sid.as_str())
                .join("origin.json"),
        )
        .unwrap(),
        recorded,
        "the old import is intact — a refused refresh replaces nothing"
    );
}

// =================================================================================================
// A `-a` selector is a standing placement decision: the row carries it, and the update honours it.
// =================================================================================================

#[test]
fn a_selector_imports_harness_choice_rides_the_row_into_the_next_update() {
    // Without the field, the next commit move re-lands the copy through the DEFAULT agent dir and
    // the person's `-a` choice quietly evaporates.
    let rig = Rig::new("harness-row");
    // THE PREMISE: codestudio is one of this machine's agents, so `-a codestudio` may name it.
    rig.pick(&["claude-code", "codestudio"]);
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/deploy/SKILL.md", b"# deploy v1\n")],
    ));
    match ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &["deploy".to_owned()],
        &["codestudio".to_owned()],
        &[],
        true,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(items) => assert_eq!(items.len(), 1),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    let chosen = rig.home.0.join(".codestudio/skills/deploy");
    assert!(chosen.join("SKILL.md").exists(), "the selector's dir");

    // The ROW carries the selection.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap_or_else(|e| panic!("{e}: {text}"));
    let row = doc
        .rows
        .iter()
        .find(|r| r.reference == "github.com/o/r/deploy")
        .unwrap_or_else(|| panic!("the import records its row: {text}"));
    match &row.value {
        // The `-a` selection is recorded as the agent's DEST DIR (default spelling) — the row
        // says where the copy lives, not which slug once resolved it.
        crate::manifest::document::EntryValue::Fields(f) => assert_eq!(
            f.dest.as_deref(),
            Some(&["~/.codestudio/skills".to_owned()][..]),
            "{text}"
        ),
        other => panic!("the dest selection rides the row, got {other:?}: {text}"),
    }

    // The source moves: the copy converges WHERE IT WAS ASKED FOR, and no default-dir copy appears.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/deploy/SKILL.md", b"# deploy v2\n")],
    ));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        out.warnings.is_empty(),
        "a landed refresh fails nothing: {:?}",
        out.warnings
    );
    assert!(
        crate::message::legacy_lines(&out.disclosures)
            .into_iter()
            .all(|w| w.starts_with("GIT_UPDATED")),
        "the moved source is disclosed and nothing else: {:?}",
        out.disclosures
    );
    assert_eq!(
        std::fs::read(chosen.join("SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the selected harness keeps the copy: {:?}",
        out.data.skills
    );
    assert!(
        !rig.home.0.join(".claude/skills/deploy").exists(),
        "and nothing was re-imported into the default agent dir"
    );
}
