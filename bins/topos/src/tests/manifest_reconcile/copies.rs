//! The copies a row placed. A whole-row `remove -g` takes every one of them out in the SAME
//! invocation, whatever enumerated the record; an update that CREATES a placement says `installed`
//! and "all up to date" may only claim itself; and a kind this build does not know is refused at
//! both doors rather than guessed into one it does.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use topos_core::digest::FileMode;
use topos_types::requests::{WireChannelEntry, WireChannelSkill};
use topos_types::results::PullAction;

use crate::error::ClientError;
use crate::sessions::Session;
use crate::{ops, sync_status};

use super::rig::*;

// =================================================================================================
// Whole-row `remove -g`: the row drop takes every copy the row placed out in the SAME invocation,
// whatever enumerated the record — the delivery cache, a live session, or nothing at all. The
// machine-scope cleaner walks the delivery cache and the forge imports; a record neither names (a
// local-path row, a frozen sweep while the plane is dark) must still leave through the verb's own
// retire rail, or the receipt's "the copies it placed leave this machine now" is a lie.
// =================================================================================================

/// The bare whole-row remove with the plane UNREACHABLE at remove time: the eager reconcile's
/// cache walk freezes (no fresh snapshot), so the verb's own rail must retire the copies — both
/// folders leave NOW, the receipt lists them, and the item's `dest_dirs` says what moved.
#[test]
fn an_offline_whole_row_remove_still_deletes_every_copy() {
    let (rig, plane, dir, _v) = add_rig("whole-offline");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join(".claude/skills/deploy/SKILL.md").exists());
    assert!(rig.home.0.join(".codex/skills/deploy/SKILL.md").exists());
    // The plane goes dark BEFORE the remove: every cache-walked clean is frozen this run.
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !rig.home.0.join(".claude/skills/deploy").exists(),
        "the claude copy leaves in-invocation; uninstalled={:?}",
        data.uninstalled
    );
    assert!(
        !rig.home.0.join(".codex/skills/deploy").exists(),
        "the codex copy leaves in-invocation; uninstalled={:?}",
        data.uninstalled
    );
    // The receipt speaks in what actually moved, and the item's dest_dirs carries it.
    let u = &data.uninstalled[0];
    assert_eq!(u.name, format!("@{WS_NAME}/deploy"));
    let mut dests = u.destinations.clone();
    dests.sort();
    assert_eq!(
        dests,
        vec![
            "~/.claude/skills/deploy".to_owned(),
            "~/.codex/skills/deploy".to_owned()
        ]
    );
    assert!(u.kept.is_empty(), "{:?}", u.kept);
    let mut dirs = data.items[0].dest_dirs.clone();
    dirs.sort();
    assert_eq!(dirs, dests);
}

/// The same offline whole-row remove with ONE EDITED copy: the clean copy leaves, the edited one
/// is KEPT IN PLACE (snapshotted into the store first) and the receipt's kept facts say so.
#[test]
fn an_offline_whole_row_remove_keeps_the_edited_copy_in_place() {
    let (rig, plane, dir, _v) = add_rig("whole-offline-edit");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let edited = rig.home.0.join(".codex/skills/deploy/SKILL.md");
    std::fs::write(&edited, b"# deploy\nlocal notes\n").unwrap();
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !rig.home.0.join(".claude/skills/deploy").exists(),
        "the clean copy leaves; uninstalled={:?}",
        data.uninstalled
    );
    assert_eq!(
        std::fs::read(&edited).unwrap(),
        b"# deploy\nlocal notes\n",
        "the edited copy stays in place, byte-identical"
    );
    let u = &data.uninstalled[0];
    assert_eq!(u.destinations, vec!["~/.claude/skills/deploy".to_owned()]);
    assert_eq!(u.kept, vec!["~/.codex/skills/deploy".to_owned()]);
    // Snapshot-first: the store holds the base version AND the kept edit.
    assert_eq!(store_versions(&rig.layout(), "s_deploy"), 2);
}

/// A `--dest` FREE-FORM folder row rides the same rail: the folder's copy leaves with the row.
#[test]
fn an_offline_whole_row_remove_cleans_a_free_form_dest_folder() {
    let (rig, plane, dir, _v) = add_rig("whole-offline-folder");
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/team-skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join("team-skills/deploy/SKILL.md").exists());
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !rig.home.0.join("team-skills/deploy").exists(),
        "the free-form folder's copy leaves; uninstalled={:?}",
        data.uninstalled
    );
    let u = &data.uninstalled[0];
    assert_eq!(u.destinations, vec!["~/team-skills/deploy".to_owned()]);
}

