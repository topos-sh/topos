//! What one sweep does beyond delivering: the clean (snapshot-first, never while frozen), the
//! `--force` rebuild that absorbs before it re-projects, the session-level facts (the ended freeze
//! and the quiet hook's staleness line), the targeted narrowing, the per-scope forge stores and
//! their cleans, the complete-state applied report, and the forge add that records the demand
//! FIRST so a member failure still leaves a convergent state.

use std::sync::{Arc, Mutex};

use topos_types::requests::{WireChannelEntry, WireChannelSkill};
use topos_types::results::{ExchangeFault, PullAction};

use crate::fs_seam::FsOps;
use crate::plane::{DeliverySkill, DeliverySnapshot};
use crate::sessions::{self, SESSION_ENDED};
use crate::{ops, sync_status};

use super::rig::*;

// =================================================================================================
// Cleaning.
// =================================================================================================

#[test]
fn a_dropped_feed_row_uninstalls_clean_copies_and_keeps_edited_ones() {
    let rig = Rig::new("feeddrop");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    let placed = rig.skills().join("deploy");
    assert!(placed.join("SKILL.md").exists());

    // A CLEAN copy: dropping the feed row uninstalls it now, and the receipt row speaks in
    // destinations — the removed dir, the qualified name — never an agent.
    rig.write_global("[bundles]\n");
    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert_eq!(
        row.display.as_deref(),
        Some(&format!("@{WS_NAME}/deploy")[..])
    );
    assert_eq!(row.destinations, vec![rig.pretty(&placed)]);
    assert!(row.kept.is_empty(), "{:?}", row.kept);
    assert!(!placed.exists(), "the clean person-scope copy is retired");
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "every sidecar byte stays"
    );

    // An EDITED copy: re-adopt, edit, drop again — the edit is the person's own work, so the
    // copy STAYS IN PLACE (disclosed on the row), with a snapshot in the store behind it.
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    assert!(placed.join("SKILL.md").exists());
    let before = store_versions(&rig.layout(), "s_deploy");
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    rig.write_global("[bundles]\n");
    let out = sweep(&ctx, &plane, &dir);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert!(row.destinations.is_empty(), "{:?}", row.destinations);
    assert_eq!(row.kept, vec![rig.pretty(&placed)]);
    assert!(placed.join("SKILL.md").exists(), "the edited copy stays");
    assert_eq!(
        std::fs::read(placed.join("SKILL.md")).unwrap(),
        b"# my edit\n",
        "byte-for-byte the person's edit"
    );
    assert!(
        store_versions(&rig.layout(), "s_deploy") > before,
        "the kept edit is snapshotted into the store too"
    );

    // Idempotent: the sweep after has nothing left to retire — the kept copy is the person's own
    // file now, not something to re-announce every session start.
    let out2 = sweep(&ctx, &plane, &dir);
    assert!(
        !out2
            .data
            .skills
            .iter()
            .any(|s| matches!(s.action, PullAction::Removed | PullAction::Withdrawn)),
        "{:?}",
        out2.data.skills
    );
    assert!(placed.join("SKILL.md").exists());
}

#[test]
fn a_new_off_row_cleans_its_bundles_placements_and_keeps_the_bytes() {
    let rig = Rig::new("offclean");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# noisy\n");
    let plane = FakePlane::new(log).with_version("s_noisy", &v);
    plane.serves(vec![delivered("s_noisy", "noisy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_noisy", "noisy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.skills().join("noisy");
    assert!(placed.exists());

    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/noisy\" = \"off\"\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    assert!(!placed.exists(), "the switch retires the copy");
    let sid = crate::id::SkillId::parse("s_noisy").unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "an off switch keeps the bytes: {:?}",
        out.warnings
    );
}

#[test]
fn a_dropped_project_row_cleans_inside_the_checkout() {
    let rig = Rig::new("projdrop");
    rig.seed_session();
    let proj = project(
        "proj-drop",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.exists());

    std::fs::write(proj.0.join(crate::manifest::MANIFEST_FILE), "[bundles]\n").unwrap();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !placed.exists(),
        "the dropped row retires the in-checkout copy: {:?}",
        out.warnings
    );
    // The project's own store still holds the bundle's bytes.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(playout.skill_dir(&sid).exists());
}

#[test]
fn an_offline_sweep_freezes_and_never_cleans() {
    let rig = Rig::new("offline");
    rig.seed_session();
    let proj = project(
        "proj-off",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &v)],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_deploy".into(),
                name: "deploy".into(),
            }],
        }],
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep(&ctx, &plane, &dir);
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.exists());

    // The whole plane is down (the session-start hook's world): NOTHING may be deleted.
    plane.serve_unreachable();
    dir.set_unavailable(true);
    let out = sweep(&ctx, &plane, &dir);
    assert!(placed.exists(), "an offline sweep freezes, never cleans");
    assert!(
        !out.data
            .skills
            .iter()
            .any(|s| s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );

    // Back online with the index still failing: the unknowable member set stays frozen.
    plane.serve(empty_snapshot());
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        placed.exists(),
        "a transient index failure freezes the members: {:?}",
        out.warnings
    );

    // Fully back: the member is still delivered.
    dir.set_unavailable(false);
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(placed.exists());
}

