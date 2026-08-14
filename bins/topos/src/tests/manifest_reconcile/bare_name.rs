//! The BARE-NAME ladder: one namespace made of the local inventory AND the connected catalogs,
//! resolved STANDING first — what the two scopes already know the name to be — with the chooser
//! that offers every spelling when nothing decides.

use std::sync::{Arc, Mutex};

use crate::error::ClientError;
use crate::{ops, sync_status};

use super::rig::*;

// =================================================================================================
// The BARE-NAME ladder: one namespace, made of the local inventory AND the connected catalogs.
// =================================================================================================

#[test]
fn a_name_only_a_workspace_publishes_resolves_to_its_reference() {
    let (rig, plane, dir, _v) = bare_rig("bare-ws-only");
    match bare_plan(&rig, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Reference { reference, note } => {
            // The CANONICAL host-qualified spelling — unambiguous however many servers this
            // machine is logged into.
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/{BARE}"));
            assert!(
                note.unwrap_or_default().contains(WS_NAME),
                "the answer says which workspace resolved the name"
            );
        }
        other => panic!("the workspace's copy is the only thing that name can mean: {other:?}"),
    }
    // A name NOBODY has stays the plain not-found — the workspace half never invents a match.
    let err = bare_plan(&rig, &plane, &dir, "zx-bare-ghost", true).unwrap_err();
    assert_eq!(err.code(), "NO_UNTRACKED_SKILL");
}

/// Record [`BARE`] as delivered by the rig's workspace — the machine's own delivery history, which
/// is the ONLY thing that nominates a workspace for a clean local resolve's receipt disclosure (a
/// local adopt never fans out across every session's catalog just for a courtesy line).
fn seed_delivered_bare(rig: &Rig) {
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )],
    )
    .unwrap();
}

#[test]
fn a_local_folder_and_a_team_copy_of_one_name_are_a_chooser() {
    let (rig, plane, dir, _v) = bare_rig("bare-both");
    // The SAME bytes the workspace serves, sitting untracked in the home agent dir. Two things
    // answer to the name and they are DIFFERENT bundles — adopting the folder would fork the
    // team's process silently, so neither is picked for the person.
    seed_delivered_bare(&rig);
    let path = untracked_skill(&rig.home.0, BARE, b"# deploy\n");

    let err = bare_plan(&rig, &plane, &dir, BARE, true).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME");
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    assert_eq!(
        envelope.data["candidates"],
        serde_json::json!([
            format!("{HOST}/{WS_NAME}/{BARE}"),
            path.display().to_string(),
        ]),
        "the remote spelling first, then the folder — ABSOLUTE, because an agent execs this"
    );
    assert_eq!(
        envelope.next_actions.len(),
        2,
        "one runnable command per way out: {:?}",
        envelope.next_actions
    );

    // NOTHING publishes the name: the one local folder is the only candidate, and it adopts with
    // no disclosure to make.
    let rig3 = Rig::new("bare-local-only");
    rig3.seed_session();
    let only = untracked_skill(&rig3.home.0, BARE, b"# deploy\n");
    let empty = FakeDirectory::new(Vec::new(), Vec::new());
    match bare_plan(&rig3, &plane, &empty, BARE, true).unwrap() {
        ops::BareAddPlan::Adopt {
            path: p,
            name,
            published,
        } => {
            assert_eq!(p, only);
            assert_eq!(name, BARE);
            assert!(published.is_none());
        }
        other => panic!("the one local copy is the only candidate: {other:?}"),
    }
}