/// A MACHINE-scope LOCAL-PATH row: no delivery cache entry, no forge origin, no workspace names
/// this record — only the verb's own rail can take its managed copies out. The adopted-in-place
/// source dir is the person's own and is NEVER deleted.
#[test]
fn a_local_path_whole_row_remove_deletes_managed_copies_and_spares_the_source() {
    let (rig, plane, dir, _v) = add_rig("whole-local");
    rig.write_global("[bundles]\n");
    let src = rig.work.0.join("my-skill");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# mine\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let scope = ops::add_scope(&ctx, true).unwrap();
    let mut d = ops::adopt_path(&ctx, &scope, &src, ops::KindDeclared::No).unwrap();
    ops::note_added_path_dest_in(
        &ctx,
        &mut d,
        &scope.target,
        &src,
        &["~/.codex/skills".to_owned()],
    )
    .unwrap();
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join(".codex/skills/my-skill/SKILL.md").exists());
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["my-skill".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !rig.home.0.join(".codex/skills/my-skill").exists(),
        "the managed copy leaves in-invocation; uninstalled={:?}",
        data.uninstalled
    );
    // The adopted-in-place source is the person's own — never deleted.
    assert_eq!(std::fs::read(src.join("SKILL.md")).unwrap(), b"# mine\n");
    let u = &data.uninstalled[0];
    assert_eq!(u.name, "my-skill");
    assert_eq!(u.destinations, vec!["~/.codex/skills/my-skill".to_owned()]);
    assert_eq!(data.items[0].dest_dirs, u.destinations);
}

/// A whole-row `remove -g` of an MCP row takes its server entries OUT of the config files in the
/// same invocation (the inline converge) — no placement dirs are involved, and the item's note
/// names the removal per config file.
#[test]
fn a_whole_row_remove_of_an_mcp_row_takes_its_config_entries_out() {
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let rig = Rig::new("whole-mcp");
    rig.seed_session();
    std::fs::create_dir_all(rig.home.0.join(".cursor")).unwrap();
    let v = mk_version(&[(
        "server.json",
        FileMode::Regular,
        mcp_server_json("https://mcp.example/linear").as_bytes(),
    )]);
    let plane = FakePlane::new(log).with_version("s_linear", &v);
    plane.serves(Vec::new());
    let mut entry = catalog_entry("s_linear", "linear", &v);
    entry.kind = "mcp".into();
    let dir = FakeDirectory::new(vec![entry], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/linear\" = {{ dest = [\"~/.cursor/mcp.json\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let cursor = rig.home.0.join(".cursor/mcp.json");
    assert!(
        std::fs::read_to_string(&cursor)
            .unwrap()
            .contains("topos-eng-linear"),
        "the sweep placed the server entry"
    );
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["linear".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    assert!(
        !cursor.exists()
            || !std::fs::read_to_string(&cursor)
                .unwrap()
                .contains("topos-eng-linear"),
        "the config entry leaves in-invocation"
    );
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(note.contains("the server's entry was removed."), "{note}");
}

/// Two teams publish a `deploy` and only the OTHER team's copy is on this machine. Dropping the
/// row of the workspace that has NO record here must move nothing: the scope store answers BARE
/// names, so the fallback's clean would otherwise retire a stranger's record — deleting its files
/// and putting them on this row's receipt.
#[test]
fn a_whole_row_remove_spares_another_workspaces_same_named_record() {
    let rig = Rig::new("whole-cross-ws");
    // The OTHER workspace's copy lands first — it is the only connection while it does.
    seed_second_session(&rig);
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_ops_deploy", &v);
    plane.serves(vec![delivered("s_ops_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_ops_deploy", "deploy", &v)],
        Vec::new(),
    );
    rig.write_global("[bundles]\n\"beta.test/ops/deploy\" = { dest = [\"~/.codex/skills\"] }\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let placed = rig.home.0.join(".codex/skills/deploy/SKILL.md");
    assert!(placed.exists());

    // This machine's recipe now carries the FIRST team's same-named bundle instead — a row with
    // no record of its own here. The plane is dark, so only the verb's own rail could move
    // anything at all.
    rig.seed_session();
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/deploy")],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean row drop applies immediately: {other:?}"),
    };
    // The other workspace's bytes AND its record are untouched.
    assert_eq!(
        std::fs::read(&placed).unwrap(),
        b"# deploy\n",
        "another workspace's copy is not this row's to delete"
    );
    let sid = crate::id::SkillId::parse("s_ops_deploy").unwrap();
    assert!(
        !rig.layout().published(&sid).retired.exists(),
        "the other workspace's record is not retired"
    );
    // And the receipt claims nothing: no uninstall block, no cleaned dirs, no copies-leave line.
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
    assert!(
        data.items[0].dest_dirs.is_empty(),
        "{:?}",
        data.items[0].dest_dirs
    );
    let tty = crate::render::remove_applied_tty(&data);
    assert!(!tty.contains("leave this machine now"), "{tty}");
}

/// The copies-stay disclosure when a CHANNEL line is what still delivers the bundle: there is no
/// feed carrying it, so the note may not name one — and `topos remove -g <name>`, which is the off
/// write for a FEED-delivered bundle, gives way to the deep read that names the real line.
#[test]
fn removing_a_row_a_channel_line_still_delivers_names_no_feed() {
    let rig = Rig::new("row-channel-stays");
    rig.seed_session();
    // The login's feed line (which assigns this person nothing), a channel line, and an explicit
    // row for the same bundle — explicit beats set while the row stands.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/channels/backend\" = \"*\"\n\
         \"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"
    ));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    // The workspace's own feed assigns nothing — the channel line is the only other demand.
    plane.serves(Vec::new());
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
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let placed = rig.skills().join("deploy/SKILL.md");
    assert!(placed.exists());

    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/deploy")],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean row drop applies immediately: {other:?}"),
    };
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("/deploy\""), "the row left: {text}");
    assert!(
        text.contains("channels/backend"),
        "the set line stands: {text}"
    );
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}\"")),
        "the feed line stands too — and carries nothing of this name: {text}"
    );
    assert!(placed.exists(), "the channel-demanded copy stays in place");
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("another line here still delivers it"),
        "{note}"
    );
    assert!(note.contains("`topos list deploy` shows which"), "{note}");
    assert!(
        !note.contains("feed still delivers"),
        "no feed carries this bundle: {note}"
    );
    let tty = crate::render::remove_applied_tty(&data);
    assert!(tty.contains("the copies stay in place"), "{tty}");
    assert!(!tty.contains("leave this machine now"), "{tty}");
}

