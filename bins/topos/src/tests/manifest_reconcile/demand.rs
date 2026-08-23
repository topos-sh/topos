//! A demand that still stands is removed by editing the demand — never by deleting the copy. The
//! refusal toward the file that holds the row, the exact drop a gone folder earns, and the classic
//! delete that spares an adopted source dir.

use std::sync::{Arc, Mutex};

use crate::ops;
use crate::sessions::Session;

use super::rig::*;

// =================================================================================================
// A demand that still stands is removed by editing the demand — never by deleting the copy
// =================================================================================================

/// DATA LOSS. `topos remove <name> --yes` from a folder no project manifest covers used to take
/// the classic permanent-delete arm even when THIS MACHINE'S OWN file still carried the row: the
/// adopted source folder was deleted, the row was left standing, and every later `update -g` failed
/// PATH_MISSING forever. The path form already refused toward `-g`; the name form must too.
#[test]
fn a_name_whose_machine_row_still_stands_refuses_toward_the_machine_file() {
    let rig = Rig::new("standing-row-name");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());

    // A local folder adopted in place, demanded by the MACHINE-WIDE file.
    let src = rig.work.0.join("weather");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# weather\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add(&ctx, &src).unwrap();
    let canonical = src.canonicalize().unwrap();
    rig.write_global(&format!(
        "[skills]\n{} = \"{}\"\n",
        canonical.file_name().unwrap().to_str().unwrap(),
        canonical.display()
    ));

    let named_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let connectors = ops::RemoveConnectors {
        session: &named_connect,
    };

    // The cwd carries no manifest of its own, so nothing here can drop that row.
    let err = ops::remove(&ctx, &connectors, &["weather".to_owned()], &[], None, true)
        .expect_err("a standing machine row is removed by editing the demand");
    let detail = err.detail();
    assert!(detail.contains("topos remove -g weather"), "{detail}");

    // NOTHING was destroyed and nothing was half-done: the folder and the row both stand.
    assert!(
        src.join("SKILL.md").exists(),
        "the adopted source folder survives"
    );
    assert!(
        global_text(&rig).contains("weather"),
        "the row survives: {}",
        global_text(&rig)
    );

    // And the command the refusal offers actually works — the row goes, through the machine arm.
    ops::remove_global(
        &ctx,
        &named_connect,
        &["weather".to_owned()],
        None,
        true,
        &Default::default(),
    )
    .expect("the offered command is runnable");
    assert_eq!(global_text(&rig), "[skills]\n");
}

/// PATH_MISSING names a command that WORKS. The row lives in the machine-wide file, so the drop
/// takes `-g` — the warning used to spell it without, which refused, leaving the only named way
/// out of a warning that repeats on every sweep as a command that could not clear it. Both halves
/// are asserted here: the exact spelling, and that running it clears the row (and the warning).
#[test]
fn path_missing_names_the_scope_exact_drop_and_it_clears_the_row() {
    let rig = Rig::new("path-missing-scope");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());

    let src = rig.work.0.join("weather");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# weather\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let canonical = src.canonicalize().unwrap();
    rig.write_global(&format!(
        "[skills]\n{} = \"{}\"\n",
        canonical.file_name().unwrap().to_str().unwrap(),
        canonical.display()
    ));

    // The folder goes; the row stays. That is the state PATH_MISSING exists to report.
    std::fs::remove_dir_all(&src).unwrap();
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let warning = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .find(|w| w.starts_with("PATH_MISSING"))
        .unwrap_or_else(|| panic!("the missing folder is reported: {:?}", out.warnings));
    // COMPLETE, not merely correct: the offered command carries `--yes`, so running it finishes
    // the job instead of describing the drop back at the reader.
    let offered = format!("'topos remove -g {} --yes'", canonical.display());
    assert!(
        warning.contains(&offered),
        "the drop is spelled for the file that HOLDS the row, and it completes: {warning}"
    );
    // THE SCOPE IN THE PERSON'S WORDS. `person` is the resolver's internal name for the
    // machine-wide scope and it shipped verbatim to anyone who deleted a folder a row still asks
    // for. The machine lane keeps the code; the TTY prints the sentence without it.
    assert!(
        warning.contains("asks for this folder machine-wide") && !warning.contains("person"),
        "the scope is named in user vocabulary: {warning}"
    );
    let warn_tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(
        warn_tty.contains("asks for this folder machine-wide, and the folder is gone")
            && !warn_tty.contains("PATH_MISSING"),
        "the TTY reads as English: {warn_tty}"
    );
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("PATH_MISSING")),
        "the machine lane keeps the code: {:?}",
        out.warnings
    );
    // THE SUMMARY AND THE STATUS AGREE. The fault is about a BUNDLE, so the bundle is counted:
    // the run used to push a line and nothing else, which printed "Checked 0 skills" beside a
    // non-zero exit — a receipt reporting nothing wrong under a status saying otherwise.
    assert_eq!(
        out.failed_bundles.len(),
        1,
        "the bundle that could not be carried forward is counted: {:?}",
        out.failed_bundles
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
        !tty.contains("Checked 0 skills"),
        "a run with a failed bundle never summarises as having checked none: {tty}"
    );
    // …and an agent reading `--json` gets the SAME way out the prose line spells, as argv.
    let action = out
        .fault_actions
        .iter()
        .find(|a| a.code.as_str() == "REMOVE_MISSING_ROW")
        .unwrap_or_else(|| panic!("the fault carries its remedy: {:?}", out.fault_actions));
    assert_eq!(
        action.argv,
        vec![
            "topos".to_owned(),
            "remove".to_owned(),
            "-g".to_owned(),
            canonical.display().to_string(),
            "--yes".to_owned(),
            "--json".to_owned(),
        ],
        "the machine channel spells the same scope — and the same completing command — the prose \
         did"
    );

    // The offered command runs, and the next sweep has nothing left to warn about.
    let named_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    ops::remove_global(
        &ctx,
        &named_connect,
        &[canonical.display().to_string()],
        None,
        true,
        &Default::default(),
    )
    .expect("the offered command is runnable");
    assert_eq!(global_text(&rig), "[skills]\n");
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        !crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("PATH_MISSING")),
        "the warning is gone with the row: {:?}",
        out.warnings
    );
}

