//! The SCOPE line over the targeted verbs. Every verb acts on WHERE YOU STAND and `-g` means the
//! machine, no verb crossing that line on its own; a bundle a project file delivers keeps its
//! engine state in the checkout's own store, so every by-name read has to look there; the scope
//! flag governs the modes that never touch the reconcile too (a go-back, `--reset`); and
//! adopted-in-place custody is never the sweep's to destroy.

use std::sync::{Arc, Mutex};

use topos_types::results::{ExchangeFault, PullAction};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::ops;
use crate::sessions::Session;

use super::rig::*;

// =================================================================================================
// The SCOPE line: every verb acts on WHERE YOU STAND, `-g` means the machine, and no verb crosses
// that line on its own.
// =================================================================================================

/// One error's next actions as `(code, argv)` pairs — the machine surface of a refusal.
fn scope_ways_out(command: &str, argv: &[&str], err: &ClientError) -> Vec<(String, Vec<String>)> {
    let argv: Vec<String> = argv.iter().map(|t| (*t).to_owned()).collect();
    crate::render::err_envelope(command, &argv, err)
        .next_actions
        .into_iter()
        .map(|a| (a.code.as_str().to_owned(), a.argv))
        .collect()
}

#[test]
fn a_bare_edit_never_writes_the_machine_file_and_a_g_edit_never_a_project_one() {
    let rig = Rig::new("zq-scope-property");
    let proj = project("zq-scope-property-proj", "");
    let ctx = rig.ctx_at(Some(&proj.0));
    let global_path = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let project_path = proj.0.join(crate::manifest::MANIFEST_FILE);

    // BARE: the row lands in THIS folder's file, and the machine-wide file is never even born.
    let inside = proj.0.join("zq-inside-src");
    skill_source(&inside, b"# zq-inside\n");
    let added = scoped_path_add(&ctx, &inside, false).unwrap();
    assert_eq!(
        added.manifest.as_deref(),
        Some(project_path.to_str().unwrap())
    );
    assert_eq!(added.reference.as_deref(), Some("./zq-inside-src"));
    assert!(
        !global_path.exists(),
        "a bare add never touches the machine-wide file"
    );

    // `-g`: the row lands in the machine-wide file, and THIS folder's is byte-identical after.
    let outside = rig.work.0.join("zq-outside-src");
    skill_source(&outside, b"# zq-outside\n");
    let before = std::fs::read_to_string(&project_path).unwrap();
    let g_added = scoped_path_add(&ctx, &outside, true).unwrap();
    assert_eq!(
        g_added.manifest.as_deref(),
        Some(global_path.to_str().unwrap())
    );
    assert_eq!(
        std::fs::read_to_string(&project_path).unwrap(),
        before,
        "a `-g` add never touches a project file"
    );
    let global_text = std::fs::read_to_string(&global_path).unwrap();
    assert!(
        global_text.contains(outside.to_str().unwrap()),
        "{global_text}"
    );

    // The INVERSES obey the same line: each removal edits only the file its scope names.
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let session_connect = connect(&plane, &dir);
    let global_before = std::fs::read_to_string(&global_path).unwrap();
    ops::remove_project(
        &ctx,
        &session_connect,
        &["./zq-inside-src".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap()
    .expect("the project row is a manifest arm");
    assert!(
        !std::fs::read_to_string(&project_path)
            .unwrap()
            .contains("zq-inside-src"),
        "the project row is gone"
    );
    assert_eq!(
        std::fs::read_to_string(&global_path).unwrap(),
        global_before,
        "the project remove never touched the machine-wide file"
    );

    let project_before = std::fs::read_to_string(&project_path).unwrap();
    ops::remove_global(
        &ctx,
        &session_connect,
        &[outside.to_str().unwrap().to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    assert!(
        !std::fs::read_to_string(&global_path)
            .unwrap()
            .contains("zq-outside-src"),
        "the machine-wide row is gone"
    );
    assert_eq!(
        std::fs::read_to_string(&project_path).unwrap(),
        project_before,
        "the `-g` remove never touched a project file"
    );
}

#[test]
fn a_project_add_with_no_manifest_refuses_and_lands_nothing() {
    let rig = Rig::new("zq-nomanifest-add");
    // A folder no `topos.toml` covers — and no `.git` either, so nothing invents a repo root.
    let bare = Scratch::new("zq-nomanifest-cwd");
    let ctx = rig.ctx_at(Some(&bare.0));
    let src = bare.0.join("zq-nomanifest-src");
    skill_source(&src, b"# zq-nomanifest\n");

    let err = scoped_path_add(&ctx, &src, false).unwrap_err();
    assert_eq!(err.code(), "NO_MANIFEST");
    // Nothing landed: no manifest at either scope, no project store, no store entry.
    assert!(!bare.0.join(crate::manifest::MANIFEST_FILE).exists());
    assert!(
        !rig.layout()
            .home()
            .join(crate::manifest::MANIFEST_FILE)
            .exists()
    );
    assert!(
        !bare.0.join(".topos").exists(),
        "the project store is never minted by a refused add"
    );
    assert!(
        std::fs::read_dir(rig.layout().skills_dir())
            .map(|d| d.count())
            .unwrap_or(0)
            == 0,
        "the home store holds nothing"
    );

    // The two ways out ride as executable actions, and the retry is the caller's OWN invocation
    // with `-g` inserted after the verb (`--json` filtered out — the surfaces append their own).
    assert_eq!(
        scope_ways_out("add", &["add", "./zq-nomanifest-src", "--json"], &err),
        vec![
            (
                "INIT_PROJECT_MANIFEST".to_owned(),
                vec!["topos".to_owned(), "init".to_owned()]
            ),
            (
                "RETRY_MACHINE_WIDE".to_owned(),
                vec![
                    "topos".to_owned(),
                    "add".to_owned(),
                    "-g".to_owned(),
                    "./zq-nomanifest-src".to_owned()
                ]
            ),
        ]
    );

    // `-g` is not refused — the machine-wide file is always resolvable, and is BORN by the add.
    let mut added = scoped_path_add(&ctx, &src, true).unwrap();
    assert_eq!(
        added.manifest.as_deref(),
        Some(
            rig.layout()
                .home()
                .join(crate::manifest::MANIFEST_FILE)
                .to_str()
                .unwrap()
        )
    );

    // The row-write BACKSTOP refuses identically for a caller that skipped the scope resolve —
    // the rule lives in one place, not only at the composition root.
    match ops::note_added_path(&ctx, &mut added, &src, false) {
        Err(e) => assert_eq!(e.code(), "NO_MANIFEST"),
        Ok(()) => panic!("the project scope has no file to record into"),
    }
}

#[test]
fn a_reference_verb_with_no_manifest_refuses_while_a_bare_name_falls_through() {
    let (rig, plane, dir, _v) = bare_rig("zq-nomanifest-ref");
    let bare = Scratch::new("zq-nomanifest-ref-cwd");
    let ctx = rig.ctx_at(Some(&bare.0));
    let session_connect = connect(&plane, &dir);

    // A workspace reference is a manifest ROW — with no file to hold it, the add refuses.
    match ops::add_reference(
        &ctx,
        &session_connect,
        None,
        &format!("@{WS_NAME}/{BARE}"),
        false,
        false,
        &Default::default(),
        None,
    ) {
        Err(e) => assert_eq!(e.code(), "NO_MANIFEST"),
        Ok(_) => panic!("no topos.toml covers this folder"),
    }
    // …and with `-g` the same reference applies against the machine-wide file.
    match ops::add_reference(
        &ctx,
        &session_connect,
        None,
        &format!("@{WS_NAME}/{BARE}"),
        true,
        false,
        &Default::default(),
        None,
    ) {
        Ok(ops::AddRefOutcome::Applied { data: d, .. }) => {
            assert_eq!(
                d.reference.as_deref(),
                Some(&format!("{HOST}/{WS_NAME}/{BARE}")[..])
            );
        }
        Ok(ops::AddRefOutcome::Described { .. }) => {
            panic!("a workspace reference never gates")
        }
        Err(e) => panic!("the machine-wide add applies: {}", e.code()),
    }

    // `remove` is the same line from the other side: a ROW-SPELLED token (a reference, or a path)
    // refuses toward the two scopes rather than falling through to a "no such skill".
    for token in [
        format!("@{WS_NAME}/{BARE}"),
        "./zq-nomanifest-ref".to_owned(),
    ] {
        let err = ops::remove_project(
            &ctx,
            &session_connect,
            std::slice::from_ref(&token),
            None,
            true,
            &Default::default(),
        )
        .expect_err("a row spelling with no file to hold it");
        assert_eq!(err.code(), "NO_MANIFEST", "token={token}");
        assert_eq!(
            scope_ways_out("remove", &["remove", &token], &err)[1].1,
            vec![
                "topos".to_owned(),
                "remove".to_owned(),
                "-g".to_owned(),
                token.clone()
            ]
        );
    }
    // A BARE NAME still falls through — the classic ladder owns untracked copies and the built-in.
    assert!(
        ops::remove_project(
            &ctx,
            &session_connect,
            &["zq-plain-name".to_owned()],
            None,
            true,
            &Default::default()
        )
        .unwrap()
        .is_none(),
        "a bare name is not a manifest row spelling"
    );
}

#[test]
fn a_project_add_keeps_its_history_in_the_checkouts_own_store() {
    let rig = Rig::new("zq-custody");
    let proj = project("zq-custody-proj", "");
    let ctx = rig.ctx_at(Some(&proj.0));

    let src = proj.0.join("zq-custody-src");
    skill_source(&src, b"# zq-custody\n");
    let added = scoped_path_add(&ctx, &src, false).unwrap();
    let sid = crate::id::SkillId::parse(added.skill_id.as_deref().unwrap_or_default()).unwrap();

    // Custody follows the SCOPE: the checkout's own store holds the history, the home store none.
    let project_store = crate::sidecar::project_store_layout(&proj.0);
    assert!(
        project_store.skill_dir(&sid).join("lock.json").exists(),
        "the checkout's own store holds the version history"
    );
    assert!(
        !rig.layout().skill_dir(&sid).exists(),
        "the home store holds no entry for a project-scoped adopt"
    );
    assert_eq!(
        std::fs::read_dir(rig.layout().skills_dir())
            .map(|d| d.count())
            .unwrap_or(0),
        0,
        "the home store's skills/ is empty"
    );
    // The row is the committed, travels-with-the-repo spelling, in the folder's own file.
    assert_eq!(added.reference.as_deref(), Some("./zq-custody-src"));
    let text = std::fs::read_to_string(proj.0.join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("\"./zq-custody-src\""), "{text}");
}

#[test]
fn an_out_of_tree_source_records_absolutely_in_the_folders_own_file() {
    let rig = Rig::new("zq-outoftree");
    let proj = project("zq-outoftree-proj", "");
    let ctx = rig.ctx_at(Some(&proj.0));

    // The source sits OUTSIDE the checkout: the reference cannot travel with the repo, so it is
    // spelled absolutely — and still recorded in THIS folder's file. The scope was the person's
    // (they omitted `-g`); it is never rerouted to the machine-wide file.
    let src = rig.work.0.join("zq-outoftree-src");
    skill_source(&src, b"# zq-outoftree\n");
    let added = scoped_path_add(&ctx, &src, false).unwrap();
    assert_eq!(added.reference.as_deref(), src.to_str());
    assert_eq!(
        added.manifest.as_deref(),
        proj.0.join(crate::manifest::MANIFEST_FILE).to_str()
    );
    assert!(
        !rig.layout()
            .home()
            .join(crate::manifest::MANIFEST_FILE)
            .exists(),
        "no machine-wide file was written"
    );
}

#[test]
fn a_dropped_rows_record_is_re_linked_while_a_standing_row_still_refuses() {
    // `remove <path>` edits the file and KEEPS the bytes, so the record outlives the row that
    // asked for it. Re-adding that folder has nothing to refuse — it re-links to the record:
    // the same id, the same lock, no second store dir, nothing minted.
    let rig = Rig::new("zq-relink");
    let proj = project("zq-relink-proj", "");
    let ctx = rig.ctx_at(Some(&proj.0));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let session_connect = connect(&plane, &dir);

    let src = proj.0.join("zq-relink-src");
    skill_source(&src, b"# zq-relink\n");
    let added = scoped_path_add(&ctx, &src, false).unwrap();
    let sid = crate::id::SkillId::parse(added.skill_id.as_deref().unwrap_or_default()).unwrap();
    let store = crate::sidecar::project_store_layout(&proj.0);
    let lock_before = std::fs::read(store.skill_dir(&sid).join("lock.json")).unwrap();

    ops::remove_project(
        &ctx,
        &session_connect,
        &["./zq-relink-src".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap()
    .expect("the path row is a manifest arm");
    assert!(
        store.skill_dir(&sid).join("lock.json").exists(),
        "the record outlives the row"
    );

    let back = scoped_path_add(&ctx, &src, false).unwrap();
    assert_eq!(back.skill_id, added.skill_id, "the same record answers");
    assert_eq!(back.version_id, added.version_id, "at the version it left");
    assert_eq!(back.name, added.name);
    assert_eq!(back.reference.as_deref(), Some("./zq-relink-src"));
    assert!(
        back.note.is_some_and(|n| n.contains("earlier add")),
        "the receipt says the record was re-linked, not freshly adopted"
    );
    assert_eq!(
        std::fs::read(store.skill_dir(&sid).join("lock.json")).unwrap(),
        lock_before,
        "the lock is untouched — no version, no history, no draft snapshot moved"
    );
    assert_eq!(
        std::fs::read_dir(store.skills_dir())
            .map(|d| d.count())
            .unwrap_or(0),
        1,
        "no second record was minted"
    );

    // A folder the file STILL spells is a second adoption of one mutable dir — refused as before.
    assert_eq!(
        scoped_path_add(&ctx, &src, false).unwrap_err().code(),
        "ALREADY_TRACKED"
    );
}

#[test]
fn a_project_remove_of_a_machine_delivered_skill_refuses_toward_g() {
    // (a) The machine-wide FILE spells the row: the near-miss is named, with the `-g` spelling.
    let rig = Rig::new("zq-crossscope");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"latest\"\n"
    ));
    let proj = project("zq-crossscope-proj", "");
    let ctx = rig.ctx_at(Some(&proj.0));
    let session_connect = connect(&plane, &dir);

    let err = ops::remove_project(
        &ctx,
        &session_connect,
        &["deploy".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .expect_err("this folder's file does not carry it");
    let message = crate::render::safe_message(&err);
    assert!(
        message.contains("topos remove -g deploy"),
        "the other scope's row is named as the fix: {message}"
    );
    assert!(
        std::fs::read_to_string(proj.0.join(crate::manifest::MANIFEST_FILE))
            .unwrap()
            .trim()
            .is_empty(),
        "the refusal wrote nothing"
    );

    // (b) The machine-wide file holds only the FEED row (no explicit line spells the bundle):
    // the manifest arm claims nothing, and the CLASSIC removal refuses toward the same `-g`
    // spelling.
    let rig = Rig::new("zq-crossscope-feed");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project("zq-crossscope-feed-proj", "");
    // The feed has actually DELIVERED here (the machine sweep records it) — the delivered set is
    // what makes the name the feed row's claim; a workspace merely publishing a name is not a
    // demand, and would fall through to the classic not-found instead.
    sweep(&rig.ctx_at(Some(&rig.work.0)), &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    let session_connect = connect(&plane, &dir);
    assert!(
        ops::remove_project(
            &ctx,
            &session_connect,
            &["deploy".to_owned()],
            None,
            true,
            &Default::default()
        )
        .unwrap()
        .is_none(),
        "no manifest arm claims a feed-delivered name"
    );
    // The classic path resolves against the workspace's own names, so this fake answers `me`.
    let named = NamedDirectory(dir.clone());
    let named_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(named.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let connectors = ops::RemoveConnectors {
        session: &named_connect,
    };
    let err = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, true)
        .expect_err("what a workspace gives you is not this folder's to delete");
    let message = crate::render::safe_message(&err);
    assert!(
        message.contains("topos remove -g deploy"),
        "the classic path names the machine-wide switch: {message}"
    );
}

// =================================================================================================
// The TARGETED verbs over PROJECT custody: a bundle a project file delivers keeps its engine state
// in the checkout's own store, so every verb that names it by name has to look there — a home-store
// -only resolution answers NO_SUCH_SKILL, or (worse) with a same-named machine copy.
// =================================================================================================

/// `diff` and `log`, run from inside the checkout, read the PROJECT store's copy: the diff shows
/// the edit made to the in-repo placement, and the log walks that store's version history. Both
/// used to resolve the home store alone — a project-delivered bundle was simply not found.
#[test]
fn diff_and_log_resolve_a_project_stores_copy() {
    let rig = Rig::new("proj-targeted");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project_custody("proj-targeted-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    // An edit in the IN-REPO placement — the draft `diff` must show.
    let placed = proj.0.join(".claude/skills/deploy");
    std::fs::write(placed.join("SKILL.md"), b"# deploy\nrun the canary first\n").unwrap();

    let d = ops::diff(
        &ctx,
        "deploy",
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    assert!(
        d.diff.contains("run the canary first"),
        "the project copy's draft is the diff: {}",
        d.diff
    );

    // `log` walks the PROJECT store's git history (the version the sweep applied there).
    let sessions = connect(&plane, &dir);
    let connectors = ops::LogConnectors { session: &sessions };
    let out = ops::log(&ctx, &connectors, "deploy", ops::RowPage::unlimited()).unwrap();
    let versions: Vec<&str> = out
        .events
        .iter()
        .filter(|e| e.get("action").and_then(|x| x.as_str()) == Some("version"))
        .filter_map(|e| e.get("version_id").and_then(|x| x.as_str()))
        .collect();
    assert!(
        versions.contains(&&*topos_core::digest::to_hex(&v.id)),
        "the applied version is in the project store's log: {:?}",
        out.events
    );
}

/// A targeted `update <name>@<version>` goes back inside the PROJECT store: the checkout's copy
/// returns to the older bytes, that store's lock records it, and the machine store — which never
/// held this bundle — stays empty.
#[test]
fn a_targeted_go_back_runs_against_the_project_store() {
    let rig = Rig::new("proj-goback");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let proj = project_custody("proj-goback-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    let placed = proj.0.join(".claude/skills/deploy");
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# deploy v1\n"
    );

    // The team moves to v2 (a real pointer move — the next generation); the project store then
    // holds both versions, which is what a go-back needs.
    let mut moved = catalog_entry("s_deploy", "deploy", &v2);
    moved.generation = 2;
    let dir2 = FakeDirectory::new(vec![moved], Vec::new());
    let out = sweep(&ctx, &plane, &dir2);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# deploy v2\n"
    );

    // Back to v1, by name — resolved in the project store, applied there. The follow seam is the
    // PRODUCTION one (built from the delivery cache the sweep just wrote): it is what makes the
    // re-plan reach for a workspace-scoped placement, so an inert seam would not exercise the
    // path at all.
    let cache_follow = ops::CacheFollow::load(&rig.fs, &rig.layout());
    let ctx = Ctx {
        follow: &cache_follow,
        ..rig.ctx_at(Some(&proj.0))
    };
    let out = ops::pull(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Here,
            name: "deploy".to_owned(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v1.id)),
        },
    )
    .unwrap();
    assert_eq!(out.data.skills.len(), 1);
    assert_eq!(out.data.skills[0].action, PullAction::Held);
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the IN-REPO copy went back"
    );
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    let sp = playout.published(&sid("s_deploy"));
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    assert_eq!(lock.base_commit, topos_core::digest::to_hex(&v1.id));
    // The bytes stayed IN the checkout: the re-plan is the project's, so nothing was aimed at the
    // machine's harness dirs (which the project store's containment rail would refuse outright).
    let map = crate::doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    assert!(
        map.placements
            .iter()
            .all(|p| std::path::Path::new(p).starts_with(&proj.0)),
        "{:?}",
        map.placements
    );
    assert!(
        !rig.layout().skill_dir(&sid("s_deploy")).exists(),
        "the machine store never gained a copy"
    );
}

// =================================================================================================
// The SCOPE FLAG on the targeted verbs: a bare run reads and acts WHERE YOU STAND, `-g` on the
// machine — including for the modes that never touch the reconcile (a go-back, `--reset`).
// =================================================================================================

/// Both scopes holding the SAME bundle: the machine (through the feed) and the checkout (through
/// its own file). Sweeps both, returns the checkout — the two-copy fixture the scope flag is about.
fn both_scopes_hold_deploy(
    tag: &str,
    rig: &Rig,
    plane: &FakePlane,
    dir: &FakeDirectory,
) -> Scratch {
    let proj = project(
        tag,
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let out = sweep_both(&rig.ctx_at(Some(&proj.0)), plane, dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        rig.layout().skill_dir(&sid("s_deploy")).exists()
            && crate::sidecar::existing_project_store(&rig.fs, &proj.0)
                .is_some_and(|l| l.skill_dir(&sid("s_deploy")).exists()),
        "the fixture needs BOTH stores holding the name"
    );
    proj
}

/// With the name in BOTH stores, the reads answer with the copy you are STANDING IN. The machine
/// copy's own draft does not pull them back to the machine: the draft preference exists for
/// publish (ship the edited copy), and letting it steer a read would make `diff` from inside a
/// checkout describe another scope's edit.
#[test]
fn reads_from_inside_a_project_answer_the_project_copy() {
    let rig = Rig::new("scope-reads");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let proj = both_scopes_hold_deploy("scope-reads-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    let machine_copy = rig.skills().join("deploy");
    let project_copy = proj.0.join(".claude/skills/deploy");

    // The team moves to v2 and only the MACHINE converges (`-g`), so the two stores' histories
    // genuinely differ — a log that answered from the wrong store would show a version this
    // checkout has never held.
    let mut moved = delivered("s_deploy", "deploy", &v2);
    moved.generation = 2;
    plane.serves(vec![moved]);
    let mut moved_cat = catalog_entry("s_deploy", "deploy", &v2);
    moved_cat.generation = 2;
    let dir2 = FakeDirectory::new(vec![moved_cat], Vec::new());
    let out = sweep_scoped(&ctx, &plane, &dir2, ops::UpdateScope::Machine);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read(machine_copy.join("SKILL.md")).unwrap(),
        b"# deploy v2\n"
    );
    assert_eq!(
        std::fs::read(project_copy.join("SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the checkout stayed where its own file put it"
    );

    // Each copy carries a DIFFERENT edit; the machine's is the one the old home-first rule would
    // have shown (a drafted home copy outranked everything).
    std::fs::write(machine_copy.join("SKILL.md"), b"# machine edit\n").unwrap();
    std::fs::write(project_copy.join("SKILL.md"), b"# project edit\n").unwrap();

    let d = ops::diff(
        &ctx,
        "deploy",
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    assert!(
        d.diff.contains("project edit") && !d.diff.contains("machine edit"),
        "the diff is the copy you stand in: {}",
        d.diff
    );

    // `-g` is the other half of the same line, and `diff` carries it like every other
    // scope-taking verb: the machine copy's edit, read without leaving the checkout. Before it
    // existed there was NO way to read the machine twin from in here — the folder you stand in
    // answered, and the flag every sibling verb takes was simply rejected.
    let g = ops::diff(
        &ctx,
        "deploy",
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Machine,
    )
    .unwrap();
    assert!(
        g.diff.contains("machine edit") && !g.diff.contains("project edit"),
        "`-g` reads the machine copy from inside the project: {}",
        g.diff
    );

    let sessions = connect(&plane, &dir2);
    let connectors = ops::LogConnectors { session: &sessions };
    let out = ops::log(&ctx, &connectors, "deploy", ops::RowPage::unlimited()).unwrap();
    let versions: Vec<String> = out
        .events
        .iter()
        .filter(|e| e.get("action").and_then(|x| x.as_str()) == Some("version"))
        .filter_map(|e| e.get("version_id").and_then(|x| x.as_str()))
        .map(str::to_owned)
        .collect();
    assert!(
        versions.contains(&topos_core::digest::to_hex(&v1.id)),
        "the checkout's own version is in its log: {versions:?}"
    );
    assert!(
        !versions.contains(&topos_core::digest::to_hex(&v2.id)),
        "the machine's newer version is NOT this copy's history: {versions:?}"
    );
}

/// A history read must not present itself as fresh when the workspace behind it did not answer. The
/// sweep's own warning is transient (and silent inside the staleness window), so `log` reads the
/// RECORDED fault — machine-scoped, like the device id, even for a copy the checkout's own file
/// delivers — and says it once, naming the workspace the way a person addresses it and ending in
/// the same clause the sweep would have used.
#[test]
fn a_project_copys_log_names_the_workspace_whose_last_exchange_failed() {
    let rig = Rig::new("logfault");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    // Delivered by the CHECKOUT's own file: the custody (and the action log) live in the project
    // store, while the freshness cache the fault rides is the machine's.
    let proj = project(
        "logfault-repo",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let out = sweep(&rig.ctx_at(Some(&proj.0)), &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj.0)
            .is_some_and(|l| l.skill_dir(&sid("s_deploy")).exists()),
        "the fixture needs the CHECKOUT holding the copy"
    );

    // The server answers with a failure. Nothing about the copy changes — only the record of the
    // exchange does.
    plane.serve_unavailable();
    sweep(&rig.ctx_at(Some(&proj.0)), &plane, &dir);
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Unavailable));

    // The production follow seam (built from the delivery cache the sweep wrote) is what maps this
    // copy back to its workspace.
    let cache_follow = ops::CacheFollow::load(&rig.fs, &rig.layout());
    let ctx = Ctx {
        follow: &cache_follow,
        ..rig.ctx_at(Some(&proj.0))
    };
    let sessions = connect(&plane, &dir);
    let connectors = ops::LogConnectors { session: &sessions };
    let data = ops::log(&ctx, &connectors, "deploy", ops::RowPage::unlimited()).unwrap();
    let fault = data.sync_fault.clone().expect("the fault reaches `log`");
    assert_eq!(
        fault.workspace, WS_NAME,
        "named the way a person addresses it"
    );
    assert_eq!(fault.kind, ExchangeFault::Unavailable);

    // ONE line, the cause named exactly as the sweep would have named it — and never the id.
    let rendered = crate::render::log_tty(&data);
    assert!(
        rendered.contains(&format!(
            "note: {WS_NAME}'s last exchange with this machine did not succeed — the server did \
             not answer successfully; retry with `topos update`"
        )),
        "{rendered}"
    );
    assert!(
        !rendered.contains(WS),
        "the cache's key is never what a person is shown: {rendered}"
    );

    // The server comes back: the note goes with the fault.
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    sweep(&rig.ctx_at(Some(&proj.0)), &plane, &dir);
    let data = ops::log(&ctx, &connectors, "deploy", ops::RowPage::unlimited()).unwrap();
    assert!(data.sync_fault.is_none());
    assert!(
        !crate::render::log_tty(&data).contains("did not succeed"),
        "a landed exchange prints no such line"
    );
}

/// `-g` pins the MACHINE store for the targeted modes too, INCLUDING when only the checkout's copy
/// carries a draft — the case the home-first-unless-drafted resolution got backwards, so a `-g
/// --reset` described and then discarded the project copy's edit. Its describe also re-spells the
/// flag in the apply command: a `--yes` without `-g` would act on the other copy.
#[test]
fn a_g_reset_inside_a_project_never_reaches_the_checkouts_copy() {
    let rig = Rig::new("scope-greset");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = both_scopes_hold_deploy("scope-greset-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    let machine_copy = rig.skills().join("deploy");
    let project_copy = proj.0.join(".claude/skills/deploy");
    // ONLY the checkout's copy is edited. The machine's is clean — so `-g` has nothing to discard,
    // and anything it DOES discard came from the scope the flag excluded.
    std::fs::write(project_copy.join("SKILL.md"), b"# project edit\n").unwrap();

    let described = ops::reset(
        &ctx,
        &["deploy".to_owned()],
        false,
        ops::StoreScope::Machine,
        &ops::Selection::default(),
    )
    .expect("the machine store holds it");
    let (items, yes_argv) = match described {
        ops::ResetOutcome::Described { items, yes_argv } => (items, yes_argv),
        other => panic!("a bare reset describes: {other:?}"),
    };
    assert!(
        !items[0].drop_diff.contains("project edit"),
        "the machine copy is clean — nothing of the checkout's is disclosed as lost: {}",
        items[0].drop_diff
    );
    assert_eq!(
        yes_argv,
        vec!["topos", "update", "-g", "deploy", "--reset", "--yes"],
        "the apply command re-spells the scope flag"
    );

    ops::reset(
        &ctx,
        &["deploy".to_owned()],
        true,
        ops::StoreScope::Machine,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(project_copy.join("SKILL.md")).unwrap(),
        b"# project edit\n",
        "`-g` never reaches into the checkout"
    );
    assert_eq!(
        std::fs::read(machine_copy.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the machine copy is where the reset ran — at the team's bytes, as it already was"
    );

    // The BARE run is the other half of the line: it acts where you stand, so it is the
    // checkout's edit that is disclosed and discarded.
    let bare = ops::reset(
        &ctx,
        &["deploy".to_owned()],
        false,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    match &bare {
        ops::ResetOutcome::Described { items, yes_argv } => {
            assert!(
                items[0].drop_diff.contains("project edit"),
                "{}",
                items[0].drop_diff
            );
            assert!(!yes_argv.contains(&"-g".to_owned()), "{yes_argv:?}");
        }
        other => panic!("a bare reset describes: {other:?}"),
    }
    ops::reset(
        &ctx,
        &["deploy".to_owned()],
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(project_copy.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the bare run discarded the copy you stand in"
    );
}

/// A name only a PROJECT store holds is an honest miss under `-g` — never quietly answered by (or
/// applied to) the copy the flag excluded. The same name resolves fine without the flag.
#[test]
fn a_g_targeted_run_misses_a_project_only_name() {
    let rig = Rig::new("scope-gmiss");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project_custody("scope-gmiss-repo", &rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    for (scope, want) in [
        (ops::StoreScope::Machine, false),
        (ops::StoreScope::Here, true),
    ] {
        let r = ops::reset(
            &ctx,
            &["deploy".to_owned()],
            false,
            scope,
            &ops::Selection::default(),
        );
        assert_eq!(
            r.is_ok(),
            want,
            "scope {scope:?} resolved {:?}",
            r.as_ref().err()
        );
        if !want {
            let err = r.unwrap_err();
            assert!(
                matches!(&err, ClientError::NoSuchSkill { name } if name == "deploy"),
                "got {err:?}"
            );
        }
    }
    // The go-back obeys the same line — `-g` looks only where the flag says.
    let err = ops::pull(
        &ctx,
        ops::PullScope::One {
            store: ops::StoreScope::Machine,
            name: "deploy".to_owned(),
            workspace: None,
            mode: ops::TargetMode::GoBack(ops::VersionRef::Full(v.id)),
        },
    );
    let err = match err {
        Ok(_) => panic!("`-g` must not reach the checkout's copy"),
        Err(e) => e,
    };
    assert!(
        matches!(&err, ClientError::NoSuchSkill { name } if name == "deploy"),
        "got {err:?}"
    );
}

/// A path adopted IN PLACE under a project file keeps its recorded placement through a reset: the
/// planner's project dispatch is for bundles whose dirs the ENGINE owns, and this dir is the
/// person's own. Re-planning it into `.claude/skills/<name>` would materialize a second copy and
/// leave the edited source exactly as it was — while the receipt claimed the edits were discarded.
#[test]
fn a_reset_of_an_adopted_path_restores_the_source_dir() {
    let rig = Rig::new("zq-adopt-reset");
    let proj = project("zq-adopt-reset-proj", "");
    let ctx = rig.ctx_at(Some(&proj.0));

    let src = proj.0.join("tools/zq-adopted");
    skill_source(&src, b"# adopted\n");
    let added = scoped_path_add(&ctx, &src, false).unwrap();
    assert_eq!(added.name, "zq-adopted");

    // The edit lands where the person works — the adopted source itself.
    std::fs::write(src.join("SKILL.md"), b"# adopted\nlocal edit\n").unwrap();
    let described = ops::reset(
        &ctx,
        &["zq-adopted".to_owned()],
        false,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    match &described {
        ops::ResetOutcome::Described { items, .. } => assert!(
            items[0].drop_diff.contains("local edit"),
            "the loss is the source dir's edit: {}",
            items[0].drop_diff
        ),
        other => panic!("a bare reset describes: {other:?}"),
    }

    ops::reset(
        &ctx,
        &["zq-adopted".to_owned()],
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(src.join("SKILL.md")).unwrap(),
        b"# adopted\n",
        "the SOURCE dir is what the reset restored"
    );
    assert!(
        !proj.0.join(".claude/skills/zq-adopted").exists(),
        "no second copy was planted in the checkout's harness dirs"
    );
}

// =================================================================================================
// Adopted-in-place custody is NEVER the sweep's to destroy: the user's source dir survives every
// clean path byte-identical, in both scopes — and a retained ghost's `remove` speaks honestly.
// =================================================================================================

/// Every file under `dir` with its exact bytes — the byte-identical assertion's witness.
fn dir_bytes(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push((
                    p.strip_prefix(dir).unwrap().to_string_lossy().into_owned(),
                    std::fs::read(&p).unwrap(),
                ));
            }
        }
    }
    out.sort();
    out
}

/// The PROJECT-scope clean paths: a plain update leaves the adopted source alone; a hand-edited
/// manifest that orphans the record (the exact file state a `remove` row-drop leaves too) retires
/// NOTHING of the source dir — it survives byte-identical, record retained, idempotent across
/// sweeps. This was live data loss: the pre-fix cleaner marked every under-project placement of an
/// undemanded record stale, the adopted source included.
#[test]
fn an_adopted_in_place_source_dir_survives_the_project_retire() {
    let rig = Rig::new("zq-adoptkeep");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let proj = project("zq-adoptkeep-proj", "");
    let src = proj.0.join("skills/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    std::fs::write(src.join("notes.txt"), b"the user's own extra file\n").unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    scoped_path_add(&ctx, &src, false).unwrap();
    let baseline = dir_bytes(&src);

    // A plain update (the row present) writes nothing into the source dir.
    sweep(&ctx, &plane, &dir);
    assert_eq!(
        dir_bytes(&src),
        baseline,
        "a plain update left the source alone"
    );

    // The row is orphaned by hand (the same file state a row-drop `remove` leaves): the sweep
    // retires nothing of the adopted source.
    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "").unwrap();
    sweep(&ctx, &plane, &dir);
    assert!(src.is_dir(), "the adopted source dir survives the retire");
    assert_eq!(dir_bytes(&src), baseline, "…byte-identical");
    // Idempotent: the sweep after changes nothing either.
    sweep(&ctx, &plane, &dir);
    assert_eq!(dir_bytes(&src), baseline);
    // The record is retained (bytes-stay honesty; the ghost row explains itself elsewhere).
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(
        std::fs::read_dir(playout.skills_dir()).unwrap().count() > 0,
        "the record is retained"
    );
}

/// The same retire driven through the REAL row-drop (`remove` → the manifest arm), project scope:
/// the row leaves, the sweep retires nothing of the source dir.
#[test]
fn remove_then_update_never_touches_an_adopted_source_dir() {
    let rig = Rig::new("zq-adoptrm");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let proj = project("zq-adoptrm-proj", "");
    let src = proj.0.join("skills/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    scoped_path_add(&ctx, &src, false).unwrap();
    let baseline = dir_bytes(&src);

    let session_connect = connect(&plane, &dir);
    let outcome = ops::remove_project(
        &ctx,
        &session_connect,
        &["quaggamap".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap()
    .expect("the row-drop arm claims the adopted name");
    match outcome {
        ops::RemoveOutcome::Applied(_) => {}
        other => panic!("a row drop applies immediately: {other:?}"),
    }
    assert_eq!(dir_bytes(&src), baseline, "the remove itself moved no byte");

    sweep(&ctx, &plane, &dir);
    assert!(src.is_dir(), "the retire sweep spared the adopted source");
    assert_eq!(dir_bytes(&src), baseline, "…byte-identical");
}

/// The MACHINE scope holds the same promise: a `-g` adopted source survives the row-drop sweep AND
/// the `--force` repair (which parks + re-projects every placement topos wrote — the user's own
/// dir is not one of them).
#[test]
fn an_adopted_source_dir_survives_the_machine_sweeps_and_rebuild() {
    let rig = Rig::new("zq-adoptg");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global("");
    let src = rig.home.0.join("tools/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    scoped_path_add(&ctx, &src, true).unwrap();
    let baseline = dir_bytes(&src);

    // `update --force` with the row present: every topos-written placement re-projects; the
    // adopted source is left exactly as it stands.
    ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            rebuild: true,
            scope: ops::UpdateScope::Machine,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        dir_bytes(&src),
        baseline,
        "a rebuild never touches the source"
    );

    // The row dropped, then the machine sweep: the source survives byte-identical.
    rig.write_global("");
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(src.is_dir(), "the machine retire spared the adopted source");
    assert_eq!(dir_bytes(&src), baseline, "…byte-identical");
}

/// Item: the demand-guard keys on a ROW claiming the name, not on store/cache provenance. With the
/// row present the refusal stands VERBATIM; with the row gone the same token is a GHOST and falls
/// through to the describe-first permanent delete, whose describe says the sweep would retire it
/// anyway — and whose receipt speaks in the applied tense.
#[test]
fn a_ghost_remove_falls_through_and_a_still_claimed_name_keeps_the_refusal() {
    let rig = Rig::new("zq-ghost");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "delivered + recorded"
    );
    // The app wires the CACHE-BACKED follow seam (the workspace provenance the guard reads);
    // the rig's default is inert, which would hide the ghost's provenance entirely.
    let cf = ops::CacheFollow::load(&rig.fs, &rig.layout());
    let mut ctx = rig.ctx_at(Some(&rig.work.0));
    ctx.follow = &cf;

    let named = NamedDirectory(dir.clone());
    let named_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(named.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let connectors = ops::RemoveConnectors {
        session: &named_connect,
    };

    // (a) STILL CLAIMED (the row is in the machine file): today's refusal, verbatim.
    let err = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, false)
        .expect_err("a claimed name refuses toward the demand");
    assert_eq!(
        crate::render::safe_message(&err),
        "'deploy' is delivered from a workspace — remove the DEMAND, not the copy: `topos \
         remove deploy` drops this folder's line for it; `topos remove -g deploy` edits your \
         machine-wide file (switching it off here). What the workspace assigns you is managed \
         on the web."
    );

    // (b) The row leaves (the ghost window: record + cache provenance remain, demand gone): the
    // false refusal is GONE — the bare run DESCRIBES the permanent delete, note honest.
    rig.write_global("");
    let outcome = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, false)
        .expect("the ghost falls through to the classic ladder");
    let items = match outcome {
        ops::RemoveOutcome::Described { data, yes_argv } => {
            assert!(yes_argv.contains(&"--yes".to_owned()));
            assert!(!data.applied);
            data.items
        }
        other => panic!("a permanent delete describes first: {other:?}"),
    };
    assert_eq!(items.len(), 1);
    assert!(matches!(
        items[0].kind,
        topos_types::results::RemoveKind::TrackedLocalPermanent
    ));
    let note = items[0].note.as_deref().expect("the ghost explains itself");
    assert!(
        note.contains("`topos update` retires it anyway"),
        "the describe says doing nothing also resolves it: {note}"
    );

    // (c) `--yes` applies: dirs + record go, and the receipt's note speaks in the applied tense.
    let outcome = ops::remove(&ctx, &connectors, &["deploy".to_owned()], &[], None, true)
        .expect("the consented apply");
    let data = match outcome {
        ops::RemoveOutcome::Applied(d) => d,
        other => panic!("--yes applies: {other:?}"),
    };
    assert!(data.applied);
    let note = data.items[0]
        .note
        .as_deref()
        .expect("the receipt discloses");
    assert!(
        !note.contains("doing nothing"),
        "an applied receipt never claims a choice that is already spent: {note}"
    );
    assert!(
        !rig.layout().skill_dir(&sid).exists(),
        "the record is gone with the copy"
    );
}

/// **The classic delete takes an MCP bundle's config ENTRIES with it.** This arm deletes recorded
/// dirs and the store record — and a config-placed bundle has neither of those where its reach
/// lives: its reach is the `topos-…` entries it wrote into agents' MCP config files. Deleting the
/// record that names them without retiring them first strands them in those files forever, with
/// nothing left on the machine that could ever prove whose they were. So the same convergence the
/// manifest arm runs on a dropped row runs here, and the receipt says which files it touched.
#[test]
fn a_classic_delete_of_an_mcp_record_takes_its_config_entries_with_it() {
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let rig = Rig::new("mcp-classic-rm");
    rig.seed_session();
    // cursor + openclaw both detect off the fake home, so the reach is hermetic.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".openclaw")).unwrap();

    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    // A CONNECTED SERVER is the catalog's second list — the document inline, no bytes to fetch.
    let dir = FakeDirectory::new(Vec::new(), Vec::new()).with_server(catalog_server(
        "s_linear",
        "linear",
        "https://mcp.example/linear",
    ));
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"~/.cursor/mcp.json\", \"~/.openclaw/openclaw.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    let claw = rig.home.0.join(".openclaw/openclaw.json");
    for f in [&cursor, &claw] {
        assert!(
            std::fs::read_to_string(f)
                .unwrap()
                .contains("topos-eng-linear"),
            "the entry landed in {f:?}"
        );
    }

    // The row leaves by hand: the record and its config entries are now nobody's demand, so the
    // name falls through to the classic ladder's permanent delete.
    rig.write_global("");
    let named = NamedDirectory(dir.clone());
    let named_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(named.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let connectors = ops::RemoveConnectors {
        session: &named_connect,
    };
    // The DESCRIBE names the whole blast radius BEFORE consent: the config files the apply will
    // edit are knowable from the ledger right now, and a `--yes` gate that mentions them only on
    // the receipt afterwards is asking for a decision it did not state.
    let described = ops::remove(&ctx, &connectors, &["linear".to_owned()], &[], None, false)
        .expect("the permanent delete describes first");
    let ops::RemoveOutcome::Described { data, .. } = described else {
        panic!("a permanent delete describes first");
    };
    let note = data.items[0]
        .note
        .clone()
        .expect("the describe states what goes");
    assert!(
        note.contains("also removes its MCP server entries from")
            && note.contains(".cursor/mcp.json")
            && note.contains(".openclaw/openclaw.json"),
        "the describe names every config file the apply will edit: {note}"
    );
    // The whole blast radius on ONE rendered line. A config-placed bundle is store-only — it has
    // no placement folders at all — so the line names the config entries and invents no folder:
    // the `from …` clause appears only where there really are dirs to empty.
    assert!(
        data.items[0].dest_dirs.is_empty(),
        "an mcp record places no skill folders: {:?}",
        data.items[0].dest_dirs
    );
    let described_tty = crate::render::remove_describe_tty(&data, &["topos".to_owned()]);
    assert!(
        described_tty.contains("Would delete 'linear' PERMANENTLY")
            && described_tty.contains(".cursor/mcp.json")
            && described_tty.contains(".openclaw/openclaw.json"),
        "the gate names the config entries, with no invented folder: {described_tty}"
    );
    // AND IT STILL SAYS THE WORD. A config-placed bundle always carries per-file note clauses, and
    // a note used to REPLACE the kind's sentence — so this gate, the one asking consent for an
    // irreversible delete, was the one gate that never said `PERMANENTLY`. The permanence leads
    // now and the per-file clauses indent under it.
    assert!(
        described_tty
            .lines()
            .next()
            .is_some_and(|l| l.contains("PERMANENTLY")),
        "the permanence is the headline, not a clause the note can swallow: {described_tty}"
    );

    let outcome = ops::remove(&ctx, &connectors, &["linear".to_owned()], &[], None, true)
        .expect("the consented apply");
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("--yes applies");
    };

    for f in [&cursor, &claw] {
        let text = std::fs::read_to_string(f).unwrap_or_default();
        assert!(
            !text.contains("topos-eng-linear"),
            "the entry outlived the record it belonged to, in {f:?}: {text}"
        );
    }
    // The scope gives the key up and RESERVES the name: both entries left, but a key names an
    // OAuth trust surface and no config file can be read to rule a filed sign-in out. The name
    // goes back only to a later mint for the same server.
    let custody = crate::config_custody::read(&rig.fs, &rig.layout()).unwrap();
    assert!(
        crate::config_custody::entries_of(&rig.fs, &rig.layout(), "s_linear").is_empty(),
        "the record's config entries left with the row"
    );
    assert!(!custody.keys.contains_key("s_linear"), "{custody:?}");
    assert!(
        custody.retired.values().any(|b| b == "s_linear"),
        "the name it minted stays reserved: {custody:?}"
    );
    // And the receipt names the files it touched — a removal that edited somebody's agent config
    // says so.
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("the server's entry was removed.") && note.contains("cursor"),
        "{note}"
    );
}

/// TWO bundles of the SAME NAME in one scope — the workspace `linear` a feed delivers and a local
/// `linear` folder a row adopts — each keep their OWN per-agent config states on the receipt. The
/// converge's outcomes join to a row by the bundle's identity: matching on the display name handed
/// whichever row came first both bundles' outcomes and left the other row silent about entries
/// that really landed.
#[test]
fn two_same_named_mcp_bundles_in_one_scope_each_keep_their_own_states() {
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let rig = Rig::new("mcp-same-name");
    rig.seed_session();
    // cursor + openclaw both detect off the fake home and hang their surfaces there, so this stays
    // hermetic whatever the dev machine has installed.
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".openclaw")).unwrap();

    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new()).with_server(catalog_server(
        "s_linear",
        "linear",
        "https://mcp.example/workspace",
    ));

    // The LOCAL bundle of the same name: its own folder, its own address.
    let local = rig.work.0.join("linear");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(
        local.join("server.json"),
        mcp_server_json("https://mcp.example/local"),
    )
    .unwrap();
    rig.write_global(&format!(
        "[mcp]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"~/.cursor/mcp.json\", \"~/.openclaw/openclaw.json\"] }}\n\
         linear = {{ path = \"{}\", kind = \"mcp\", dest = [\"~/.cursor/mcp.json\", \"~/.openclaw/openclaw.json\"] }}\n",
        local.display()
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    let rows: Vec<&topos_types::results::PullSkill> = out
        .data
        .skills
        .iter()
        .filter(|s| s.skill == "linear")
        .collect();
    assert_eq!(rows.len(), 2, "one row each: {:?}", out.data.skills);
    for row in rows {
        let mut agents: Vec<&str> = row.harnesses.iter().map(|h| h.agent.as_str()).collect();
        agents.sort_unstable();
        assert_eq!(agents, ["cursor", "openclaw"], "{row:?}");
    }
    // Both entries really landed, each under its own minted key.
    let text = std::fs::read_to_string(rig.home.0.join(".cursor/mcp.json")).unwrap();
    assert!(
        text.contains("topos-eng-linear")
            && text.contains("topos-local-linear")
            && text.contains("https://mcp.example/workspace")
            && text.contains("https://mcp.example/local"),
        "{text}"
    );
}