// =================================================================================================
// `--force` (the rebuild repair).
// =================================================================================================

#[test]
fn rebuild_absorbs_the_edit_before_it_re_projects() {
    let rig = Rig::new("rebuild");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.skills().join("deploy");
    assert!(placed.join("SKILL.md").exists());
    let before = store_versions(&rig.layout(), "s_deploy");

    // A local edit, plus a stray file the served version never had.
    std::fs::write(placed.join("SKILL.md"), b"# hand-edited\n").unwrap();
    std::fs::write(placed.join("stray.md"), b"junk\n").unwrap();

    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            rebuild: true,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // ABSORBED first: the edit is in the store, so nothing was lost to the repair.
    assert!(
        store_versions(&rig.layout(), "s_deploy") > before,
        "the edit was snapshotted before the dir was dropped"
    );
    // Then RE-PROJECTED, pristine: the served bytes, and no stray file.
    assert_eq!(
        snapshot_dir(&placed),
        vec![("SKILL.md".to_owned(), b"# deploy\n".to_vec())],
        "the copy is re-materialized from the store"
    );
}

/// The BUILT-IN survives a `--force`, in the same invocation.
///
/// Its force-sync from the binary IS its rebuild, and it has already run by the time the repair
/// starts. Re-projecting it the ordinary way — drop the dirs, let the sweep write them back —
/// therefore deletes a folder nothing later in the run puts back: the person asks for a repair and
/// their agents lose the one skill that teaches them what topos is, silently, until the next bare
/// sweep happens to heal it.
#[test]
fn force_leaves_the_built_in_placed() {
    let rig = Rig::new("rebuild-builtin");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    // The same force-sync every real invocation runs before the reconcile.
    ops::ensure_builtin(&ctx).unwrap();
    let builtin: Vec<std::path::PathBuf> = ops::builtin_placement_dirs(&ctx)
        .unwrap()
        .iter()
        .map(std::path::PathBuf::from)
        .collect();
    assert!(!builtin.is_empty(), "the built-in is placed to begin with");
    for dir in &builtin {
        assert!(dir.join("SKILL.md").exists(), "{}", dir.display());
    }

    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            rebuild: true,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    for dir in &builtin {
        assert!(
            dir.join("SKILL.md").exists(),
            "the built-in is still placed after --force: {}",
            dir.display()
        );
    }
}