/// THE GONE-FOLDER CHAIN, END TO END: every surface a person meets after deleting a folder a row
/// still asks for names the SAME fact and the SAME command, and none of them promises anything
/// that cannot happen. `list` used to offer `topos update -g` for a row that can never apply; the
/// remove hedged with the loss guard's "files could not be read" (through a doubled possessive)
/// about files that are not there; and the receipt printed `Undo: topos add <gone path>` — an
/// inverse that must fail.
#[test]
fn a_gone_folders_row_states_the_fact_and_withholds_the_undo() {
    let rig = Rig::new("gone-folder-chain");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());

    let src = rig.work.0.join("solo");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# solo\n").unwrap();
    let canonical = src.canonicalize().unwrap();
    let raw = canonical.display().to_string();
    rig.write_global(&format!("[skills]\nsolo = \"{raw}\"\n"));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    std::fs::remove_dir_all(&src).unwrap();

    // THE LISTING. One note, and the only command in it is the one that changes something.
    let listing = ops::list_with(
        &ctx,
        &ops::ListRequest {
            view: ops::ScopeView::Machine,
            ..Default::default()
        },
        None,
        None,
        ops::RowPage::unlimited(),
    )
    .unwrap();
    let text = crate::render::list_tty(&listing);
    assert!(
        text.contains(&format!(
            "(its folder is gone — run 'topos remove -g {raw} --yes' to drop the line)"
        )),
        "{text}"
    );
    assert!(
        !text.contains("topos update -g` applies it"),
        "a row that can never apply is never offered the update: {text}"
    );

    // THE REMOVE. Bare — nothing can be lost, so nothing gates — with the real reason, and the
    // possessive spelled once.
    let out = ops::remove_global(
        &ctx,
        &|_s: &Session| ops::SessionTransports {
            plane: Box::new(plane.clone()),
            directory: Box::new(dir.clone()),
            contribute: Box::new(NoContribute),
            governance: Box::new(NoGovernance),
        },
        std::slice::from_ref(&raw),
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    let data = match out {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("nothing can be lost, so nothing describes first: {other:?}"),
    };
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains(
            "its folder is gone, so there are no files to check — dropping the line changes \
             nothing else"
        ),
        "{note}"
    );
    assert!(!note.contains("could not be read"), "{note}");
    assert!(!note.contains("''s"), "one apostrophe, one s: {note}");

    // THE RECEIPT. No undo: `topos add <gone path>` cannot restore anything.
    assert!(
        data.undo.is_empty(),
        "an inverse that must fail is withheld: {:?}",
        data.undo
    );
    assert_eq!(global_text(&rig), "[skills]\n");
}