/// The same disclosure over a CROSS-WORKSPACE same-name collision: this workspace's feed line
/// stands but does not carry the bundle (its row was the only demand) — ANOTHER workspace's feed
/// delivers the name. The note names the workspace whose feed actually delivers it, so the off
/// switch it offers is the one that reaches the copies.
#[test]
fn removing_a_row_names_the_feed_that_actually_delivers_it() {
    let rig = Rig::new("row-cross-feed");
    rig.seed_session();
    seed_second_session(&rig);
    // This workspace's feed line + its explicit row; the feed itself assigns nothing.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"
    ));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let placed = rig.skills().join("deploy/SKILL.md");
    assert!(placed.exists());

    // The OTHER workspace's feed carries a bundle of the same name, and this machine adopts it.
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            "w_ops".to_owned(),
            sync_status::WorkspaceSync {
                host: Some("beta.test".to_owned()),
                workspace_name: Some("ops".to_owned()),
                last_delivery_at: Some(1),
                delivered: [(
                    "s_ops_deploy".to_owned(),
                    sync_status::DeliveredSkill {
                        name: "deploy".to_owned(),
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
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n\
         \"beta.test/ops\" = \"*\"\n"
    ));
    // Dark, so the frozen sweep moves nothing and the cached provenance stands.
    plane.serve_unreachable();
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[format!("{HOST}/{WS_NAME}/deploy")],
        None,
        false,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean row drop applies immediately: {other:?}"),
    };
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains("ops's feed still delivers it here"),
        "the feed that DELIVERS it is named, not the row's own workspace: {note}"
    );
    assert!(
        !note.contains(&format!("{WS_NAME}'s feed still delivers it here")),
        "this workspace's feed line carries nothing of the kind: {note}"
    );
    assert!(placed.exists(), "the copies stay in place");
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
}

// =================================================================================================
// Placement-heal honesty: an update that CREATES a placement (a folder materialized where none
// was) says `installed` — and "all up to date" may only claim itself when nothing changed on disk.
// =================================================================================================

/// Delete a delivered skill's placement folder and run `update`: the files come back — and the
/// receipt MUST say so (`+ … installed (…)`), never `all up to date`. Around the heal, a genuinely
/// untouched update keeps the exact all-up-to-date summary, byte for byte.
#[test]
fn healing_a_deleted_placement_reads_installed_never_all_up_to_date() {
    let rig = Rig::new("healrcpt");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // First sweep installs; the second is genuinely untouched and pins the exact summary — down to
    // the header's verb, which says what the run DID: it checked, it did not update.
    sweep(&ctx, &plane, &dir);
    let placed = rig.skills().join("deploy");
    assert!(placed.join("SKILL.md").exists());
    let clean = sweep(&ctx, &plane, &dir);
    assert_eq!(
        crate::render::pull_tty(
            &clean.data,
            &clean.decisions,
            &clean.warnings,
            &clean.advisories,
            &clean.disclosures,
            0,
        ),
        "checked machine-wide\nChecked 1 skill: all up to date."
    );

    // The placement folder vanishes (a hand-delete, an agent cleanup). The next update re-creates
    // it — files materialized where none were is an INSTALL, not "up to date".
    std::fs::remove_dir_all(&placed).unwrap();
    let healed = sweep(&ctx, &plane, &dir);
    assert!(
        placed.join("SKILL.md").exists(),
        "the heal restores the files"
    );
    let row = healed
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(
        row.action,
        PullAction::Installed,
        "a created placement is an install: {row:?}"
    );
    assert_eq!(row.destinations.len(), 1, "{:?}", row.destinations);
    let out = crate::render::pull_tty(
        &healed.data,
        &healed.decisions,
        &healed.warnings,
        &healed.advisories,
        &healed.disclosures,
        healed.failed_bundles.len(),
    );
    assert!(out.contains(&format!("+ @{WS_NAME}/deploy")), "{out}");
    assert!(out.contains("installed ("), "{out}");
    assert!(
        !out.contains("all up to date"),
        "a run that created a placement may not claim it: {out}"
    );

    // Healed and untouched again → exactly the all-up-to-date summary again.
    let again = sweep(&ctx, &plane, &dir);
    assert_eq!(
        crate::render::pull_tty(
            &again.data,
            &again.decisions,
            &again.warnings,
            &again.advisories,
            &again.disclosures,
            0,
        ),
        "checked machine-wide\nChecked 1 skill: all up to date."
    );
}