/// A rebuild of a bundle whose merge is still UNDECIDED leaves its folders exactly as they are.
///
/// The ordinary repair works by dropping every placement dir and letting the sweep re-project the
/// bundle pristine — but a blocked bundle has no pristine version to project: the sweep stops at
/// the block and writes no placement, so dropping the dirs would empty every agent folder and
/// leave it empty until the merge is settled. Nothing is lost either way (the bytes are
/// snapshotted), but "your agents have this skill" would stop being true because of a repair. So
/// the rebuild stands aside and names the two exits — both of which rewrite every managed
/// placement on their way out, which is the repair the person came for.
#[test]
fn rebuild_leaves_a_blocked_bundle_alone_and_names_both_exits() {
    let rig = Rig::new("rebuild-blocked");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# deploy\n");
    let v2 = one_file(b"# deploy, theirs\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.skills().join("deploy");
    assert!(placed.join("SKILL.md").exists());

    // My own edit to the same file the team is about to change → a conflict on the next sweep.
    std::fs::write(placed.join("SKILL.md"), b"# deploy, mine\n").unwrap();
    plane.serves(vec![DeliverySkill {
        version_id: v2.id,
        generation: 2,
        bundle_digest: v2.digest,
        ..delivered("s_deploy", "deploy", &v2)
    }]);
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        out.data.skills.iter().map(|s| s.action).collect::<Vec<_>>(),
        vec![PullAction::Conflicted]
    );
    let sp = rig
        .layout()
        .published(&crate::id::SkillId::parse("s_deploy").unwrap());
    assert!(sp.conflict.exists(), "the block is recorded");
    let mine = snapshot_dir(&placed);
    assert_eq!(
        mine,
        vec![("SKILL.md".to_owned(), b"# deploy, mine\n".to_vec())]
    );

    // THE POINT: `--force` touches nothing here, and says why in the scope's own spelling.
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            rebuild: true,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert_eq!(
        snapshot_dir(&placed),
        mine,
        "a rebuild must not empty the folders of a bundle it cannot re-project"
    );
    assert!(sp.conflict.exists(), "and the block still stands");
    // A bundle waiting on a person is NOT a failure: nothing broke, no retry helps, and the run's
    // exit status must not say otherwise.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // No internal code, and each way out on its own line, runnable exactly as printed.
    assert_eq!(out.decisions.len(), 1, "{:?}", out.decisions);
    assert_eq!(
        out.decisions[0].name, "deploy",
        "the bundle leads its own row"
    );
    assert_eq!(
        out.decisions[0].line,
        "a merge is still waiting on your answer, so topos left its folders as they are."
    );
    assert_eq!(
        out.decisions[0].detail,
        vec![
            "settle it first, then rebuild:".to_owned(),
            "  topos update -g deploy --keep-mine".to_owned(),
            "  topos update -g deploy --reset".to_owned(),
        ]
    );
    // The receipt renders it as a row of this run, padded with the rest, and counts it as an
    // answer owed — never as something that failed.
    let tty = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
    );
    assert!(
        tty.contains(
            "deploy   a merge is still waiting on your answer, so topos left its folders as they \
             are.\n    settle it first, then rebuild:\n      topos update -g deploy \
             --keep-mine\n      topos update -g deploy --reset"
        ),
        "{tty}"
    );
    assert!(tty.contains("waiting on you"), "{tty}");
    assert!(!tty.contains("failed"), "{tty}");
    // ONE pair of exits on the machine surface. The bundle is re-disclosed as a conflicted row in
    // this same run, and that row's typed pair IS the decision — a second, differently-spelled
    // pair from the rebuild itself handed an agent four actions for two choices.
    let actions: Vec<(String, String)> = crate::render::conflict_next_actions(&out.data)
        .into_iter()
        .chain(crate::render::decision_next_actions(&out.decisions))
        .map(|a| (a.code.as_str().to_owned(), a.argv.join(" ")))
        .collect();
    assert_eq!(
        actions,
        vec![
            (
                "RESOLVE_DIVERGED_DRAFT".to_owned(),
                "topos update -g deploy --keep-mine --json".to_owned()
            ),
            (
                "RESOLVE_DIVERGED_DRAFT".to_owned(),
                "topos update -g deploy --reset --json".to_owned()
            ),
        ]
    );
    // And the HEADER says what the run did: it checked. Nothing moved here — the whole point of
    // the assertions above is that the folders were left as they are — so `updated machine-wide`
    // was a claim the rows under it contradicted.
    assert!(tty.starts_with("checked machine-wide\n"), "{tty}");
    // The row in the SAME receipt still names the untouched folder — the rebuild changed nothing
    // about what the conflict row can promise.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.action == PullAction::Conflicted)
        .expect("the block is re-disclosed");
    assert_eq!(
        row.merge.as_ref().map(|m| m.placements.len()),
        Some(1),
        "{:?}",
        row.merge
    );
}

// =================================================================================================
// The session-level facts: the ended freeze, and the quiet hook's staleness line.
// =================================================================================================

#[test]
fn an_ended_session_freezes_and_prints_once() {
    let rig = Rig::new("ended");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serve_not_found();
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("SESSION_ENDED")),
        "{:?}",
        out.warnings
    );
    assert_eq!(out.access_gone, vec![WS_NAME.to_owned()]);
    let all = sessions::read_sessions(&rig.fs, &rig.layout()).unwrap();
    assert_eq!(all.sessions[0].status, SESSION_ENDED);
    // The second run skips the ended session — the line printed once.
    let out2 = sweep(&ctx, &plane, &dir);
    assert!(out2.warnings.is_empty(), "{:?}", out2.warnings);
}

