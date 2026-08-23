//! The forge IMPORT path and what it records: the staged self-ignore sentinel (and the disclosure
//! a shipped ignore earns), the `-s`/`-a` selector import, the ONE split that removes several
//! members of a set, and the project containment rail. Plus the two reads that ride the same
//! stores: the applied report's deterministic cross-store pick, and a machine-local registry
//! failing CLOSED on a document this build cannot decipher.

use std::sync::{Arc, Mutex};

use topos_core::digest::FileMode;
use topos_types::requests::{WireChannelEntry, WireChannelSkill};
use topos_types::results::PullAction;

use crate::ctx::Ctx;
use crate::ops;
use crate::sidecar::Layout;

use super::rig::*;

/// A hand-run `topos update` from inside the checkout: the person-typed posture, which re-resolves
/// every follow row and rewrites the project's lock (`LockMode::Update`) instead of installing
/// exactly what the lock already records.
fn update_hand_run(ctx: &Ctx<'_>, plane: &FakePlane, dir: &FakeDirectory) -> ops::PullOutcome {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        None,
        &ops::ManifestUpdateOpts {
            lock: ops::LockMode::Update,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap()
}

// =================================================================================================
// Self-ignore breadth: the forge IMPORT path stages the sentinel; a shipped ignore discloses.
// =================================================================================================

/// `git init` + `git status --porcelain` — the real visibility witness (`None` = no git binary;
/// the caller skips that half).
fn git_status(repo: &std::path::Path) -> Option<String> {
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(repo)
        .output()
        .ok()?;
    if !init.status.success() {
        return None;
    }
    let out = std::process::Command::new("git")
        .args(["status", "--porcelain", "-uall"])
        .current_dir(repo)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

#[test]
fn a_project_import_stages_the_sentinel_and_a_shipped_ignore_discloses() {
    let rig = Rig::new("import-sentinel");
    let proj = project("proj-sentinel", "[skills]\n");
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha\n")],
    ));
    let ctx = rig.ctx_at(Some(&proj.0));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(proj.0.clone()),
    };
    let spec = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: "o".into(),
        repo: "r".into(),
        git_ref: None,
        subdir: None,
    };
    let data = ops::add_remote(
        &ctx,
        &git,
        &spec,
        &roots,
        &ops::AddRemoteOpts {
            skill: Some("alpha".into()),
            harness: None,
            dest_root: None,
            global: false,
        },
    )
    .unwrap();
    let alpha = proj.0.join(".claude/skills/alpha");
    assert_eq!(
        std::fs::read(alpha.join(crate::scan::IGNORE_FILE)).unwrap(),
        crate::scan::IGNORE_SENTINEL,
        "the import path stages the sentinel exactly like the materializer"
    );
    assert!(
        data.note.is_none(),
        "a sentinel placement needs no disclosure: {:?}",
        data.note
    );

    // A repo shipping its OWN root ignore that does NOT self-ignore: placed verbatim, disclosed.
    git.serve(build_repo_targz(
        "o-r2-cccccccccccc3",
        &[
            ("skills/beta/SKILL.md", b"# beta\n"),
            ("skills/beta/.gitignore", b"*.log\n"),
        ],
    ));
    let spec2 = crate::source::RemoteSpec {
        host: crate::source::GitHost::GitHub,
        owner: "o".into(),
        repo: "r2".into(),
        git_ref: None,
        subdir: None,
    };
    let data2 = ops::add_remote(
        &ctx,
        &git,
        &spec2,
        &roots,
        &ops::AddRemoteOpts {
            skill: Some("beta".into()),
            harness: None,
            dest_root: None,
            global: false,
        },
    )
    .unwrap();
    let beta = proj.0.join(".claude/skills/beta");
    assert_eq!(
        std::fs::read(beta.join(".gitignore")).unwrap(),
        b"*.log\n",
        "a shipped root ignore is content, never overlaid"
    );
    let note = data2.note.clone().unwrap_or_default();
    assert!(note.contains("visible to git"), "{note}");

    // The REAL git witness: the sentinel placement is invisible; the shipped-ignore one shows.
    match git_status(&proj.0) {
        Some(status) => {
            assert!(!status.contains("alpha"), "{status}");
            assert!(status.contains("beta"), "{status}");
        }
        None => eprintln!("skipping git-visibility half: no usable git binary"),
    }
}