/// Drop the feed line (copies retire), re-add it, update: the bytes come back — the receipt says
/// `installed`, with the destination, even though the served version never moved.
#[test]
fn re_adding_the_feed_line_reinstalls_with_a_receipt_line() {
    let rig = Rig::new("readdline");
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

    // The feed line is removed: the next update retires the copies.
    rig.write_global("[bundles]\n");
    let retired = sweep(&ctx, &plane, &dir);
    assert!(
        !placed.exists(),
        "the dropped line retires the copies: {:?}",
        retired.data.skills
    );

    // The line comes back: the next update re-places the bytes — an install, said as one.
    rig.seed_feed();
    let back = sweep(&ctx, &plane, &dir);
    assert!(placed.join("SKILL.md").exists());
    let row = back
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(
        row.action,
        PullAction::Installed,
        "re-placed bytes are an install: {row:?}"
    );
    assert!(!row.destinations.is_empty(), "{row:?}");
    let out = crate::render::pull_tty(
        &back.data,
        &back.decisions,
        &back.warnings,
        &back.advisories,
        &back.disclosures,
        back.failed_bundles.len(),
    );
    assert!(out.contains("installed ("), "{out}");
    assert!(!out.contains("all up to date"), "{out}");
}

// ---------------------------------------------------------------------------------------------
// `-a`/`--dest` on `diff` / `publish` / `update --reset`: naming ONE copy when a bundle sits in
// several folders — including the divergent-copies FREEZE, which is the case the selector exists
// for. Naming the copy IS the choice the freeze asks for, so each verb reads past it rather than
// re-refusing, and the copy nobody named is never touched.
// ---------------------------------------------------------------------------------------------

/// A locally-adopted bundle in TWO of the machine's skills folders, both edited with DIFFERENT
/// bytes — the frozen shape. Returns the rig, the bundle's name, its id (which DIFFERS from the
/// name), and the two dirs (the adopted one first). The folders are ones the registry actually
/// spells, so `-a claude-code` names the second and the `~/` display form names either.
fn frozen_copies(tag: &str) -> (Rig, String, crate::id::SkillId, PathBuf, PathBuf) {
    let rig = Rig::new(tag);
    rig.seed_session();
    // The adopted copy lives in the shared agents folder; a sibling copy sits in Claude Code's.
    let shared = rig.home.0.join(".agents/skills/coolify-deploy");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("SKILL.md"), b"# coolify-deploy\nbase\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &shared).unwrap();
    let id = crate::id::SkillId::parse(added.skill_id.as_deref().unwrap()).unwrap();
    assert_ne!(id.as_str(), "coolify-deploy", "the id and the name differ");
    // The sibling: the SAME base bytes at the same recorded baseline — a clean managed replica
    // until it is edited below.
    let native = rig.home.0.join(".claude/skills/coolify-deploy");
    add_managed_copy(&rig, &id, &native, b"# coolify-deploy\nbase\n");
    // Two DIFFERENT edits: neither copy's bytes are the other's recorded baseline, so they are
    // true competitors — the freeze.
    std::fs::write(shared.join("SKILL.md"), b"# coolify-deploy\nshared edit\n").unwrap();
    std::fs::write(native.join("SKILL.md"), b"# coolify-deploy\nnative edit\n").unwrap();
    (rig, "coolify-deploy".to_owned(), id, shared, native)
}

/// Write `body` into `dir` and record it as a second MANAGED placement of `id`, at the record's
/// current baseline — the multi-folder shape the selector narrows.
fn add_managed_copy(rig: &Rig, id: &crate::id::SkillId, dir: &std::path::Path, body: &[u8]) {
    use topos_types::persisted::{PlacementKind, PlacementState, SwapCapability};
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("SKILL.md"), body).unwrap();
    let sp = rig.layout().published(id);
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    let mut map = crate::doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    map.placements.push(dir.display().to_string());
    map.placement_state.push(PlacementState {
        kind: PlacementKind::Native,
        agent: Some("claude-code".to_owned()),
        materialized_sha: Some(lock.bundle_digest),
        pre_existing_sha: None,
        swap_capability: SwapCapability::Unsupported,
        adopted_source: false,
        claim: None,
    });
    crate::doc::write_map(&rig.fs, &sp.map, &map).unwrap();
}