#[test]
fn an_unreachable_and_stale_workspace_warns_by_name() {
    let rig = Rig::new("stalewarn");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // One good sweep stamps the freshness row — under the workspace ID, with the served window.
    sweep(&ctx, &plane, &dir);
    let status = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    assert_eq!(status.workspaces[WS].last_delivery_at, Some(rig_now(&rig)));
    assert_eq!(status.workspaces[WS].staleness_window_ms, 604_800_000);
    assert!(
        !status.workspaces.contains_key(WS_NAME),
        "the freshness cache is keyed by id, never by name — the warning must look it up that way"
    );

    assert_eq!(
        recorded_fault(&rig, WS),
        None,
        "a landed exchange records no fault"
    );

    // Now the server is gone.
    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert_eq!(out.unreachable[0].workspace_id, WS);
    assert_eq!(out.unreachable[0].workspace_name, WS_NAME);
    // The transient warning below is only half of it: the fault OUTLIVES this run, under the same
    // id the freshness facts sit under, so a later read can still say the exchange did not land.
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Unreachable));
    assert_eq!(
        sync_status::read(&rig.fs, &rig.layout())
            .unwrap()
            .workspaces[WS]
            .last_delivery_at,
        Some(rig_now(&rig)),
        "the fault lands BESIDE the freshness facts — it never overwrites the entry"
    );

    // Past the recorded 7-day window: the ONE line, naming the workspace a person knows.
    let stale_now = rig_now(&rig) + 8 * DAY_MS;
    let lines = ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out);
    assert_eq!(
        lines,
        vec![format!(
            "topos: {WS_NAME} last synced 8d ago — the server could not be reached."
        )]
    );

    // INSIDE the window: silence — a transient blip must not spam every session start.
    let fresh_now = rig_now(&rig) + 3_600_000;
    assert!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), fresh_now, &out).is_empty(),
        "a fresh miss stays quiet"
    );
}

#[test]
fn an_answering_server_never_gets_blamed_on_the_network() {
    let rig = Rig::new("stalekind");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // One good sweep stamps the freshness row the staleness check reads.
    sweep(&ctx, &plane, &dir);
    let stale_now = rig_now(&rig) + 8 * DAY_MS;

    // The plane ANSWERED with a failure status. The nudge is just as true — but the network is fine.
    plane.serve_unavailable();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("PLANE_UNAVAILABLE")),
        "{:?}",
        out.warnings
    );
    let unavailable_line =
        format!("topos: {WS_NAME} last synced 8d ago — the server did not answer successfully.");
    assert_eq!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out),
        vec![unavailable_line.clone()]
    );
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Unavailable));

    // The OTHER half of the same variant: the answer got cut off part-way.
    plane.serve_truncated();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out),
        vec![unavailable_line],
        "a truncated body is the same variant and reads the same"
    );
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Unavailable));

    // The plane ANSWERED unreadably. Pointing a person at their network here sends them the wrong
    // way entirely — the signal is about the bytes.
    plane.serve_malformed();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("WIRE_INVALID")),
        "the MALFORMED arm ran, not the transport one: {:?}",
        out.warnings
    );
    assert_eq!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), stale_now, &out),
        vec![format!(
            "topos: {WS_NAME} last synced 8d ago — the server's answer could not be read."
        )]
    );
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Malformed));

    // All three still stay quiet inside the window — the reason never overrides the threshold.
    let fresh_now = rig_now(&rig) + 3_600_000;
    assert!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), fresh_now, &out).is_empty(),
        "a fresh miss stays quiet whatever the reason"
    );
}

#[test]
fn a_never_delivered_workspace_stays_silent_while_unreachable() {
    let rig = Rig::new("stalenever");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serve_unreachable();
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Unreachable from the very first sweep: nothing was ever delivered, so there is no freshness
    // row — and nothing to be stale FROM.
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), i64::MAX, &out).is_empty(),
        "no record, no warning"
    );
    // And no row is BORN to carry the fault either — the same philosophy: there is nothing here a
    // later read could be answering staler than.
    assert!(
        !sync_status::read(&rig.fs, &rig.layout())
            .unwrap()
            .workspaces
            .contains_key(WS),
        "a never-delivered workspace gains no freshness row from a failure"
    );
}

/// A landed exchange CLEARS a recorded fault: the successful write replaces a workspace's whole
/// entry, so the fault cannot outlive the run that fixed it (a stale one would have `log` warning
/// forever about a server that came back).
#[test]
fn a_landed_exchange_clears_the_recorded_fault() {
    let rig = Rig::new("faultclear");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    sweep(&ctx, &plane, &dir);
    plane.serve_malformed();
    sweep(&ctx, &plane, &dir);
    assert_eq!(recorded_fault(&rig, WS), Some(ExchangeFault::Malformed));

    // The server comes back.
    plane.serve(empty_snapshot());
    sweep(&ctx, &plane, &dir);
    assert_eq!(recorded_fault(&rig, WS), None);
}