#[test]
fn a_delivered_bundle_shipping_a_non_self_ignoring_gitignore_warns_on_the_sweep() {
    let rig = Rig::new("sweep-gitvisible");
    rig.seed_session();
    let proj = project(
        "proj-gitvisible",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = mk_version(&[
        ("SKILL.md", FileMode::Regular, b"# deploy\n"),
        (".gitignore", FileMode::Regular, b"*.log\n"),
    ]);
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    let placed = proj.0.join(".claude/skills/deploy");
    assert_eq!(
        std::fs::read(placed.join(".gitignore")).unwrap(),
        b"*.log\n",
        "bundle content is never edited: {:?}",
        out.warnings
    );
    let w = crate::message::legacy_lines(&out.advisories)
        .into_iter()
        .find(|w| w.starts_with("GIT_VISIBLE"))
        .unwrap_or_else(|| panic!("the visibility disclosure: {:?}", out.advisories));
    assert!(w.contains("deploy"), "{w}");
}

#[test]
fn an_interactive_add_of_a_git_source_always_describes_first() {
    // The describe is a property of the VERB, not of the origin. `add` is where a person is
    // present to read what a repo holds and where it would land, so every interactive add says it
    // — including a re-add of a source already tracked here, and including one whose rows are
    // partly standing in the file. A whole-repo add expands to one row per discovered skill (a
    // repo set has no v2 row), and the standing-row note names exactly which of them stand.
    let rig = Rig::new("row-no-trust");
    let proj = project("proj-rowtrust", "[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha\n"),
            ("skills/beta/SKILL.md", b"# beta\n"),
        ],
    ));

    // The bare add DESCRIBES — the row's existence changes the wording, never the gate.
    let outcome = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        false,
        &Default::default(),
        None,
    )
    .unwrap();
    match outcome {
        ops::AddRefOutcome::Described { data, yes_argv } => {
            assert_eq!(data.members, vec!["alpha".to_owned(), "beta".to_owned()]);
            let note = data.note.expect("the describe names the standing row");
            assert!(note.contains("already records alpha"), "{note}");
            assert!(note.contains("records the rest"), "{note}");
            assert!(note.contains("demand, not consent"), "{note}");
            assert!(yes_argv.contains(&"--yes".to_owned()));
        }
        ops::AddRefOutcome::Applied { .. } => {
            panic!("an interactive add of a git source always describes first")
        }
    }
    assert!(
        !proj.0.join(".claude/skills/alpha").exists(),
        "the describe installs nothing"
    );

    // `--yes` applies.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r",
        false,
        true,
        &Default::default(),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied { .. } => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(proj.0.join(".claude/skills/alpha/SKILL.md").exists());

    // Tracked now — and a further BARE add of the SAME origin still describes. That is the
    // deliberate cost of making the shape a property of the verb: an answer a person asked for is
    // never skipped because the machine happens to have seen the source before.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r/beta",
        false,
        false,
        &Default::default(),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Described { .. } => {}
        ops::AddRefOutcome::Applied { .. } => {
            panic!("a repeat add describes too — the shape belongs to the verb")
        }
    }
}

// =================================================================================================
// The SELECTOR import (`-s`/`-a`) — the same describe-first shape, the same per-scope store.
// =================================================================================================