#[test]
fn a_form_that_cannot_subscribe_still_discloses_the_team_spelling_on_its_adopt() {
    // An `-s` member pick is about a repo, so it never resolves a name toward a workspace — and
    // the local adopt it narrows keeps the bounded courtesy disclosure, judged against the bytes
    // that actually landed.
    let (rig, plane, dir, _v) = bare_rig("bare-disclose");
    seed_delivered_bare(&rig);
    let path = untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let published = match bare_plan(&rig, &plane, &dir, BARE, false).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => {
            published.expect("the one workspace publishing the name is disclosed")
        }
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    let data = ops::add_with_name(
        &ctx,
        &path,
        Some(BARE),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    let same = published.suggestion(data.bundle_digest.as_deref().unwrap_or_default());
    assert_eq!(same.reference, format!("{HOST}/{WS_NAME}/{BARE}"));
    assert_eq!(same.workspace, WS_NAME);
    assert!(same.identical, "byte-identical to what the catalog serves");

    // A local copy that has DRIFTED from the team's version is disclosed just the same — only the
    // identical claim goes away.
    let rig2 = Rig::new("bare-both-drift");
    rig2.seed_session();
    seed_delivered_bare(&rig2);
    let drifted = untracked_skill(&rig2.home.0, BARE, b"# deploy (mine)\n");
    let ctx2 = rig2.ctx_at(Some(&rig2.work.0));
    let published2 = match bare_plan(&rig2, &plane, &dir, BARE, false).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => published.expect("still disclosed"),
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    let data2 = ops::add_with_name(
        &ctx2,
        &drifted,
        Some(BARE),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    assert!(
        !published2
            .suggestion(data2.bundle_digest.as_deref().unwrap_or_default())
            .identical
    );

    // A name the workspace publishes but never delivered HERE adopts with no disclosure at all —
    // the local act does not go asking every catalog for one.
    let rig3 = Rig::new("bare-both-undelivered");
    rig3.seed_session();
    untracked_skill(&rig3.home.0, BARE, b"# deploy\n");
    match bare_plan(&rig3, &plane, &dir, BARE, false).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => assert!(published.is_none()),
        other => panic!("a local copy adopts in place: {other:?}"),
    }
}

#[test]
fn a_name_several_workspaces_publish_offers_every_spelling() {
    let (rig, plane, dir, _v) = bare_rig("bare-two-ws");
    seed_second_session(&rig);

    let err = bare_plan(&rig, &plane, &dir, BARE, true).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME");
    assert_eq!(
        chooser_candidates(&err),
        vec![
            format!("{HOST}/{WS_NAME}/{BARE}"),
            format!("beta.test/ops/{BARE}"),
        ],
        "remote spellings, sorted"
    );
    // The machine-readable half: one runnable command per spelling.
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    assert_eq!(
        envelope.next_actions.len(),
        2,
        "{:?}",
        envelope.next_actions
    );

    // The global `--workspace` selector settles it: only the named workspace is probed, so the
    // same machine records deterministically the one that was ASKED — never the other.
    match plan_bare_add_in(&rig, &plane, &dir, BARE, true, Some("w_ops")).unwrap() {
        ops::BareAddPlan::Reference { reference, .. } => {
            assert_eq!(reference, format!("beta.test/ops/{BARE}"));
        }
        other => panic!("the selector names the workspace: {other:?}"),
    }

    // A LOCAL copy on top of the two workspaces joins the same list — three things answer to the
    // name, and the folder is spelled in its portable form after the remote pair.
    let local = untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    let err = bare_plan(&rig, &plane, &dir, BARE, true).unwrap_err();
    assert_eq!(
        chooser_candidates(&err),
        vec![
            format!("{HOST}/{WS_NAME}/{BARE}"),
            format!("beta.test/ops/{BARE}"),
            local.display().to_string(),
        ]
    );
}

#[test]
fn a_name_in_two_folders_offers_both_and_the_team_copy_beside_them() {
    let (rig, plane, dir, _v) = bare_rig("bare-scope");
    // The same name in the home AND project skills folders, and a workspace publishes it too:
    // three ways out, all in one list.
    let home = untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    let project = untracked_skill(&rig.work.0, BARE, b"# deploy\n");

    let err = bare_plan(&rig, &plane, &dir, BARE, true).unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME");
    assert_eq!(
        chooser_candidates(&err),
        vec![
            format!("{HOST}/{WS_NAME}/{BARE}"),
            home.display().to_string(),
            project.display().to_string(),
        ],
        "the remote spelling first, then the folders sorted — every one ABSOLUTE in argv"
    );
    // Every line is a runnable command carrying that one candidate.
    let envelope = crate::render::err_envelope("add", &["add".to_owned()], &err);
    let offered: Vec<String> = envelope
        .next_actions
        .iter()
        .map(|a| a.argv.join(" "))
        .collect();
    assert_eq!(
        offered,
        chooser_candidates(&err)
            .iter()
            .map(|c| format!("topos add {c} --json"))
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_selector_or_harness_form_never_subscribes() {
    let (rig, plane, dir, _v) = bare_rig("bare-gated");
    // `-s`/`-a` narrow a LOCAL adopt, so the fully-bare gate is closed: today's answer, exactly.
    let err = bare_plan(&rig, &plane, &dir, BARE, false).unwrap_err();
    assert_eq!(err.code(), "NO_UNTRACKED_SKILL");
    // Same for a second workspace publishing it — the gate is about the FORM, not the count.
    seed_second_session(&rig);
    assert_eq!(
        bare_plan(&rig, &plane, &dir, BARE, false)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );
    // A `@harness` suffix names a local harness's dir — it cannot reach the subscribe arm at all.
    let err = bare_plan(&rig, &plane, &dir, &format!("{BARE}@aider-desk"), true).unwrap_err();
    assert_eq!(err.code(), "HARNESS_NOT_FOUND");
}

#[test]
fn a_cache_only_match_subscribes_and_an_unanswering_session_is_skipped() {
    let (rig, plane, dir, _v) = bare_rig("bare-offline");
    // The catalog cannot be read at all — a transport fault, never an existence claim.
    dir.set_unavailable(true);
    assert_eq!(
        bare_plan(&rig, &plane, &dir, BARE, true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL",
        "an unreachable directory answers nothing; it never fabricates a match"
    );

    // The offline delivery cache remembers what this workspace last delivered here — enough to
    // spell the reference, never enough to claim the bytes agree.
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )],
    )
    .unwrap();
    match bare_plan(&rig, &plane, &dir, BARE, true).unwrap() {
        ops::BareAddPlan::Reference { reference, .. } => {
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/{BARE}"));
        }
        other => panic!("the cache alone is enough to spell the reference: {other:?}"),
    }
    // And with a local copy beside it, the disclosure carries no identical claim: a cached row
    // holds a served VERSION id, which is a different hash from a bundle digest.
    let path = untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let published = match bare_plan(&rig, &plane, &dir, BARE, false).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => published.expect("disclosed"),
        other => panic!("a local copy adopts in place: {other:?}"),
    };
    let data = ops::add_with_name(
        &ctx,
        &path,
        Some(BARE),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    assert!(
        !published
            .suggestion(data.bundle_digest.as_deref().unwrap_or_default())
            .identical
    );
}

// =================================================================================================
// A bare name resolves STANDING first: what the two scopes already know it to be.
// =================================================================================================

/// A rig whose MACHINE store already holds `alpha`, delivered by the workspace's feed, plus a
/// stray untracked folder of the same name in an agent dir — the shape a real machine is in.
fn standing_rig(tag: &str) -> (Rig, FakePlane, FakeDirectory) {
    let rig = Rig::new(tag);
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# alpha\n");
    let plane = FakePlane::new(log).with_version("s_alpha", &v);
    plane.serves(vec![delivered("s_alpha", "alpha", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_alpha", "alpha", &v)], Vec::new());
    sweep(&rig.ctx_at(Some(&rig.work.0)), &plane, &dir);
    untracked_skill(&rig.home.0, "alpha", b"# a stray copy\n");
    (rig, plane, dir)
}

#[test]
fn a_name_this_scope_already_records_answers_already_added_not_ambiguous() {
    let (rig, plane, dir) = standing_rig("standing-here");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(rig.work.0.clone()),
    };
    // The machine scope holds it AND a stray folder of the same name sits in an agent dir. The
    // standing record answers first, so the stray never enters a count — no ambiguity at all.
    let err = ops::plan_bare_add(
        &ctx,
        &connect(&plane, &dir),
        &roots,
        "alpha",
        ops::BareAdd {
            global: true,
            ..bare_opts(true)
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "ALREADY_TRACKED");
    // The standing record answers first, so the stray never enters a count — and it is still
    // LISTED underneath, because "what do I do about that folder?" is the question a person
    // standing in front of it actually has.
    assert_eq!(
        err.to_string(),
        format!(
            "alpha is already added machine-wide (~/.topos/topos.toml)\nsource: \
             {HOST}/{WS_NAME}/alpha\n'alpha' is also in 1 unmanaged folder here:\n  \
             ~/.aider-desk/skills/alpha — edited (adopting it makes these your draft)\nname the \
             one to adopt: topos add -g <folder> --as {HOST}/{WS_NAME}/alpha"
        )
    );
}

#[test]
fn a_name_the_other_scope_records_is_added_here_from_that_source() {
    let (rig, plane, dir) = standing_rig("standing-other");
    // A project covering the cwd: nothing stands in ITS scope, so the name means what the machine
    // already means by it — recorded HERE, with no question asked.
    let proj = project("standing-other-proj", "[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(proj.0.clone()),
    };
    match ops::plan_bare_add(
        &ctx,
        &connect(&plane, &dir),
        &roots,
        "alpha",
        bare_opts(true),
    )
    .unwrap()
    {
        ops::BareAddPlan::Reference { reference, note } => {
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/alpha"));
            assert!(note.is_none(), "nothing to explain — {note:?}");
        }
        other => panic!("the other scope's record resolves the name: {other:?}"),
    }
}

/// A bare `add <name> -a/--dest …` — the invocation that asks about DESTINATIONS, not existence.
fn plan_dest_add(
    rig: &Rig,
    plane: &FakePlane,
    dir: &FakeDirectory,
    cwd: &std::path::Path,
    name: &str,
    global: bool,
) -> Result<ops::BareAddPlan, ClientError> {
    let ctx = rig.ctx_at(Some(cwd));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(cwd.to_path_buf()),
    };
    ops::plan_bare_add(
        &ctx,
        &connect(plane, dir),
        &roots,
        name,
        ops::BareAdd {
            subscribe: true,
            dest_selected: true,
            global,
            workspace: None,
        },
    )
}

#[test]
fn a_standing_name_with_a_destination_extends_its_row_instead_of_answering_already_added() {
    let (rig, plane, dir) = standing_rig("standing-extend");
    // MACHINE scope: the bundle stands here, and the invocation named a folder. "Already added"
    // would be true and useless — the row this file spells gains the destination instead.
    match plan_dest_add(&rig, &plane, &dir, &rig.work.0, "alpha", true).unwrap() {
        ops::BareAddPlan::Reference { reference, note } => {
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/alpha"));
            assert!(note.is_none(), "{note:?}");
        }
        other => panic!("the standing row is what gains the destination: {other:?}"),
    }
    // Without the flags, the same invocation is still the already-added answer.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(rig.work.0.clone()),
    };
    assert_eq!(
        ops::plan_bare_add(
            &ctx,
            &connect(&plane, &dir),
            &roots,
            "alpha",
            ops::BareAdd {
                global: true,
                ..bare_opts(true)
            },
        )
        .unwrap_err()
        .code(),
        "ALREADY_TRACKED"
    );

    // THE SET-LINE MEMBER. `alpha` has no row of its own here — the workspace FEED line delivers
    // it, exactly as a channel line would. There is no row to extend, so the reference arm writes
    // the member its OWN row carrying the destination: the same move `remove`'s per-agent refusal
    // teaches, reached without anyone having to be told about it.
    let data = applied_dest_add(
        &ctx,
        &plane,
        &dir,
        &format!("{HOST}/{WS_NAME}/alpha"),
        &["~/.cursor/skills"],
    );
    assert!(
        data.dest_change.is_none(),
        "a row born here is not an extend: {:?}",
        data.dest_change
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!(
            "\"{HOST}/{WS_NAME}/alpha\" = {{ dest = [\"~/.cursor/skills\"] }}"
        )),
        "the member gained its own row, frozen to the folder that was named: {text}"
    );

    // PROJECT scope: nothing stands in the project, so the name still resolves through the other
    // scope — an ordinary add of that reference, carrying the flags exactly as a spelled-out
    // reference would.
    let proj = project("standing-extend-proj", "[bundles]\n");
    match plan_dest_add(&rig, &plane, &dir, &proj.0, "alpha", false).unwrap() {
        ops::BareAddPlan::Reference { reference, .. } => {
            assert_eq!(reference, format!("{HOST}/{WS_NAME}/alpha"));
        }
        other => panic!("the other scope's record resolves the name: {other:?}"),
    }
}

#[test]
fn a_standing_folder_with_a_destination_extends_that_folders_own_row() {
    // The same rule for a bundle adopted from a folder: no second adopt, no new version — the
    // path row this scope already spells gains the destination, and the receipt says which.
    let rig = Rig::new("standing-extend-folder");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let folder = untracked_skill(&rig.home.0, "alpha", b"# alpha\n");
    let mut adopted = ops::add_with_name(
        &ctx,
        &folder,
        Some("alpha"),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    let scope = ops::add_scope(&ctx, true).unwrap();
    ops::note_added_path_in(&ctx, &mut adopted, &scope.target, &folder).unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());

    let ops::BareAddPlan::ExtendFolderDest { dir: planned } =
        plan_dest_add(&rig, &plane, &dir, &rig.work.0, "alpha", true).unwrap()
    else {
        panic!("the standing folder's own row gains the destination");
    };
    assert_eq!(planned, folder);

    // The row named NO destinations, so the new one joins the token standing for its reach.
    let selection = ops::dest_select::Selection::new(&[], &["~/.cursor/skills".to_owned()]);
    let data = ops::extend_folder_dest(&ctx, &scope, &folder, &selection)
        .unwrap()
        .expect("the folder is tracked here");
    let change = data.dest_change.clone().expect("the row gained a folder");
    assert_eq!(change.added, vec!["~/.cursor/skills".to_owned()]);
    assert!(change.default_reach, "the row kept what it already reached");
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "remove".to_owned(),
            "-g".to_owned(),
            folder.display().to_string(),
            "--dest".to_owned(),
            "~/.cursor/skills".to_owned(),
        ],
        "the inverse subtracts exactly what this add put on the row"
    );
    // No adopt happened, so nothing was armed and no version was minted a second time.
    assert!(data.currency.is_none());
    assert_eq!(data.version_id, adopted.version_id);

    // The SAME destination again changes nothing, and says so.
    let again = ops::extend_folder_dest(&ctx, &scope, &folder, &selection)
        .unwrap()
        .expect("the folder is tracked here");
    assert!(again.dest_change.is_none());
    assert!(again.undo.is_empty());
    assert!(
        again
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("nothing changed"),
        "{:?}",
        again.note
    );

    // A SECOND destination joins the first — neither replaces the other.
    let more = ops::dest_select::Selection::new(&[], &["~/.zed/skills".to_owned()]);
    let data = ops::extend_folder_dest(&ctx, &scope, &folder, &more)
        .unwrap()
        .expect("the folder is tracked here");
    assert_eq!(
        data.dest_change.expect("a second folder").added,
        vec!["~/.zed/skills".to_owned()]
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains("~/.cursor/skills") && text.contains("~/.zed/skills"),
        "{text}"
    );
}