#[test]
fn a_zero_staleness_window_never_warns() {
    let rig = Rig::new("stalezero");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serve(DeliverySnapshot {
        staleness_window_ms: 0,
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    assert_eq!(
        sync_status::read(&rig.fs, &rig.layout())
            .unwrap()
            .workspaces[WS]
            .staleness_window_ms,
        0
    );

    plane.serve_unreachable();
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(out.unreachable.len(), 1);
    assert!(
        ops::quiet_hook_lines(&rig.fs, &rig.layout(), i64::MAX, &out).is_empty(),
        "a zero window opts the workspace out of the warning entirely"
    );
}

// =================================================================================================
// Targeted updates.
// =================================================================================================

#[test]
fn a_targeted_update_narrows_the_sweep_and_names_a_miss() {
    let rig = Rig::new("targeted");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let deploy = one_file(b"# deploy\n");
    let other = one_file(b"# other\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &deploy)
        .with_version("s_other", &other);
    plane.serves(vec![
        delivered("s_deploy", "deploy", &deploy),
        delivered("s_other", "other", &other),
    ]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &deploy),
            catalog_entry("s_other", "other", &other),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["deploy".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(rig.skills().join("deploy/SKILL.md").exists());
    assert!(
        !rig.skills().join("other").exists(),
        "a targeted update touches only what was named"
    );

    // A target nothing answers refuses typed, naming the way back.
    let refused = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["nonesuch".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    );
    let Err(err) = refused else {
        panic!("an unmatched target must refuse");
    };
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err}");
    // Standing outside any project, the searched scope was the machine — and the line says so.
    assert!(
        err.to_string()
            .contains("'nonesuch' is not in your machine-wide set"),
        "{err}"
    );
}

// =================================================================================================
// Per-scope forge stores, the absent-member clean, and the dropped-row clean.
// =================================================================================================

#[test]
fn a_repo_row_converges_in_place_inside_the_forge_interval() {
    let rig = Rig::new("repo-postgate");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    assert!(rig.home.0.join(".claude/skills/alpha/SKILL.md").exists());
    // An `add` is not a lane round — it schedules nothing. The first sweep after it therefore
    // still checks; this one is what starts the clock.
    quiet_sweep(&ctx, &plane, &dir, &git);

    // Tracked now: inside its interval the silent sweep converges in place without dialing, and
    // the hand-run update answers the same way because nothing upstream moved.
    let dialed = git.probes() + git.fetches();
    let quiet = quiet_sweep(&ctx, &plane, &dir, &git);
    assert_eq!(
        git.probes() + git.fetches(),
        dialed,
        "inside the interval, nothing is asked"
    );
    assert!(
        quiet
            .data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?}",
        quiet.data.skills
    );
    let out = update_now(&ctx, &plane, &dir, &git);
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "alpha" && s.action == PullAction::UpToDate),
        "{:?}",
        out.data.skills
    );
}

#[test]
fn project_checkouts_keep_their_own_forge_stores() {
    // TWO checkouts of one repo row: each gets its OWN tracked import + placement, and a move
    // taken in one (a pin move / a new commit) never reaches into the other's placements.
    let rig = Rig::new("repo-two-proj");
    let proj_a = project("proj-fa", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let proj_b = project("proj-fb", "[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));

    // Gate each checkout separately — trust is per store.
    let ctx_a = rig.ctx_at(Some(&proj_a.0));
    match ops::add_reference(
        &ctx_a,
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
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    let a_copy = proj_a.0.join(".claude/skills/alpha/SKILL.md");
    assert!(a_copy.exists(), "checkout A holds its own placement");
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj_a.0).is_some(),
        "checkout A has its own store"
    );

    let ctx_b = rig.ctx_at(Some(&proj_b.0));
    match ops::add_reference(
        &ctx_b,
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
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    let b_copy = proj_b.0.join(".claude/skills/alpha/SKILL.md");
    assert!(b_copy.exists(), "checkout B holds its own placement");

    // The source moves; ONLY checkout B updates. A's placement must be untouched — the refresh
    // operates within B's store alone (never a stash-and-delete of A's copy).
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    let out = ops::manifest_update(
        &ctx_b,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&b_copy).unwrap(),
        "# alpha v2\n",
        "{:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read_to_string(&a_copy).unwrap(),
        "# alpha v1\n",
        "checkout A's placement never moves from a run in checkout B"
    );
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &proj_a.0)
            .map(|l| rig.fs.read_dir(&l.skills_dir()).unwrap().len())
            .unwrap_or(0)
            > 0,
        "checkout A's store is intact"
    );
}