#[test]
fn a_selector_import_describes_first_then_installs() {
    // A selector narrows WHICH members land and WHERE; it is not a way around reading what a
    // source holds. So `add owner/repo -s alpha` DESCRIBES, puts nothing in place, and applies
    // only under `--yes` — exactly like the bare reference arm.
    let rig = Rig::new("sel-gate");
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
    let skills = vec!["alpha".to_owned()];

    let described = ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &skills,
        &[],
        &[],
        true,
        false,
    )
    .unwrap();
    match described {
        ops::AddManyOutcome::Described { data, yes_argv } => {
            assert_eq!(data.source, "github.com/o/r");
            assert_eq!(data.members, vec!["alpha".to_owned()]);
            assert!(yes_argv.contains(&"--yes".to_owned()), "{yes_argv:?}");
            assert!(yes_argv.contains(&"-s".to_owned()), "{yes_argv:?}");
        }
        ops::AddManyOutcome::Applied(_) => panic!("an untracked origin describes first"),
    }
    assert!(
        !rig.home.0.join(".claude/skills/alpha").exists(),
        "a describe installs nothing"
    );

    // `--yes` GRANTS the origin, then installs.
    let applied = ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &skills,
        &[],
        &[],
        true,
        true,
    )
    .unwrap();
    match applied {
        ops::AddManyOutcome::Applied(items) => assert_eq!(items.len(), 1),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());
    assert!(
        !rig.home.0.join(".claude/skills/beta").exists(),
        "the selector narrowed the landing"
    );
    // A later bare reference add of the same origin describes too — the two-phase shape belongs
    // to the verb, so it never depends on what the machine happens to have seen before.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "github.com/o/r/beta",
        true,
        false,
        &Default::default(),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Described { .. } => {}
        ops::AddRefOutcome::Applied { .. } => panic!("an interactive add describes first"),
    }
}

#[test]
fn a_project_scope_selector_import_converges_on_a_later_update() {
    // The bug this pins: a selector import that wrote its engine state into the HOME store while
    // the row lived in a PROJECT manifest — the project reconcile reads the checkout's own store,
    // so it could never see the import and would re-install it forever.
    let rig = Rig::new("sel-project");
    let proj = project("sel-proj", "[skills]\nalpha = \"github:o/r\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha\n")],
    ));
    match ops::add_forge_selected(
        &ctx,
        &connect(&plane, &dir),
        &git,
        "o/r",
        &["alpha".to_owned()],
        &[],
        &[],
        false,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(items) => assert_eq!(items.len(), 1),
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    // The import lives in the CHECKOUT's own store — the one the project reconcile reads.
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0)
        .expect("the selector import minted the project store");
    let pctx = ops_ctx_with_layout(&ctx, &playout);
    assert_eq!(
        crate::ops::forge_imports(&pctx).len(),
        1,
        "the project store tracks the import"
    );
    let fetches_before = git.fetches();
    let probes_before = git.probes();

    // The update therefore CONVERGES it — and the comparison is the cheap probe, so no archive
    // moves at all.
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?} / {:?}",
        out.data.skills,
        out.warnings
    );
    assert_eq!(
        git.fetches(),
        fetches_before,
        "nothing re-installed, and nothing downloaded to find that out"
    );
    assert_eq!(git.probes(), probes_before + 1, "one probe to compare");
}

/// The store-routing helper the reconcile uses, reachable from the suite.
fn ops_ctx_with_layout<'a>(ctx: &'a Ctx<'a>, layout: &Layout) -> Ctx<'a> {
    Ctx {
        progress: crate::progress::silent(),
        layout: layout.clone(),
        fs: ctx.fs,
        ids: ctx.ids,
        clock: ctx.clock,
        device_id: ctx.device_id.clone(),
        harness: ctx.harness,
        triggers: ctx.triggers.clone(),
        plane: ctx.plane,
        follow: ctx.follow,
        roots: ctx.roots.clone(),
    }
}

// =================================================================================================
// Removing SEVERAL members of one set is ONE split; a bare name that names two rows refuses.
// =================================================================================================