#[test]
fn a_member_pick_asks_about_a_repo_so_the_standing_rungs_stay_out_of_it() {
    // `-s <member>` says the source is a REPO holding several skills. A standing record is not
    // one, and letting it answer turned `add <name> -s <member>` into a re-add of something else
    // entirely — the selector silently dropped.
    let (rig, plane, dir) = standing_rig("standing-selector");
    let proj = project("standing-selector-proj", "[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(proj.0.clone()),
    };
    // Bare: the machine's record resolves the name into this project.
    assert!(matches!(
        ops::plan_bare_add(
            &ctx,
            &connect(&plane, &dir),
            &roots,
            "alpha",
            bare_opts(true)
        ),
        Ok(ops::BareAddPlan::Reference { .. })
    ));
    // With `-s`, the standing record never answers: resolution falls to the local inventory the
    // selector is about, and the machine's copy of the bundle is not consulted at all.
    match ops::plan_bare_add(
        &ctx,
        &connect(&plane, &dir),
        &roots,
        "alpha",
        bare_opts(false),
    ) {
        Ok(ops::BareAddPlan::Adopt { .. }) => {}
        other => panic!("a member pick resolves against the inventory alone: {other:?}"),
    }
    // And with nothing in the inventory either, the answer is about the name this machine tracks
    // — never a silent re-add of something the selector was not asked about.
    let bare_rig = Rig::new("standing-selector-empty");
    bare_rig.seed_session();
    let bctx = bare_rig.ctx_at(Some(&bare_rig.work.0));
    let folder = untracked_skill(&bare_rig.home.0, "beta", b"# beta\n");
    ops::add_with_name(
        &bctx,
        &folder,
        Some("beta"),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    let broots = ops::DiscoveryRoots {
        home: bare_rig.home.0.clone(),
        cwd: Some(bare_rig.work.0.clone()),
    };
    assert_eq!(
        ops::plan_bare_add(
            &bctx,
            &connect(&plane, &dir),
            &broots,
            "beta",
            bare_opts(false)
        )
        .unwrap_err()
        .code(),
        "ALREADY_TRACKED"
    );
}

#[test]
fn a_folder_that_is_gone_names_nothing_and_is_never_offered() {
    // A candidate is a line to paste. An adopted folder someone deleted would paste into a
    // missing-source refusal, and a `source:` line pointing at it sends a reader nowhere.
    let rig = Rig::new("standing-vanished");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let folder = untracked_skill(&rig.home.0, "alpha", b"# alpha\n");
    ops::add_with_name(
        &ctx,
        &folder,
        Some("alpha"),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    std::fs::remove_dir_all(&folder).unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(rig.work.0.clone()),
    };
    let err = ops::plan_bare_add(
        &ctx,
        &connect(&plane, &dir),
        &roots,
        "alpha",
        ops::BareAdd {
            global: true,
            ..bare_opts(true)
        },
    )
    .unwrap_err();
    // The record still STANDS — it is added — but nothing on this machine can name where it came
    // from, so the answer says less rather than something false.
    assert_eq!(err.code(), "ALREADY_TRACKED");
    assert_eq!(
        err.to_string(),
        "alpha is already added machine-wide (~/.topos/topos.toml)",
        "no `source:` line for a folder that is not there"
    );
}

#[test]
fn a_path_re_add_naming_destinations_extends_instead_of_refusing_as_already_tracked() {
    // `topos add ./foo --dest B` on a folder this scope already tracks is about WHERE the copies
    // live. It used to reach `adopt_path`, which refuses a second adopt of one folder — so the
    // union could never happen through the path door at all.
    let rig = Rig::new("path-re-add");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let folder = untracked_skill(&rig.home.0, "alpha", b"# alpha\n");
    let mut adopted = ops::add_with_name(
        &ctx,
        &folder,
        Some("alpha"),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    let scope = ops::add_scope(&ctx, true).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut adopted,
        &scope.target,
        &folder,
        &["~/.codex/skills".to_owned()],
    )
    .unwrap();

    // The SAME folder again, naming a second destination: the standing row gains it.
    let selection = ops::dest_select::Selection::new(&[], &["~/.cursor/skills".to_owned()]);
    let data = ops::extend_folder_dest(&ctx, &scope, &folder, &selection)
        .unwrap()
        .expect("the folder is tracked in this scope");
    assert_eq!(
        data.dest_change.expect("the row gained a folder").added,
        vec!["~/.cursor/skills".to_owned()]
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains("\"~/.codex/skills\", \"~/.cursor/skills\""),
        "the standing destination survives and the new one is appended: {text}"
    );

    // A folder NOTHING here tracks is not a re-add at all — the caller adopts as it always would.
    let fresh = untracked_skill(&rig.home.0, "beta", b"# beta\n");
    assert!(
        ops::extend_folder_dest(&ctx, &scope, &fresh, &selection)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_locally_adopted_record_answers_with_the_folder_its_bytes_live_in() {
    let rig = Rig::new("standing-local");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let folder = untracked_skill(&rig.home.0, "alpha", b"# alpha\n");
    ops::add_with_name(
        &ctx,
        &folder,
        Some("alpha"),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let roots = ops::DiscoveryRoots {
        home: rig.home.0.clone(),
        cwd: Some(rig.work.0.clone()),
    };
    let err = ops::plan_bare_add(
        &ctx,
        &connect(&plane, &dir),
        &roots,
        "alpha",
        ops::BareAdd {
            global: true,
            ..bare_opts(true)
        },
    )
    .unwrap_err();
    // The `source:` line is the folder, in the spelling that stays portable.
    assert_eq!(
        err.to_string(),
        format!(
            "alpha is already added machine-wide (~/.topos/topos.toml)\nsource: ~/{}",
            folder.strip_prefix(&rig.home.0).unwrap().display()
        )
    );

    // A SECOND record of the same name in the same store: no single answer, so the chooser — one
    // runnable re-add per record, each naming the folder it stands for.
    let second = untracked_skill(&rig.work.0, "alpha", b"# another alpha\n");
    ops::add_with_name(
        &ctx,
        &second,
        Some("alpha"),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();
    let err = ops::plan_bare_add(
        &ctx,
        &connect(&plane, &dir),
        &roots,
        "alpha",
        ops::BareAdd {
            global: true,
            ..bare_opts(true)
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "AMBIGUOUS_NAME");
    let mut expected = vec![
        format!("~/{}", folder.strip_prefix(&rig.home.0).unwrap().display()),
        second.display().to_string(),
    ];
    expected.sort();
    assert_eq!(chooser_candidates(&err), expected);
}

#[test]
fn a_bare_name_subscribe_records_the_canonical_row_and_its_inverse() {
    let (rig, plane, dir, _v) = bare_rig("bare-e2e");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let ops::BareAddPlan::Reference {
        reference,
        note: resolution,
    } = bare_plan(&rig, &plane, &dir, BARE, true).unwrap()
    else {
        panic!("nothing local carries the name");
    };
    let resolution = resolution.expect("the resolution is disclosed");

    // With NO manifest covering this folder the subscribe refuses toward the two scopes — it
    // never invents a file. `topos init` is what creates one.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &reference,
        false,
        false,
        &Default::default(),
        None,
    ) {
        Err(e) => assert_eq!(e.code(), "NO_MANIFEST"),
        Ok(_) => panic!("no topos.toml covers this folder — the subscribe must refuse"),
    }
    ops::init(&ctx, false).expect("the folder's manifest");

    // The composition root's own hand-off: the resolved reference goes through the ORDINARY
    // reference arm, so the row, the delivery, and the receipt shape are the spelled-out ones.
    let mut data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &reference,
        false,
        false,
        &Default::default(),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => {
            panic!("a workspace reference applies immediately")
        }
    };
    ops::push_note(&mut data, resolution);

    let manifest = rig.work.0.join(crate::manifest::MANIFEST_FILE);
    assert_eq!(data.manifest.as_deref(), Some(manifest.to_str().unwrap()));
    assert_eq!(data.reference.as_deref(), Some(reference.as_str()));
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(text.contains(&format!("\"{reference}\"")), "{text}");
    assert_eq!(
        data.undo,
        vec!["topos".to_owned(), "remove".to_owned(), reference.clone()],
        "the inverse is the project-scope remove of the same key"
    );
    let note = data.note.expect("the resolution is disclosed");
    assert!(note.contains(BARE) && note.contains(&reference), "{note}");
}

#[test]
fn a_cache_row_of_an_ended_session_or_a_withdrawal_resolves_nothing() {
    let (rig, plane, dir, _v) = bare_rig("bare-stale-cache");
    // The catalog is unreachable, so ONLY the cache could answer — exactly the offline posture.
    dir.set_unavailable(true);

    // A leftover row from a workspace whose session has ENDED (`logout` removes the session but
    // leaves `sync_status.json` behind): resolving it would aim the subscribe at a workspace this
    // machine can no longer reach — an honest not-found is the only right answer.
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            "w_gone".to_owned(),
            sync_status::WorkspaceSync {
                host: Some("gone.test".to_owned()),
                workspace_name: Some("gone".to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )],
    )
    .unwrap();
    assert_eq!(
        bare_plan(&rig, &plane, &dir, BARE, true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL",
        "an ended session's cache row keeps no say in the namespace"
    );

    // A row the workspace since WITHDREW — the live session stands, but withdrawn is not
    // published, whatever the cache still holds.
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
                        withdrawn: true,
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )],
    )
    .unwrap();
    assert_eq!(
        bare_plan(&rig, &plane, &dir, BARE, true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );
}

#[test]
fn an_unspellable_catalog_name_stays_the_plain_not_found() {
    // A workspace CAN publish a bundle named `channels` (nothing upstream reserves it yet), but
    // the manifest grammar reserves that spelling as an incomplete channel reference — so the
    // ladder must answer exactly as before the workspace half existed, never hand the reference
    // arm a key it will refuse.
    let rig = Rig::new("bare-unspellable");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# channels\n");
    let plane = FakePlane::new(log).with_version("s_ch", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_ch", "channels", &v)], Vec::new());
    assert_eq!(
        bare_plan(&rig, &plane, &dir, "channels", true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );
}

#[test]
fn a_read_catalog_clears_the_cache_row_it_no_longer_carries() {
    let (rig, plane, _dir, _v) = bare_rig("bare-cache-invalidate");
    // The last delivery still remembers the name…
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                delivered: [(
                    "s_bare".to_owned(),
                    sync_status::DeliveredSkill {
                        name: BARE.to_owned(),
                        ..Default::default()
                    },
                )]
                .into_iter()
                .collect(),
                ..Default::default()
            },
        )],
    )
    .unwrap();
    // …but the workspace's catalog ANSWERS and no longer carries it (deleted or archived since).
    // The answered read is authoritative: the stale row must not fabricate a subscribe that
    // `add_reference` would then refuse as not-available.
    let empty = FakeDirectory::new(Vec::new(), Vec::new());
    assert_eq!(
        bare_plan(&rig, &plane, &empty, BARE, true)
            .unwrap_err()
            .code(),
        "NO_UNTRACKED_SKILL"
    );

    // The clean-resolve receipt obeys the same authority: its ONE confirming read finds the name
    // gone, so the stale row's disclosure is withdrawn — never printed beside an adopt.
    untracked_skill(&rig.home.0, BARE, b"# deploy\n");
    match bare_plan(&rig, &plane, &empty, BARE, true).unwrap() {
        ops::BareAddPlan::Adopt { published, .. } => assert!(published.is_none()),
        other => panic!("a local copy adopts in place: {other:?}"),
    }
}