#[test]
fn a_member_gone_from_the_archive_is_cleaned_snapshot_first() {
    let rig = Rig::new("repo-minus");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v1\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
        ],
    ));
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    let beta = rig.home.0.join(".claude/skills/beta");
    assert!(beta.join("SKILL.md").exists());
    // An edit rides on the leaving member — it must be absorbed before the dir goes.
    std::fs::write(beta.join("SKILL.md"), b"# my beta edit\n").unwrap();
    let beta_sid = {
        // The tracked import's id, for the snapshot-first witness below.
        let mut hit = None;
        for entry in rig.fs.read_dir(&rig.layout().skills_dir()).unwrap() {
            let id = entry.file_name().unwrap().to_str().unwrap().to_owned();
            let sid = crate::id::SkillId::parse(&id).unwrap();
            let lock: topos_types::persisted::Lock =
                crate::doc::read_doc(&rig.fs, &rig.layout().published(&sid).lock)
                    .unwrap()
                    .unwrap();
            if lock.name == "beta" {
                hit = Some(id);
            }
        }
        hit.expect("beta tracked")
    };
    let before = store_versions(&rig.layout(), &beta_sid);

    // The new archive no longer holds beta: the explicit update renders `-beta` AND retires the
    // copy, snapshot-first; the sidecar bytes stay.
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[("skills/alpha/SKILL.md", b"# alpha v2\n")],
    ));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = crate::message::legacy_lines(&out.disclosures)
        .into_iter()
        .find(|w| w.starts_with("GIT_UPDATED"))
        .expect("the moved-source line");
    assert!(line.contains("-beta"), "{line}");
    assert!(!beta.exists(), "the absent member's copy is retired");
    assert!(
        out.data
            .skills
            .iter()
            .any(|s| s.skill == "beta" && s.action == PullAction::Withdrawn),
        "{:?}",
        out.data.skills
    );
    assert!(
        store_versions(&rig.layout(), &beta_sid) > before,
        "the edit was snapshotted into the store first"
    );
    let sid = crate::id::SkillId::parse(&beta_sid).unwrap();
    assert!(
        rig.layout().skill_dir(&sid).exists(),
        "the sidecar bytes stay"
    );
}

#[test]
fn a_dropped_repo_row_cleans_its_members_like_any_undemanded_item() {
    let rig = Rig::new("repo-drop");
    rig.write_global("[bundles]\n\"github.com/o/r\" = \"*\"\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[("skills/alpha/SKILL.md", b"# alpha v1\n")],
    ));
    gate_add(&ctx, &plane, &dir, &git, "github.com/o/r");
    let alpha = rig.home.0.join(".claude/skills/alpha");
    assert!(alpha.exists());

    // The row goes; the next BARE sweep (no forge lane needed) retires the members' placements
    // and keeps every sidecar byte.
    rig.write_global("[bundles]\n");
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !alpha.exists(),
        "a dropped repo row's member is undemanded: {:?}",
        out.warnings
    );
    // The person's OWN choice ended it, so the row reads `removed` (with the destination that
    // left, `~`-abbreviated), never "withdrawn upstream".
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "alpha" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert_eq!(row.destinations, vec!["~/.claude/skills/alpha".to_owned()]);
    // Idempotent: nothing left to retire on the sweep after.
    let out2 = sweep(&ctx, &plane, &dir);
    assert!(
        !out2
            .data
            .skills
            .iter()
            .any(|s| matches!(s.action, PullAction::Removed | PullAction::Withdrawn)),
        "{:?}",
        out2.data.skills
    );
}

// =================================================================================================
// Per-scope mentions (the person clean is never shielded by a project mention).
// =================================================================================================

/// Driven through the BACKGROUND sweep: only a both-scope run converges the person scope from
/// inside a project, and the cross-scope shielding question only arises when it does.
#[test]
fn a_project_mention_never_shields_a_person_scope_clean() {
    let rig = Rig::new("scope-mention");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());

    // A project whose manifest mentions an UNRELATED thing of the same NAME (a local folder
    // called `deploy`).
    let proj = project("proj-mention", "[bundles]\n\"./deploy\" = \"*\"\n");
    std::fs::create_dir_all(proj.0.join("deploy")).unwrap();
    std::fs::write(proj.0.join("deploy/SKILL.md"), b"# local deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&proj.0));
    sweep_both(&ctx, &plane, &dir);
    let placed = rig.skills().join("deploy");
    assert!(placed.exists(), "the person feed installed its copy");

    // The feed withdraws the bundle. The person-scope copy must retire — the PROJECT's mention
    // of the same name is a different scope's business and shields nothing.
    plane.serves(Vec::new());
    let out = sweep_both(&ctx, &plane, &dir);
    assert!(
        !placed.exists(),
        "the person copy retired despite the project mention: {:?}",
        out.warnings
    );
    assert!(
        proj.0.join("deploy/SKILL.md").exists(),
        "the project's own folder is untouched"
    );
}

// =================================================================================================
// The applied report is complete-state: project stores and manifest deliveries included.
// =================================================================================================