/// TWO ROWS, TWO FOLDERS, ONE NAME — and TWO failures. A gone folder's bundle is counted under
/// the MANIFEST LINE that asks for it (unique per scope by construction), never under the display
/// name two folders can share: the name-keyed tally filed both rows as one bundle, so a sweep that
/// could carry neither forward summarised "1 failed" while warning about two.
#[test]
fn two_same_named_local_rows_whose_folders_are_gone_are_two_failures() {
    let rig = Rig::new("path-missing-twins");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());

    let mut refs = Vec::new();
    for parent in ["one", "two"] {
        let src = rig.work.0.join(parent).join("linear");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("SKILL.md"), format!("# {parent}\n")).unwrap();
        refs.push(src.canonicalize().unwrap());
    }
    rig.write_global(&format!(
        "[skills]\nlinear = \"{}\"\nlinear-2 = \"{}\"\n",
        refs[0].display(),
        refs[1].display()
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    for r in &refs {
        std::fs::remove_dir_all(r).unwrap();
    }
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);

    let warned: Vec<String> = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .filter(|w| w.starts_with("PATH_MISSING"))
        .collect();
    assert_eq!(warned.len(), 2, "both rows are reported: {warned:?}");
    assert_eq!(
        out.failed_bundles.len(),
        2,
        "the summary counts what the warnings named: {:?}",
        out.failed_bundles
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
    assert!(tty.contains("2 failed"), "{tty}");
}

/// THREE SURFACES, ONE MACHINE. `list` prints `(draft)` and `status` counts `1 draft ahead` for a
/// bundle the person is still editing — and `update` used to answer "all up to date" about the
/// very same bundle, because delivery owed it nothing and the row said only that. The row now
/// carries the draft fact, so the receipt says both true things and its summary matches what the
/// other two surfaces report.
#[test]
fn an_update_never_calls_a_drafted_bundle_all_up_to_date() {
    let rig = Rig::new("draft-agreement");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());

    // An adopted-in-place local folder: the dir IS the placement, so editing it IS the draft.
    let src = rig.work.0.join("notes");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# notes\nbase\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add(&ctx, &src).unwrap();
    let canonical = src.canonicalize().unwrap();
    rig.write_global(&format!(
        "[skills]\n{} = \"{}\"\n",
        canonical.file_name().unwrap().to_str().unwrap(),
        canonical.display()
    ));

    // CLEAN: nothing owed, nothing unshared — the compact sentence is true and stays.
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(tty.contains("all up to date"), "{tty}");
    assert!(
        out.data.skills.iter().all(|s| !s.draft),
        "{:?}",
        out.data.skills
    );

    // DRAFTED: the person edits their own copy. `list` calls this a draft; so must `update`.
    std::fs::write(
        src.join("SKILL.md"),
        b"# notes\nbase\nmy own unshared line\n",
    )
    .unwrap();
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill.contains("notes"))
        .unwrap_or_else(|| panic!("the row: {:?}", out.data.skills));
    assert!(row.draft, "the row carries the draft fact: {row:?}");
    assert_eq!(row.action, topos_types::results::PullAction::UpToDate);
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
        !tty.contains("all up to date"),
        "the sentence that contradicted `list`: {tty}"
    );
    assert!(
        tty.contains("up to date — your edits are not shared yet (topos publish"),
        "{tty}"
    );
    assert!(tty.ends_with("Checked 1 skill: 1 draft ahead."), "{tty}");
    // …AND THE MACHINE LANE OFFERS THE SAME ACT. The row told an agent, in the payload, that its
    // edits are unshared and then handed it nothing to do about that; the TTY had been printing
    // `topos publish <name>` all along.
    let actions = crate::render::draft_next_actions(&out.data);
    assert_eq!(actions.len(), 1, "{actions:?}");
    assert_eq!(
        actions[0].argv,
        vec!["topos".to_owned(), "publish".to_owned(), row.skill.clone(),],
        "the offered argv is the command the row prints"
    );
}