#[test]
fn removing_two_members_of_one_set_leaves_neither() {
    // The bug this pins: two SetSplit arms applied in sequence each rebuilt the set line from its
    // FULL member list, so the second one wrote the first one's removal straight back.
    let rig = Rig::new("split-multi");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let a = one_file(b"# alpha\n");
    let b = one_file(b"# beta\n");
    let c = one_file(b"# gamma\n");
    let plane = FakePlane::new(log)
        .with_version("s_a", &a)
        .with_version("s_b", &b)
        .with_version("s_c", &c);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_a", "alpha", &a),
            catalog_entry("s_b", "beta", &b),
            catalog_entry("s_c", "gamma", &c),
        ],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![
                WireChannelSkill {
                    skill_id: "s_a".into(),
                    name: "alpha".into(),
                },
                WireChannelSkill {
                    skill_id: "s_b".into(),
                    name: "beta".into(),
                },
                WireChannelSkill {
                    skill_id: "s_c".into(),
                    name: "gamma".into(),
                },
            ],
        }],
    );
    rig.write_global(&format!(
        "[channels]\n\"{HOST}/{WS_NAME}/backend\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let targets = vec!["alpha".to_owned(), "beta".to_owned()];

    // The describe reflects the COMBINED split: one line, both names, one survivor.
    match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &targets,
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Described { data, .. } => {
            assert_eq!(data.items.len(), 1, "one line split once: {:?}", data.items);
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("alpha") && note.contains("beta"), "{note}");
            assert!(note.contains("new rows are written for gamma"), "{note}");
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    assert!(matches!(
        ops::remove_global(
            &ctx,
            &connect(&plane, &dir),
            &targets,
            None,
            true,
            &Default::default()
        )
        .unwrap(),
        ops::RemoveOutcome::Applied(_)
    ));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap();
    let refs: Vec<&str> = doc.rows.iter().map(|r| r.reference.as_str()).collect();
    assert!(
        !refs.contains(&format!("{HOST}/{WS_NAME}/alpha").as_str()),
        "alpha is gone: {text}"
    );
    assert!(
        !refs.contains(&format!("{HOST}/{WS_NAME}/beta").as_str()),
        "beta is gone too — the second split must not resurrect the first: {text}"
    );
    assert!(
        refs.contains(&format!("{HOST}/{WS_NAME}/gamma").as_str()),
        "the survivor keeps flowing: {text}"
    );
}

#[test]
fn a_bare_name_two_rows_answer_to_is_refused_not_guessed() {
    // Two rows deliver a `deploy` — one from a workspace, one from a repo. Taking the first is a
    // coin flip with someone's row; the refusal names both qualified references.
    let rig = Rig::new("ambig-remove");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"latest\"\ndeploy = \"github:o/tools\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME", "{err:?}");
    let msg = ways_out(&err);
    assert!(msg.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{msg}");
    assert!(msg.contains("github.com/o/tools/deploy"), "{msg}");
    // Nothing moved.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("github:o/tools"), "{text}");

    // Spelled in full, exactly one row answers — and it applies.
    let one = format!("{HOST}/{WS_NAME}/deploy");
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
    assert!(
        !text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "{text}"
    );
    assert!(
        text.contains("github:o/tools"),
        "the other row stands: {text}"
    );
}

// =================================================================================================
// The project containment rail — a committed symlink never aims managed bytes out of the checkout.
// =================================================================================================

#[test]
fn a_committed_topos_symlink_refuses_the_project_store() {
    // A repo can commit `.topos` as a symlink exactly as easily as `.claude/skills`. The store is
    // REFUSED, never followed — and a plain directory still works.
    let rig = Rig::new("store-escape");
    let proj = project("store-escape-proj", "[skills]\n");
    let outside = Scratch::new("store-escape-outside");
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".topos")).unwrap();
    let err = crate::sidecar::ensure_project_store(&rig.fs, &proj.0).unwrap_err();
    assert_eq!(err.code(), "PLACEMENT_UNSUPPORTED", "{err:?}");
    assert!(
        err.to_string()
            .contains("does not resolve inside this checkout (the project store)"),
        "{err}"
    );
    assert!(
        !outside.0.join("state").exists(),
        "nothing was written through the symlink"
    );
    // The read-side probe refuses it too, so no report or clean ever visits it.
    assert!(crate::sidecar::existing_project_store(&rig.fs, &proj.0).is_none());

    // A NORMAL checkout still mints its store.
    let ok = project("store-ok-proj", "[skills]\n");
    let layout = crate::sidecar::ensure_project_store(&rig.fs, &ok.0).unwrap();
    assert!(layout.home().exists());
    assert!(crate::sidecar::existing_project_store(&rig.fs, &ok.0).is_some());
}