#[test]
fn the_report_covers_project_manifest_deliveries() {
    let rig = Rig::new("report-proj");
    rig.seed_session();
    let proj = project(
        "proj-report",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    // The FEED delivers nothing — the demand is the project manifest row alone.
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        proj.0.join(".claude/skills/deploy/SKILL.md").exists(),
        "{:?}",
        out.warnings
    );
    // The applied report names the PROJECT-delivered bundle — held in the project's own store,
    // outside the feed — so the server's fleet state is never falsely empty.
    assert!(
        log.lock().unwrap().iter().any(|l| l == "report s_deploy"),
        "{:?}",
        log.lock().unwrap()
    );
}

#[test]
fn the_report_carries_another_checkouts_holdings() {
    // COMPLETE-state across checkouts: checkout A's project delivery keeps riding the applied
    // report when the update runs from checkout B (the visited-stores index) — and deleting A's
    // store drops it naturally on the next read.
    let rig = Rig::new("report-cross");
    rig.seed_session();
    let proj_a = project(
        "proj-cross-a",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let proj_b = project("proj-cross-b", "[bundles]\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    // The FEED delivers nothing — the demand is checkout A's manifest row alone.
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());

    // Checkout A delivers + reports its project bundle.
    let ctx_a = rig.ctx_at(Some(&proj_a.0));
    sweep(&ctx_a, &plane, &dir);
    assert!(proj_a.0.join(".claude/skills/deploy/SKILL.md").exists());
    let reports = |want: &str| {
        log.lock()
            .unwrap()
            .iter()
            .filter(|l| l.as_str() == want)
            .count()
    };
    assert_eq!(reports("report s_deploy"), 1, "{:?}", log.lock().unwrap());

    // The update runs from checkout B: A's holdings still ride the complete-state report — an
    // omission would make the server delete A's fleet rows.
    let ctx_b = rig.ctx_at(Some(&proj_b.0));
    sweep(&ctx_b, &plane, &dir);
    assert_eq!(
        reports("report s_deploy"),
        2,
        "checkout A's holdings ride the report from checkout B: {:?}",
        log.lock().unwrap()
    );

    // A's store goes (the checkout was deleted): the holding drops out of the report naturally.
    std::fs::remove_dir_all(proj_a.0.join(".topos")).unwrap();
    sweep(&ctx_b, &plane, &dir);
    assert_eq!(
        reports("report s_deploy"),
        2,
        "a deleted store's holdings leave the report: {:?}",
        log.lock().unwrap()
    );
    assert_eq!(
        reports("report "),
        1,
        "the report is honestly empty now: {:?}",
        log.lock().unwrap()
    );
}

#[test]
fn the_report_covers_a_declined_but_locally_added_bundle() {
    let rig = Rig::new("report-declined");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# noisy\n");
    let plane = FakePlane::new(log.clone()).with_version("s_noisy", &v);
    // Declined on the web: the feed omits it; the machine's own row still delivers it.
    plane.serve(DeliverySnapshot {
        declined: vec![("s_noisy".into(), "noisy".into())],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_noisy", "noisy", &v)], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/noisy\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        crate::message::legacy_lines(&out.advisories)
            .into_iter()
            .any(|w| w.starts_with("DECLINED_OVERRIDE")),
        "{:?}",
        out.warnings
    );
    assert!(rig.skills().join("noisy/SKILL.md").exists());
    // The report includes the declined-but-applied bundle — which is exactly what makes the
    // web's declined-but-applied disclosure real.
    assert!(
        log.lock().unwrap().iter().any(|l| l == "report s_noisy"),
        "{:?}",
        log.lock().unwrap()
    );
}

// =================================================================================================
// The forge add records the demand FIRST; a member failure leaves a convergent state.
// =================================================================================================

#[test]
fn a_failed_member_install_leaves_the_row_and_the_next_update_converges() {
    let rig = Rig::new("add-partial");
    let proj = project("proj-partial", "[bundles]\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let git = FakeGit::new(build_repo_targz(
        "o-r-aaaaaaaaaaaa1",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v1\n"),
            ("skills/beta/SKILL.md", b"# beta v1\n"),
        ],
    ));
    // beta's destination is OCCUPIED by foreign content — its install will refuse.
    let beta_dest = proj.0.join(".claude/skills/beta");
    std::fs::create_dir_all(&beta_dest).unwrap();
    std::fs::write(beta_dest.join("notes.txt"), b"mine\n").unwrap();

    let data = match ops::add_reference(
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
        ops::AddRefOutcome::Applied(d) => d,
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    };
    // The DEMAND landed first — the row is in the manifest even though beta did not land.
    let manifest = std::fs::read_to_string(proj.0.join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(manifest.contains("github.com/o/r"), "{manifest}");
    assert!(proj.0.join(".claude/skills/alpha/SKILL.md").exists());
    let note = data.note.clone().unwrap_or_default();
    assert!(note.contains("did not land: beta"), "{note}");
    assert!(
        note.contains("`topos update` completes the landing"),
        "{note}"
    );
    assert_eq!(
        std::fs::read(beta_dest.join("notes.txt")).unwrap(),
        b"mine\n",
        "the occupant is never clobbered"
    );

    // The occupation clears; the ordinary explicit update converges the landing — the row was
    // already the demand.
    std::fs::remove_dir_all(&beta_dest).unwrap();
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        beta_dest.join("SKILL.md").exists(),
        "converged: {:?}",
        out.warnings
    );
}