/// The freeze REFUSES a bare read and hands back a per-copy menu — and `--dest` reads straight past
/// it. The bypass is the point: the aggregate classification refuses before anything can be picked,
/// so before this a frozen bundle could not even be INSPECTED.
#[test]
fn zz_dest_reads_one_copy_of_a_frozen_bundle_while_the_bare_diff_still_refuses() {
    let (rig, name, id, _shared, native) = frozen_copies("zz-freeze-diff");
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Bare: the typed freeze, naming BOTH copies in both spellings the menu prints.
    let err = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .expect_err("a bare diff of divergent copies refuses");
    let ClientError::PlacementsDiverged { copies, .. } = &err else {
        panic!("the typed freeze: {err:?}");
    };
    let displays: Vec<&str> = copies.iter().map(|c| c.display.as_str()).collect();
    assert_eq!(
        displays,
        vec![
            "~/.agents/skills/coolify-deploy",
            "~/.claude/skills/coolify-deploy"
        ]
    );
    let dests: Vec<&str> = copies.iter().map(|c| c.dest.as_str()).collect();
    assert_eq!(dests, vec!["~/.agents/skills", "~/.claude/skills"]);

    // ALL THREE accepted spellings of the SAME copy read that copy — plus `-a`, the registry's
    // sugar for the row spelling.
    for sel in [
        ops::Selection::one(None, Some("~/.claude/skills")),
        ops::Selection::one(None, Some("~/.claude/skills/coolify-deploy")),
        ops::Selection::one(None, Some(&native.display().to_string())),
        ops::Selection::one(Some("claude-code"), None),
    ] {
        let d = ops::diff(
            &ctx,
            &name,
            None,
            ops::DiffBudget::unlimited(),
            &sel,
            ops::StoreScope::Here,
        )
        .expect("the selector reads past the freeze");
        assert!(d.diff.contains("native edit"), "{}", d.diff);
        assert!(!d.diff.contains("shared edit"), "{}", d.diff);
        // The header names WHICH copy — the bundle sits in more than one folder.
        assert_eq!(d.dest.as_deref(), Some("~/.claude/skills/coolify-deploy"));
        assert_eq!(d.skill.as_deref(), Some("coolify-deploy"));
    }

    // The OTHER copy, by its own folder — and the left-hand side is the SAME applied base, which
    // is what makes two `--dest` runs comparable.
    let sp = rig.layout().published(&id);
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    let other = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::one(None, Some("~/.agents/skills")),
        ops::StoreScope::Here,
    )
    .unwrap();
    assert!(other.diff.contains("shared edit"), "{}", other.diff);
    assert_eq!(other.version_id, lock.base_commit);
}

/// The two refusals the selector owes a person who names the wrong folder: one holding no copy of
/// this bundle (answered with every folder that DOES), and one holding a copy with nothing to act
/// on (answered plainly, rather than doing a no-op and reporting success).
#[test]
fn zz_a_dest_naming_no_copy_or_a_clean_copy_refuses_by_name() {
    let (rig, name, id, _shared, _native) = frozen_copies("zz-dest-refusals");
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::one(None, Some("~/.cursor/skills")),
        ops::StoreScope::Here,
    )
    .expect_err("a folder holding no copy refuses");
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    let message = crate::render::safe_message(&err);
    assert!(
        message.contains("~/.agents/skills/coolify-deploy")
            && message.contains("~/.claude/skills/coolify-deploy"),
        "the refusal names every copy it DOES have: {message}"
    );

    // A third copy at the recorded baseline — a managed replica with nothing edited in it.
    let clean = rig.home.0.join(".codex/skills/coolify-deploy");
    add_managed_copy(&rig, &id, &clean, b"# coolify-deploy\nbase\n");
    let err = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::one(None, Some("~/.codex/skills")),
        ops::StoreScope::Here,
    )
    .expect_err("a copy with no edits refuses rather than answering nothing");
    let message = crate::render::safe_message(&err);
    assert!(message.starts_with("that copy has no edits"), "{message}");
    assert!(
        message.contains("~/.codex/skills/coolify-deploy"),
        "{message}"
    );

    // A selector plus a `<ref>` asks two different questions at once: the selector narrows the
    // side that holds YOUR edits, and a version-to-version diff has no such side — it reads the
    // same in every folder. Refused whole, with both ways to re-spell it.
    let err = ops::diff(
        &ctx,
        &name,
        Some(&"ab".repeat(32)),
        ops::DiffBudget::unlimited(),
        &ops::Selection::one(None, Some("~/.claude/skills")),
        ops::StoreScope::Here,
    )
    .expect_err("a selector with a version reference refuses");
    assert_eq!(
        crate::render::safe_message(&err),
        "`--dest`/`-a` names the copy of YOUR edits a diff reads, and a version-to-version diff \
         reads the same in every folder — drop the `<ref>`, or drop the selector"
    );
}