#[test]
fn a_committed_skills_symlink_is_refused_as_a_placement_root() {
    // The DEFAULT project root gets the override's rail: `.claude/skills` committed as a symlink
    // out of the checkout places nothing, and the sweep says so.
    let rig = Rig::new("root-escape");
    let proj = project("root-escape-proj", "[skills]\n");
    let outside = Scratch::new("root-escape-outside");
    std::fs::create_dir_all(proj.0.join(".claude")).unwrap();
    std::os::unix::fs::symlink(&outside.0, proj.0.join(".claude/skills")).unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    let plan = crate::placement::project_plan(
        &ctx,
        &proj.0,
        "topos_deadbeef",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: Some(WS_NAME),
        },
        None,
        None,
    );
    assert!(
        crate::message::legacy_lines(&plan.refused)
            .into_iter()
            .any(|r| r.starts_with("PLACEMENT_ESCAPES_PROJECT")),
        "the escaping root is refused: {:?}",
        plan.refused
    );
    assert!(
        plan.dirs().all(|t| !t.dir.starts_with(&outside.0)),
        "nothing is aimed outside the checkout: {:?}",
        plan.targets
    );

    // A NORMAL checkout still plans its in-repo dirs.
    let ok = project("root-ok-proj", "[skills]\n");
    let ok_ctx = rig.ctx_at(Some(&ok.0));
    let plan = crate::placement::project_plan(
        &ok_ctx,
        &ok.0,
        "topos_deadbeef",
        topos_harness::PlacementNaming {
            name: Some("deploy"),
            workspace_slug: Some(WS_NAME),
        },
        None,
        None,
    );
    assert!(plan.refused.is_empty(), "{:?}", plan.refused);
    assert!(
        plan.dirs().all(|t| t.dir.starts_with(&ok.0)),
        "{:?}",
        plan.targets
    );
}

// =================================================================================================
// The applied report's cross-store pick is deterministic; cross-scope STALENESS is disclosed and
// a deliberate pin is not.
// =================================================================================================