// =================================================================================================
// The stale-marker self-heal.
// =================================================================================================

/// A retirement marker records ONE conclusion — that nothing demanded the record — and this pass
/// owns whether that still holds. A machine already in the contradictory state (the marker under a
/// row that names the bundle) heals on its very next update: the marker goes, silently, and every
/// surface answers for the record again. Until it does, the row reads honestly never-applied — a
/// listing states what it can prove, and a record no walker reads is not among those things.
#[test]
fn a_sweep_revives_a_retired_record_a_row_still_claims() {
    let rig = Rig::new("stale-marker-heal");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global("[bundles]\n");
    let src = rig.home.0.join("tools/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();
    let mut added = ops::adopt_path(&ctx, &scope, &src, ops::KindDeclared::No).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut added,
        &scope.target,
        &src,
        &["~/dest-a".to_owned()],
    )
    .unwrap();
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join("dest-a/quaggamap/SKILL.md").exists());
    let record = sid(added.skill_id.as_deref().expect("an adopt records its id"));
    let version = added.version_id.clone().expect("the adopt minted one");

    // The contradiction, written directly: the marker under a row that still asks for the bundle.
    crate::sidecar::retire_record(&rig.fs, &rig.layout(), &record, rig_now(&rig)).unwrap();
    let item = machine_item(&ctx, "quaggamap").expect("the manifest row is still a line");
    assert!(
        item.version.is_none() && item.placements.is_empty(),
        "a record no walker reads answers for nothing: {item:?}"
    );

    // ONE ordinary update heals it, and says nothing: the retirement had its line, and a record
    // returning to the surfaces its own row already names is no news.
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        !rig.layout().published(&record).retired.exists(),
        "the live row lifted the stale marker"
    );
    assert!(
        !out.data
            .skills
            .iter()
            .any(|s| s.skill == "quaggamap" && s.action == PullAction::Released),
        "a revival is silent — never a second closing statement: {:?}",
        out.data.skills
    );

    // …and the record answers again: the version the row stands at, and the folders holding it.
    let item = machine_item(&ctx, "quaggamap").expect("the row is still a line");
    assert_eq!(item.version.as_deref(), Some(version.as_str()), "{item:?}");
    assert!(
        item.placements
            .iter()
            .any(|p| p.ends_with("dest-a/quaggamap")),
        "the placement is on the surfaces again: {item:?}"
    );
}

/// The inverse still holds: a record NOTHING claims stays retired through every later sweep — the
/// self-heal reads the demand, never the marker's age.
#[test]
fn a_sweep_leaves_a_retired_record_no_row_claims_exactly_as_it_is() {
    let rig = Rig::new("stale-marker-keep");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global("[bundles]\n");
    let src = rig.home.0.join("tools/quaggamap");
    skill_source(&src, b"# quaggamap\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = scoped_path_add(&ctx, &src, true).unwrap();
    let record = sid(added.skill_id.as_deref().expect("an adopt records its id"));

    // The row is dropped by hand, and the record retires on the next sweep — its ONE statement.
    rig.write_global("[bundles]\n");
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let marker = rig.layout().published(&record).retired;
    assert!(marker.exists(), "the one-time resolution ran");
    let stamp = std::fs::read(&marker).unwrap();

    // Two more sweeps say nothing and rewrite nothing.
    for _ in 0..2 {
        let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
        assert!(
            !out.data.skills.iter().any(|s| s.skill == "quaggamap"),
            "a retired record is never mentioned again: {:?}",
            out.data.skills
        );
    }
    assert!(marker.exists(), "…and it is still retired");
    assert_eq!(std::fs::read(&marker).unwrap(), stamp, "byte-identical");
}

/// `topos list <name> -g` — the deep answer, which is where a row's applied version and the
/// folders holding it are printed.
fn machine_item(ctx: &crate::ctx::Ctx<'_>, name: &str) -> Option<topos_types::results::ListDetail> {
    crate::ops::list_with(
        ctx,
        &ops::ListRequest {
            view: ops::ScopeView::Machine,
            name: Some(name.to_owned()),
            ..Default::default()
        },
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap()
    .data
    .detail
}