/// The per-copy reset: the loss-led rail unchanged, narrowed to ONE folder. The described delta is
/// that copy's alone, the apply restores only that copy, and the copy left behind keeps its bytes —
/// which is what makes it the single ordinary draft afterwards.
#[test]
fn zz_a_per_copy_reset_drops_one_copys_edits_and_leaves_the_other_alone() {
    let (rig, name, _id, shared, native) = frozen_copies("zz-per-copy-reset");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let sel = ops::Selection::one(Some("claude-code"), None);

    let described = ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        false,
        ops::StoreScope::Here,
        &sel,
    )
    .unwrap();
    let ops::ResetOutcome::Described { items, yes_argv } = &described else {
        panic!("the bare reset DESCRIBES: {described:?}");
    };
    assert!(
        items[0].drop_diff.contains("native edit") && !items[0].drop_diff.contains("shared edit"),
        "the loss shown is THIS copy's: {}",
        items[0].drop_diff
    );
    // The SENTENCES around that delta say the same thing it does. The consent surface names the
    // one folder it takes and states, in the same breath, that the other copy keeps its edits —
    // a describe claiming the whole bundle's edits would be asking for the wrong yes.
    assert_eq!(
        items[0].dest.as_deref(),
        Some("~/.claude/skills/coolify-deploy")
    );
    assert_eq!(
        items[0].others_kept,
        vec!["~/.agents/skills/coolify-deploy"]
    );
    // The sentence states the loss plainly, in the same voice the receipt uses: the delta printed
    // under it is the argument, and a shouted word above it would only make the surface anxious.
    let described_tty = crate::render::reset_describe_tty(items, yes_argv);
    assert!(
        described_tty.starts_with(
            "Reset 'coolify-deploy' in ~/.claude/skills/coolify-deploy — discard \
             local edits to "
        ),
        "{described_tty}"
    );
    assert!(!described_tty.contains("DISCARDS"), "{described_tty}");
    assert!(
        described_tty
            .contains("your other copy in ~/.agents/skills/coolify-deploy keeps its edits"),
        "{described_tty}"
    );
    // The apply command carries the selector — without it `--yes` would discard both copies.
    assert_eq!(
        yes_argv,
        &vec![
            "topos".to_owned(),
            "update".to_owned(),
            name.clone(),
            "-a".to_owned(),
            "claude-code".to_owned(),
            "--reset".to_owned(),
            "--yes".to_owned(),
        ]
    );

    let applied = ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &sel,
    )
    .unwrap();
    let ops::ResetOutcome::Applied(done) = &applied else {
        panic!("`--yes` applies: {applied:?}");
    };
    // The receipt tells the same truth as the describe: the named copy, and what survived.
    let applied_tty = crate::render::reset_applied_tty(done);
    assert!(
        applied_tty.starts_with("Reset 'coolify-deploy' in ~/.claude/skills/coolify-deploy to "),
        "{applied_tty}"
    );
    // No recovery clause: the snapshot lands in whichever store owns the copy, and there is no
    // command a person could type to get it back — an offer nobody can take is not an offer.
    assert!(
        applied_tty.contains(
            "that copy's local edits discarded.\nyour other copy in \
             ~/.agents/skills/coolify-deploy keeps its edits"
        ),
        "{applied_tty}"
    );
    assert!(!applied_tty.contains("snapshot"), "{applied_tty}");
    assert_eq!(
        std::fs::read(native.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nbase\n",
        "the named copy is back at base"
    );
    assert_eq!(
        std::fs::read(shared.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nshared edit\n",
        "the copy nobody named keeps its edits"
    );
    // The freeze is gone: one edited copy left, so it is THE draft and a bare diff reads it.
    let d = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .expect("one edited copy is the ordinary draft");
    assert!(d.diff.contains("shared edit"), "{}", d.diff);
}

/// **A reset that names ONE folder does not end the merge.**
///
/// `--dest` narrows a reset so the copies it does not name keep their edits — and a copy still
/// holding its edits is a copy still holding this unfinished merge. Clearing the record there
/// would unblock `publish` while the merge stands in another folder. Git ends a merge at the
/// commit, and refuses to commit while anything is still unresolved; so the record stands until
/// the last copy is settled, and the reset that settles it takes the block and the marked-up copy
/// with it.
#[test]
fn zz_a_per_copy_reset_ends_the_merge_only_once_no_copy_still_holds_it() {
    use topos_types::persisted::{ConflictReason, ConflictState};

    let (rig, name, id, _shared, _native) = frozen_copies("zz-per-copy-reset-conflict");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let sp = rig.layout().published(&id);
    let copy = rig.layout().conflict_copy_dir(
        &crate::sidecar::ConflictDir::parse("coolify-deploy").expect("a plain safe component"),
    );
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), b"<<<<<<< mine\n").unwrap();
    // The record names the workbench AND the bytes topos wrote into it — the untouched signal a
    // reset reads before it removes anything.
    let marked = topos_core::digest::to_hex(&crate::scan::scan(&copy).unwrap().bundle_digest);
    let record = ConflictState {
        schema_version: 1,
        base_commit: "ab".repeat(32),
        base_digest: "ab".repeat(32),
        current_commit: "ab".repeat(32),
        current_digest: "ab".repeat(32),
        draft_commit: "ab".repeat(32),
        draft_digest: "ab".repeat(32),
        result_commit: "ab".repeat(32),
        conflicted_digest: marked,
        copy_dir: Some("coolify-deploy".to_owned()),
        reason: ConflictReason::ThreeWay,
        concluded: None,
        paths: Vec::new(),
    };
    crate::doc::write_doc(&rig.fs, &sp.conflict, &record).unwrap();

    let reset_one = |sel: ops::Selection| {
        let applied = ops::reset(
            &ctx,
            std::slice::from_ref(&name),
            true,
            ops::StoreScope::Here,
            &sel,
        )
        .unwrap();
        let ops::ResetOutcome::Applied(done) = applied else {
            panic!("`--yes` applies");
        };
        done
    };

    // ONE copy taken back to base; the other still holds its edits — so the merge is not over.
    let done = reset_one(ops::Selection::one(Some("claude-code"), None));
    assert!(
        sp.conflict.exists(),
        "a copy still holding this merge keeps the block"
    );
    assert!(copy.exists(), "and the marked-up copy stays with it");
    let tty = crate::render::reset_applied_tty(&done);
    assert!(
        tty.contains("your other copy in ~/.agents/skills/coolify-deploy keeps its edits"),
        "{tty}"
    );
    assert!(
        !tty.contains("hand merge"),
        "nothing survived unread: {tty}"
    );

    // The last copy: now nothing is unresolved, so the block clears and the workbench — still
    // exactly what topos wrote there — goes with it.
    let done = reset_one(ops::Selection::default());
    assert!(!sp.conflict.exists(), "the block is gone with its cause");
    assert!(!copy.exists(), "the marked-up copy goes with the record");
    let tty = crate::render::reset_applied_tty(&done);
    assert!(!tty.contains("publish"), "{tty}");
    assert!(!tty.contains("hand merge"), "{tty}");
}