#[test]
fn a_bundle_held_at_two_versions_reports_the_person_copy_and_says_nothing_about_a_pin() {
    // The wire carries ONE row per (session, bundle). Which store answers must not depend on which
    // checkout the update happened to run from — the PERSON store answers whenever it holds the
    // bundle. The OTHER store's version is a DIFFERENCE, and here a deliberate one: the project
    // row is pinned, so no line is earned. Difference is the design working; only staleness is
    // news, and only when a command from here would fix it.
    let rig = Rig::new("split-report");
    rig.seed_session();
    rig.seed_feed();
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let v1_hex = topos_core::digest::to_hex(&v1.id);
    let proj = project(
        "proj-split",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"{v1_hex}\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    // The FEED serves the current version; the project row is PINNED to the older one.
    plane.serves(vec![delivered("s_deploy", "deploy", &v2)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v2)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep_both(&ctx, &plane, &dir);

    assert_eq!(
        std::fs::read(rig.skills().join("deploy/SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the person scope takes the served current"
    );
    assert_eq!(
        std::fs::read(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the project scope holds its pin"
    );
    // The reported row is the PERSON store's — deterministically, not by iteration luck.
    let reported = plane.reported.lock().unwrap().clone();
    let row = reported
        .iter()
        .find(|(id, ..)| id == "s_deploy")
        .unwrap_or_else(|| panic!("the bundle is reported: {reported:?}"));
    assert_eq!(row.1, topos_core::digest::to_hex(&v2.id), "{reported:?}");
    // The pin is a CHOICE, so the receipt says nothing about it: no staleness entry, no line, and
    // above all no internal warning code on a run where everything went right.
    assert!(
        out.data.behind_elsewhere.is_empty(),
        "a pinned row is deliberate, never behind: {:?}",
        out.data.behind_elsewhere
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(!tty.contains("behind"), "{tty}");
    assert!(!tty.contains("VERSION_SPLIT"), "{tty}");
    assert!(!tty.contains(&v1_hex[..12]), "{tty}");
}

#[test]
fn the_machine_copy_left_behind_by_a_project_update_earns_the_counted_trailer() {
    // The other half of the rule: a copy that differs because it is simply OLD — no pin, no `off`,
    // nothing deliberate — IS news, and the receipt closes with the count and the one command
    // that fixes it. This run drives the PROJECT (the scope rule), so the machine-wide copy is
    // exactly what nothing here touched.
    let rig = Rig::new("behind-machine");
    rig.seed_session();
    rig.seed_feed();
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let proj = project(
        "proj-behind",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());

    // Both scopes land v1 (the background sweep drives both).
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep_both(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(rig.skills().join("deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n"
    );

    // Current moves; a HAND-RUN update from inside the checkout drives the project alone.
    let mut served = delivered("s_deploy", "deploy", &v2);
    served.generation = 2;
    plane.serves(vec![served]);
    let mut listed = catalog_entry("s_deploy", "deploy", &v2);
    listed.generation = 2;
    let dir = FakeDirectory::new(vec![listed], Vec::new());
    let out = update_hand_run(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the scope you stand in is the one that moves"
    );
    assert_eq!(
        std::fs::read(rig.skills().join("deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the machine-wide copy was never driven"
    );
    assert_eq!(
        out.data.behind_elsewhere,
        vec![topos_types::results::BehindElsewhere {
            bundle: "deploy".to_owned(),
            project_dir: None,
        }],
        "the machine's own copy is the one behind"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(
        tty.ends_with("1 bundle behind machine-wide — `topos update -g` updates it."),
        "{tty}"
    );

    // Run THAT command and the line is gone — a trailer nobody can clear is a nag, not a receipt.
    let after = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        after.data.behind_elsewhere.is_empty(),
        "{:?}",
        after.data.behind_elsewhere
    );
}

/// The PINNED half of the staleness rule, reached for real. The two tests around it never touch
/// it: one pins the scope the run DRIVES (so the driven-scope filter answers first and the pin is
/// never consulted), the other uses `"off"`. Here the pin sits on the scope this run leaves alone,
/// which is the only arrangement in which the pin itself decides — and the control run, the same
/// fixture with `"*"` in place of the pin, proves the silence is the PIN's doing and not the
/// fixture's.
#[test]
fn a_pin_on_the_scope_this_run_left_alone_is_never_reported_behind() {
    let rig = Rig::new("behind-pinned");
    rig.seed_session();
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let v1_hex = topos_core::digest::to_hex(&v1.id);
    let proj = project(
        "proj-pinned",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());

    // The MACHINE recipe pins the bundle to v1; the project takes whatever is current. Both land
    // v1 on the first sweep, which drives both scopes.
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n\n[skills]\n\
         \"{HOST}/{WS_NAME}/deploy\" = \"{v1_hex}\"\n"
    ));
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep_both(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(rig.skills().join("deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n"
    );

    // Current moves; a hand-run update from inside the checkout drives the PROJECT alone, so the
    // machine scope is exactly the one nothing here touched — the arrangement that makes the pin
    // the deciding fact.
    let mut served = delivered("s_deploy", "deploy", &v2);
    served.generation = 2;
    plane.serves(vec![served.clone()]);
    let mut listed = catalog_entry("s_deploy", "deploy", &v2);
    listed.generation = 2;
    let dir2 = FakeDirectory::new(vec![listed.clone()], Vec::new());
    let out = update_hand_run(&ctx, &plane, &dir2);
    assert_eq!(
        std::fs::read(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        b"# deploy v2\n",
        "the scope you stand in is the one that moves"
    );
    assert_eq!(
        std::fs::read(rig.skills().join("deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the pinned machine copy is materially behind v2 — deliberately"
    );
    assert!(
        out.data.behind_elsewhere.is_empty(),
        "a pinned row is a choice no `topos update` would undo: {:?}",
        out.data.behind_elsewhere
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(!tty.contains("behind"), "{tty}");

    // THE CONTROL. Same fixture, same undriven scope, same version gap — only the pin removed.
    // The line appears, so the silence above is the pin's and nothing else's.
    let rig = Rig::new("behind-unpinned");
    rig.seed_session();
    let proj = project(
        "proj-unpinned",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n\n[skills]\n\
         \"{HOST}/{WS_NAME}/deploy\" = \"latest\"\n"
    ));
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    sweep_both(&ctx, &plane, &dir);
    plane.serves(vec![served]);
    let dir2 = FakeDirectory::new(vec![listed], Vec::new());
    let out = update_hand_run(&ctx, &plane, &dir2);
    assert_eq!(
        out.data.behind_elsewhere,
        vec![topos_types::results::BehindElsewhere {
            bundle: "deploy".to_owned(),
            project_dir: None,
        }],
        "with no pin the same gap IS news"
    );
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(
        tty.ends_with("1 bundle behind machine-wide — `topos update -g` updates it."),
        "{tty}"
    );
}

/// The reason there is no SET-level branch in the staleness rule: a channel — the only set a
/// workspace bundle arrives through — cannot carry a pin at all. The grammar refuses it in both
/// spellings, so "a pinned set delivers this bundle" is not a state a manifest can express, and a
/// rule about it would be one no fixture could ever reach.
#[test]
fn a_channel_cannot_be_pinned_so_a_set_is_never_the_deliberate_fact() {
    let rig = Rig::new("behind-pinned-set");
    let v1 = one_file(b"# deploy v1\n");
    let v1_hex = topos_core::digest::to_hex(&v1.id);
    for body in [
        format!("[channels]\n\"{HOST}/{WS_NAME}/backend\" = \"{v1_hex}\"\n"),
        format!("[channels]\n\"{HOST}/{WS_NAME}/backend\" = {{ version = \"{v1_hex}\" }}\n"),
    ] {
        rig.write_global(&body);
        let err = crate::manifest::scopes::person_plan(&rig.fs, &rig.layout())
            .expect_err("a pinned channel is refused at the parser");
        let message = crate::render::safe_message(&err);
        assert!(message.contains("channel takes no pin"), "{message}");
    }
}

#[test]
fn an_off_row_is_never_reported_behind() {
    // The `"off"` switch is the other deliberate spelling: the machine keeps its copy but stops
    // taking the workspace's version of it. Nothing would ever bring it to current, so a line
    // saying it is behind could never be cleared.
    let rig = Rig::new("behind-off");
    rig.seed_session();
    let v1 = one_file(b"# deploy v1\n");
    let v2 = one_file(b"# deploy v2\n");
    let proj = project(
        "proj-off",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    rig.seed_feed();
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep_both(&ctx, &plane, &dir);

    // The machine switches the bundle off AFTER its copy landed, then current moves on. The `off`
    // row is global-file-only, so the copy STAYS on disk at v1 — materially behind v2, and
    // deliberately so.
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n\n[skills]\n\
         \"{HOST}/{WS_NAME}/deploy\" = \"off\"\n"
    ));
    let mut served = delivered("s_deploy", "deploy", &v2);
    served.generation = 2;
    plane.serves(vec![served]);
    let mut listed = catalog_entry("s_deploy", "deploy", &v2);
    listed.generation = 2;
    let dir = FakeDirectory::new(vec![listed], Vec::new());
    let out = update_hand_run(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(rig.skills().join("deploy/SKILL.md")).unwrap(),
        b"# deploy v1\n",
        "the switched-off copy is left exactly where it was"
    );
    assert!(
        out.data.behind_elsewhere.is_empty(),
        "an `off` row is a choice: {:?}",
        out.data.behind_elsewhere
    );
}

// =================================================================================================
// The machine-local registries fail CLOSED on a document this build cannot decipher.
// =================================================================================================

/// A `state/<doc>` written at a schema version FROM THE FUTURE — what a newer build leaves behind
/// when someone downgrades, or runs two versions side by side.
fn write_newer_schema_doc(layout: &Layout, path: &std::path::Path, body: &str) -> Vec<u8> {
    std::fs::create_dir_all(layout.state_dir()).unwrap();
    let bytes = body.as_bytes().to_vec();
    std::fs::write(path, &bytes).unwrap();
    bytes
}

#[test]
fn a_newer_visited_store_index_contributes_nothing_and_is_never_written_over() {
    let rig = Rig::new("visited-newer");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let path = rig.layout().visited_stores_path();
    let bytes = write_newer_schema_doc(
        &rig.layout(),
        &path,
        "{\n  \"schema_version\": 9999,\n  \"stores\": [\"/nowhere\"]\n}\n",
    );
    let layouts = crate::visited_stores::recall_and_record(&ctx, &[]);
    assert!(layouts.is_empty(), "no recorded store is recalled");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        bytes,
        "the newer document is byte-untouched"
    );
}