/// THE ARM STILL EXISTS — and it takes the RECORD, not the person's folder. A record no visible
/// scope demands is still the classic ladder's business (the guard is about a STANDING demand, not
/// about local records in general), but `add ./weather` adopted that folder IN PLACE: topos wrote
/// nothing into it and never created it. Deleting it here would make this the one arm in the CLI
/// where a `remove` destroys the user's own working directory — while the manifest arm, the retire
/// sweep and the `--force` rebuild all spare it. One rule now: topos deletes only what topos made.
#[test]
fn a_record_no_row_demands_takes_the_classic_delete_and_spares_the_adopted_folder() {
    let rig = Rig::new("orphan-name");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());

    let src = rig.work.0.join("weather");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# weather\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();
    let sid = crate::id::SkillId::parse(&added.skill_id.expect("the adopt minted a record"))
        .expect("a minted id parses");
    // NO row anywhere demands it.
    rig.write_global("");

    let named_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let connectors = ops::RemoveConnectors {
        session: &named_connect,
    };
    let outcome = ops::remove(&ctx, &connectors, &["weather".to_owned()], &[], None, true)
        .expect("an undemanded record is the classic ladder's business");
    // The RECORD is what ends.
    assert!(
        !rig.layout().skill_dir(&sid).exists(),
        "the store record is gone"
    );
    // The FOLDER is theirs.
    assert!(
        src.is_dir() && src.join("SKILL.md").is_file(),
        "the adopted source folder survives the removal"
    );
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("--yes applies");
    };
    assert!(
        data.items[0].bytes_kept && data.items[0].dest_dirs.is_empty(),
        "nothing is claimed deleted: {data:?}"
    );
    assert_eq!(
        data.items[0].kept_dirs,
        vec![src.display().to_string()],
        "the receipt names the folder it left alone"
    );
    let tty = crate::render::remove_applied_tty(&data);
    assert!(
        tty.contains("stays where it is") && !tty.contains("PERMANENTLY"),
        "a receipt that kept every byte never says the word: {tty}"
    );
}

/// **A PARENT CHECKOUT'S ROW IS A STANDING DEMAND TOO.** The classic arm resolves a name in the
/// store you STAND in — nearest checkout first, then the machine — which is what lets a bare
/// `remove` reach a project record at all. But its demand guard only ever read the MACHINE's rows,
/// so from a checkout nested inside another one the ladder found the OUTER checkout's record,
/// found no machine row claiming the name, and called it an ended workspace delivery: a gated
/// permanent delete of a record whose row is sitting in the parent's `topos.toml`, described to
/// the person as a leftover. The guard asks the scope that OWNS the record now, and refuses toward
/// the file that carries the row.
#[test]
fn a_parent_checkouts_row_refuses_the_nested_classic_delete() {
    let rig = Rig::new("zq-nested");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global("");

    // The OUTER checkout adopts `quaggamap` and keeps the row.
    let outer = project("zq-nested-outer", "");
    let src = outer.0.join("skills/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    let outer_ctx = rig.ctx_at(Some(&outer.0));
    scoped_path_add(&outer_ctx, &src, false).unwrap();
    let outer_manifest = outer.0.join(crate::manifest::MANIFEST_FILE);
    assert!(
        std::fs::read_to_string(&outer_manifest)
            .unwrap()
            .contains("quaggamap"),
        "the outer file carries the row"
    );

    // A checkout NESTED inside it, whose own file demands nothing.
    let inner = outer.0.join("packages/inner");
    std::fs::create_dir_all(inner.join(".git")).unwrap();
    std::fs::write(inner.join(crate::manifest::MANIFEST_FILE), "").unwrap();
    let ctx = rig.ctx_at(Some(&inner));

    // The resolver universe needs a directory that ANSWERS `me`.
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

    // (a) STANDING: the refusal names the file that actually carries the row.
    let err = ops::remove(
        &ctx,
        &connectors,
        &["quaggamap".to_owned()],
        &[],
        None,
        true,
    )
    .expect_err("a row in the parent checkout is a standing demand");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("quaggamap") && msg.contains("topos.toml"),
        "the refusal names the file that carries the row: {msg}"
    );
    assert!(
        std::fs::read_to_string(src.join("SKILL.md")).is_ok(),
        "nothing of the person's was deleted"
    );

    // (b) GENUINELY ORPHANED — no row in ANY scope's file — still takes the classic arm.
    std::fs::write(&outer_manifest, "").unwrap();
    let outcome = ops::remove(
        &ctx,
        &connectors,
        &["quaggamap".to_owned()],
        &[],
        None,
        true,
    )
    .expect("with no row anywhere the classic ladder owns it");
    let ops::RemoveOutcome::Applied(data) = outcome else {
        panic!("--yes applies");
    };
    // Its dir was adopted in place, so the folder stays and the record retires.
    assert_eq!(
        data.items[0].kind,
        topos_types::results::RemoveKind::TrackedLocalRetired,
        "{data:?}"
    );
    assert!(src.is_dir(), "the adopted folder is still the person's");
}