/// **A reset never deletes a by-hand merge it did not read.** The workbench folder is the only copy
/// of a hand resolution — it sits outside the placement map, so the materializer's snapshot rail
/// never sees it — and `--reset` snapshots every placement while deleting that folder blind. Now it
/// reads it first: bytes topos wrote go, anything else stays and is named on the receipt.
/// `git merge --abort` removes no untracked file either.
#[test]
fn zz_a_reset_leaves_a_hand_merge_it_never_read_and_names_it() {
    use topos_types::persisted::{ConflictReason, ConflictState};

    let (rig, name, id, shared, _native) = frozen_copies("zz-reset-hand-merge");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let sp = rig.layout().published(&id);
    let copy = rig.layout().conflict_copy_dir(
        &crate::sidecar::ConflictDir::parse("coolify-deploy").expect("a plain safe component"),
    );
    std::fs::create_dir_all(&copy).unwrap();
    std::fs::write(copy.join("SKILL.md"), b"<<<<<<< mine\n").unwrap();
    let marked = topos_core::digest::to_hex(&crate::scan::scan(&copy).unwrap().bundle_digest);
    // …and then the person merges it by hand, so the folder no longer holds what topos wrote.
    std::fs::write(
        copy.join("SKILL.md"),
        b"# coolify-deploy\nreconciled by hand\n",
    )
    .unwrap();
    crate::doc::write_doc(
        &rig.fs,
        &sp.conflict,
        &ConflictState {
            schema_version: 1,
            base_commit: "ab".repeat(32),
            base_digest: "ab".repeat(32),
            current_commit: "ab".repeat(32),
            current_digest: "ab".repeat(32),
            draft_commit: "ab".repeat(32),
            draft_digest: "ab".repeat(32),
            result_commit: "ab".repeat(32),
            conflicted_digest: marked,
            copy_dir: Some("coolify-deploy".to_owned()),
            reason: ConflictReason::ThreeWay,
            concluded: None,
            paths: Vec::new(),
        },
    )
    .unwrap();
    // Both copies at once, so the merge really is over and the record really does clear.
    std::fs::write(shared.join("SKILL.md"), b"# coolify-deploy\nbase\n").unwrap();

    let applied = ops::reset(
        &ctx,
        std::slice::from_ref(&name),
        true,
        ops::StoreScope::Here,
        &ops::Selection::default(),
    )
    .unwrap();
    let ops::ResetOutcome::Applied(done) = &applied else {
        panic!("`--yes` applies: {applied:?}");
    };
    assert!(!sp.conflict.exists(), "the block still clears");
    assert_eq!(
        std::fs::read(copy.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nreconciled by hand\n",
        "the only copy of the hand merge must survive"
    );
    let tty = crate::render::reset_applied_tty(done);
    assert!(
        tty.contains("your hand merge is still in ~/.topos/conflicts/coolify-deploy"),
        "{tty}"
    );
}

/// Publishing ONE copy is the other way out of the freeze, and it is safe: the copy that shipped
/// advances to the new current, the copy that did not is never written, and the competitor test
/// then finds exactly one edited copy — the same shape "a teammate published while I had local
/// edits" has always produced. The receipt carries both halves.
#[test]
fn zz_a_per_copy_publish_leaves_the_other_copy_alone_and_resolves_the_freeze() {
    let (rig, name, _id, shared, native) = frozen_copies("zz-per-copy-publish");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let publish = |sel: &ops::Selection| {
        ops::publish(
            &ctx,
            Some(&session_connect),
            None,
            &name,
            false,
            None,
            None,
            None,
            sel,
            ops::StoreScope::Here,
        )
    };

    // A BARE publish still refuses — it must never pick for you; `--dest` is the consent.
    let bare = publish(&ops::Selection::default())
        .expect_err("a bare publish of divergent copies refuses");
    assert_eq!(bare.code(), "PLACEMENTS_DIVERGED");

    let outcome = publish(&ops::Selection::one(None, Some("~/.agents/skills")))
        .expect("the named copy publishes");
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    assert_eq!(
        data.from_placement.as_deref(),
        Some("~/.agents/skills/coolify-deploy")
    );
    assert_eq!(data.other_edited, vec!["~/.claude/skills/coolify-deploy"]);

    // THE safety property: the copy nobody named is byte-for-byte untouched.
    assert_eq!(
        std::fs::read(native.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nnative edit\n"
    );
    assert_eq!(
        std::fs::read(shared.join("SKILL.md")).unwrap(),
        b"# coolify-deploy\nshared edit\n",
        "the published copy keeps the bytes it shipped"
    );
    // And the freeze has resolved: one edited copy remains, so it is the single ordinary draft.
    let d = ops::diff(
        &ctx,
        &name,
        None,
        ops::DiffBudget::unlimited(),
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .expect("the survivor is the single draft");
    assert!(d.diff.contains("native edit"), "{}", d.diff);
}

// =================================================================================================
// A kind this build does not know: refused at both doors, never guessed into one it does.
// =================================================================================================

/// THE SWEEP DOOR. A workspace on a newer server serves a bundle whose `kind` this binary predates.
/// The row is skipped WHOLE — nothing fetched, no store record minted, nothing placed — and the run
/// says why in one line that names the bundle, the kind, and the way out. Guessing here would
/// materialize a bundle of unknown mechanics into skill folders; the point of the closed vocabulary
/// is that the guess is impossible.
#[test]
fn a_served_kind_this_build_cannot_deliver_is_skipped_and_named() {
    let rig = Rig::new("alien-kind-sweep");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# not a skill\n");
    let plane = FakePlane::new(log).with_version("s_alien", &v);
    let mut ds = delivered("s_alien", "runbook", &v);
    ds.kind = "knowledge".into();
    plane.serves(vec![ds]);
    let mut entry = catalog_entry("s_alien", "runbook", &v);
    entry.kind = "knowledge".into();
    let dir = FakeDirectory::new(vec![entry], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // ONE line, and it teaches: the bundle, the kind, and the command that makes this machine able
    // to take it.
    let w = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .find(|w| w.starts_with("UNKNOWN_KIND"))
        .unwrap_or_else(|| panic!("the refusal line; got {:?}", out.warnings));
    assert!(w.contains("runbook"), "{w}");
    assert!(w.contains("knowledge"), "{w}");
    assert!(w.contains("topos self-update"), "{w}");

    // The STORE is untouched: no record, no placement, and no receipt row claiming otherwise.
    let sid = crate::id::SkillId::parse("s_alien").unwrap();
    assert!(
        !rig.layout().skill_dir(&sid).exists(),
        "no store record is minted for a kind this build cannot place"
    );
    assert!(!rig.home.0.join(".claude/skills/runbook").exists());
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "runbook"),
        "no receipt row: {:?}",
        out.data.skills
    );

    // And the OFFLINE cache does not learn it either — a cached row would be read back next sweep
    // and placed as a skill, which is the exact corruption the refusal exists to prevent.
    let cache = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    assert!(
        cache
            .workspaces
            .values()
            .all(|ws| !ws.delivered.contains_key("s_alien")),
        "the delivery cache stays clean"
    );
}

/// THE `add` DOOR. Naming that same bundle refuses with the same teaching, before a row is written
/// for something no sweep could ever converge.
#[test]
fn adding_a_kind_this_build_cannot_deliver_refuses_with_the_same_teaching() {
    let rig = Rig::new("alien-kind-add");
    rig.seed_session();
    rig.write_global("[bundles]\n");
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# not a skill\n");
    let plane = FakePlane::new(log).with_version("s_alien", &v);
    plane.serves(Vec::new());
    let mut entry = catalog_entry("s_alien", "runbook", &v);
    entry.kind = "knowledge".into();
    let dir = FakeDirectory::new(vec![entry], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    let err = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("{HOST}/{WS_NAME}/runbook"),
        true,
        true,
        &Default::default(),
        None,
    )
    .expect_err("a kind this build cannot deliver is refused");
    let msg = err.to_string();
    assert!(msg.contains("runbook"), "{msg}");
    assert!(msg.contains("knowledge"), "{msg}");
    assert!(msg.contains("topos self-update"), "{msg}");

    // Refusal-first: nothing was written for it.
    let manifest =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!manifest.contains("runbook"), "{manifest}");
}

/// THE UNIVERSAL MARKER. The first sync of an ORDINARY SKILL writes its kind marker too — the one
/// durable rung the classifier reads. Before this the marker was minted only on the MCP divert, and
/// a skill's kind was re-derived from the delivery cache, which any sweep may drop.
#[test]
fn the_first_sync_of_a_skill_records_its_kind_durably() {
    let rig = Rig::new("skill-marker");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let marker = std::fs::read_to_string(rig.layout().published(&sid).kind).expect("kind.json");
    assert!(marker.contains("\"skill\""), "{marker}");

    // Which is what lets the chain answer with the delivery cache GONE — the rung this increment
    // deletes. Without the marker this record would classify as indeterminate.
    std::fs::remove_file(rig.layout().sync_status_path()).ok();
    assert_eq!(
        crate::bundle_kind::classify(&ctx, "s_deploy", &[]),
        crate::bundle_kind::RecordKind::Known(crate::bundle_kind::BundleKind::Skill),
    );
}
