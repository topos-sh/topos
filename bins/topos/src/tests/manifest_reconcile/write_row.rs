//! Writing a manifest ROW: `write_row` over an existing row and its exact-inverse discipline, the
//! remove loss-guard on every placement-retiring arm, the set split that carries the line's fields
//! onto each survivor, and the landed publish that its LOCAL rewrite half may never fail.

use std::sync::{Arc, Mutex};

use topos_types::requests::{WireChannelEntry, WireChannelSkill};

use crate::ctx::Ctx;
use crate::sessions::Session;
use crate::{ops, sync_status};

use super::rig::*;

// =================================================================================================
// `write_row` over an existing row: the exact-inverse discipline.
// =================================================================================================

#[test]
fn a_replaced_row_value_offers_only_the_exact_inverse() {
    let (rig, plane, dir, _v) = add_rig("row-replace");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let digest = "0123456789abcdef".repeat(4);

    // Replacing a `"*"` row with a pin: applied, the prior value named, the undo re-adds `"*"`.
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let data = applied_add(&ctx, &plane, &dir, &format!("@{WS_NAME}/deploy@{digest}"));
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "add".to_owned(),
            "-g".to_owned(),
            format!("{HOST}/{WS_NAME}/deploy"),
        ],
        "the undo restores the prior `\"*\"`, never a bare remove"
    );
    assert!(
        data.note.as_deref().unwrap_or("").contains("prior value"),
        "{:?}",
        data.note
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains(&digest), "the replace applied: {text}");

    // Replacing a PIN with `"*"`: the undo re-adds the exact pin.
    let data = applied_add(&ctx, &plane, &dir, &format!("@{WS_NAME}/deploy"));
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "add".to_owned(),
            "-g".to_owned(),
            format!("{HOST}/{WS_NAME}/deploy@{digest}"),
        ],
        "the undo restores the prior pin"
    );

    // The SAME value again: the redundancy disclosure — nothing written, and no undo (a remove
    // would delete a row that predates this add).
    let before =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let data = applied_add(&ctx, &plane, &dir, &format!("@{WS_NAME}/deploy"));
    assert!(data.undo.is_empty(), "{:?}", data.undo);
    assert!(
        data.note
            .as_deref()
            .unwrap_or("")
            .contains("already recorded"),
        "{:?}",
        data.note
    );
    let after =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert_eq!(before, after, "a same-value re-add writes nothing");

    // A prior FIELDS value: no single command restores it — no undo, and the note says why.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.codex/skills\"] }}\n"
    ));
    let data = applied_add(&ctx, &plane, &dir, &format!("@{WS_NAME}/deploy@{digest}"));
    assert!(
        data.undo.is_empty(),
        "no undo beats a wrong one: {:?}",
        data.undo
    );
    assert!(
        data.note.as_deref().unwrap_or("").contains("no undo"),
        "{:?}",
        data.note
    );
}

#[test]
fn destinations_extend_on_a_re_add_and_the_undo_takes_back_only_the_new_ones() {
    let (rig, plane, dir, _v) = add_rig("dest-extend");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let reference = format!("{HOST}/{WS_NAME}/deploy");
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);

    // The row already names ONE destination (written by hand — the person's own freeze).
    rig.write_global(&format!(
        "[bundles]\n\"{reference}\" = {{ dest = [\"~/.codex/skills\"] }}\n"
    ));
    let data = applied_dest_add(&ctx, &plane, &dir, &reference, &["~/.cursor/skills"]);
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        text.contains("\"~/.codex/skills\", \"~/.cursor/skills\""),
        "the standing entry survives and the new one is appended: {text}"
    );
    assert_eq!(
        data.dest_change.as_ref().map(|c| c.added.clone()),
        Some(vec!["~/.cursor/skills".to_owned()]),
        "the receipt names ONLY what it added"
    );
    assert!(
        data.dest_change.as_ref().is_some_and(|c| !c.default_reach),
        "the row already named destinations, so there is no default reach to disclose"
    );
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "remove".to_owned(),
            "-g".to_owned(),
            "deploy".to_owned(),
            "--dest".to_owned(),
            "~/.cursor/skills".to_owned(),
        ],
        "the inverse subtracts the new destination, never the whole set — and names the bundle \
         the way the add named it"
    );

    // The SAME destination again changes nothing, and says so.
    let before = std::fs::read_to_string(&manifest).unwrap();
    let data = applied_dest_add(&ctx, &plane, &dir, &reference, &["~/.cursor/skills"]);
    assert!(data.dest_change.is_none(), "{:?}", data.dest_change);
    assert!(data.undo.is_empty(), "{:?}", data.undo);
    assert!(
        data.note
            .as_deref()
            .unwrap_or_default()
            .contains("nothing changed"),
        "{:?}",
        data.note
    );
    assert_eq!(
        before,
        std::fs::read_to_string(&manifest).unwrap(),
        "a fully-duplicate add writes nothing"
    );

    // A brand-new row born WITH a destination is a whole-row add, so its inverse is the row drop.
    rig.write_global("[bundles]\n");
    let data = applied_dest_add(&ctx, &plane, &dir, &reference, &["~/.codex/skills"]);
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "remove".to_owned(),
            "-g".to_owned(),
            "deploy".to_owned(),
        ]
    );
}

#[test]
fn the_first_destination_on_a_row_that_reached_everywhere_keeps_that_reach() {
    // A row with NO `dest` reaches every agent, now and later. The add joins the new entry to the
    // `"*"` token that stands for that reach — never to a list of the folders one machine held on
    // one day, which would stop every agent installed after today.
    let rig = Rig::new("dest-star");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let reference = format!("{HOST}/{WS_NAME}/deploy");
    rig.write_global(&format!("[bundles]\n\"{reference}\" = \"*\"\n"));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    let before = std::fs::read_to_string(&manifest).unwrap();

    let data = applied_dest_add(&ctx, &plane, &dir, &reference, &["~/.cursor/skills"]);
    let change = data.dest_change.clone().expect("a destination-only act");
    assert_eq!(change.added, vec!["~/.cursor/skills".to_owned()]);
    assert!(change.default_reach, "the row still stands for its reach");
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        text.contains("dest = [\"*\", \"~/.cursor/skills\"]"),
        "the token sorts first and the new entry joins it: {text}"
    );

    // The inverse SUBTRACTS what this add put there — and lands the file back byte for byte,
    // because a row left standing for nothing but its default reach IS the `"*"` row it was.
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "remove".to_owned(),
            "-g".to_owned(),
            "deploy".to_owned(),
            "--dest".to_owned(),
            "~/.cursor/skills".to_owned(),
        ],
        "{:?}",
        data.undo
    );
    match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&[], &["~/.cursor/skills"]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(_) => {}
        other => panic!("the undo applies immediately: {other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        before,
        "add and its undo are exact inverses over the file"
    );

    // A PINNED row is the same story: the pin is what the row says beyond its reach, so the
    // collapse hands back the pin row itself, not a table spelling the pin and a lone token.
    let pinned = format!("[bundles]\n\"{reference}\" = \"{}\"\n", "a".repeat(64));
    rig.write_global(&pinned);
    applied_dest_add(&ctx, &plane, &dir, &reference, &["~/.cursor/skills"]);
    match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&[], &["~/.cursor/skills"]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(_) => {}
        other => panic!("the undo applies immediately: {other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), pinned);
}

#[test]
fn an_off_switch_is_never_extended_and_never_advertises_a_subtraction() {
    // An `"off"` row is the row's own NEGATION, not a destination set. Treating it as one
    // advertised `remove --dest X` as the inverse — which removes the last destination, drops the
    // row, and resumes the feed EVERYWHERE, the opposite of the state it claimed to restore.
    let (rig, plane, dir, _v) = add_rig("off-dest");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let reference = format!("{HOST}/{WS_NAME}/deploy");
    rig.write_global(&format!("[bundles]\n\"{reference}\" = \"off\"\n"));

    let data = applied_dest_add(&ctx, &plane, &dir, &reference, &["~/.cursor/skills"]);
    assert!(
        data.dest_change.is_none(),
        "an off switch is not a destination set: {:?}",
        data.dest_change
    );
    assert!(
        data.undo.is_empty(),
        "no undo beats a wrong one: {:?}",
        data.undo
    );
    assert!(
        data.note.as_deref().unwrap_or_default().contains("no undo"),
        "and the receipt says why: {:?}",
        data.note
    );
}

#[test]
fn a_pin_move_beside_a_destination_is_a_replacement_not_an_extend() {
    // `add <ref>@<new-pin> --dest B` changes TWO things. A destination-only receipt left the moved
    // pin unsaid and offered an inverse that left it moved; the ordinary replaced-prior path names
    // the pin and offers only an undo that verifiably restores it.
    let (rig, plane, dir, _v) = add_rig("pin-and-dest");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let reference = format!("{HOST}/{WS_NAME}/deploy");
    let old = "a".repeat(64);
    let new = "b".repeat(64);
    rig.write_global(&format!("[bundles]\n\"{reference}\" = \"{old}\"\n"));

    let data = applied_dest_add(
        &ctx,
        &plane,
        &dir,
        &format!("{reference}@{new}"),
        &["~/.cursor/skills"],
    );
    assert!(
        data.dest_change.is_none(),
        "not a destination-only act: {:?}",
        data.dest_change
    );
    assert_eq!(
        data.undo,
        vec![
            "topos".to_owned(),
            "add".to_owned(),
            "-g".to_owned(),
            format!("{reference}@{old}"),
        ],
        "the undo restores the PIN the add moved — an `add`-shaped restore keeps the canonical \
         reference, because a bare name would resolve to the record now standing instead"
    );
    assert!(
        data.note
            .as_deref()
            .unwrap_or_default()
            .contains("prior pin"),
        "{:?}",
        data.note
    );
    // Both changes landed, whatever the receipt classified the act as.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&new) && text.contains("~/.cursor/skills"),
        "{text}"
    );
}

#[test]
fn a_narrowed_rows_current_set_comes_from_the_row_not_from_a_name_two_bundles_share() {
    // Resolving "where does this bundle live now" by BARE NAME collapsed on a collision, and an
    // empty current set reads as "reaches nowhere" — so the materialize a narrowing runs would
    // have dropped every copy the bundle really had. The row's own identity cannot collide.
    let rig = Rig::new("dest-qualified");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let reference = format!("{HOST}/{WS_NAME}/deploy");
    rig.write_global(&format!("[bundles]\n\"{reference}\" = \"*\"\n"));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);

    // A SECOND, unrelated bundle of the same name, adopted from a folder into the same store —
    // exactly the collision that made the bare-name lookup ambiguous.
    let twin = untracked_skill(&rig.home.0, "deploy", b"# a different deploy\n");
    ops::add_with_name(
        &ctx,
        &twin,
        Some("deploy"),
        true,
        crate::bundle_kind::BundleKind::Skill,
    )
    .unwrap();

    // The narrow materializes the row's current set before subtracting. Resolved by bare name the
    // set would be EMPTY (two records answer to `deploy`), and the subtraction would refuse with
    // "nothing has synced" over a bundle whose copy is on disk.
    let shared = rig.pretty(&rig.skills());
    // `--yes`: the twin makes the by-name draft scan indeterminate, which is its own gate.
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        std::slice::from_ref(&reference),
        None,
        true,
        &sel(&[], &[&shared]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("the workspace row's own copy resolved: {other:?}"),
    };
    assert_eq!(data.items.len(), 1, "{:?}", data.items);
    assert!(
        !rig.skills().join("deploy").exists(),
        "the copy the row's own record named is the one that left"
    );
}

#[test]
fn the_governance_transfer_carries_the_rows_destinations_onto_the_workspace_row() {
    // Governance moves; LOCAL STATE STAYS. A path row frozen to two folders is the person's
    // standing decision about where this machine puts the bytes — the workspace taking over the
    // version history says nothing about that, so the freeze rides onto the new row.
    let rig = Rig::new("govern-dest");
    rig.seed_session();
    let folder = untracked_skill(&rig.home.0, "deploy", b"# deploy\n");
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = {{ dest = [\"~/.codex/skills\", \"~/.cursor/skills\"] }}\n",
        folder.display()
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let outcome =
        ops::rewrite_to_governed(&ctx, "deploy", HOST, WS_NAME, std::slice::from_ref(&folder))
            .expect("the rewrite reads and writes the machine file");
    assert!(matches!(outcome, ops::GovernedOutcome::Rewritten(_)));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert_eq!(
        text,
        format!(
            "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.codex/skills\", \
             \"~/.cursor/skills\"] }}\n"
        ),
        "the path row is gone and the workspace row carries the same two folders"
    );

    // A row with NO freeze keeps the plain `"*"` — there is no local state to carry.
    let plain = Rig::new("govern-plain");
    plain.seed_session();
    let bare = untracked_skill(&plain.home.0, "deploy", b"# deploy\n");
    plain.write_global(&format!("[bundles]\n\"{}\" = \"*\"\n", bare.display()));
    let pctx = plain.ctx_at(Some(&plain.work.0));
    ops::rewrite_to_governed(&pctx, "deploy", HOST, WS_NAME, &[bare]).unwrap();
    assert_eq!(
        std::fs::read_to_string(plain.layout().home().join(crate::manifest::MANIFEST_FILE))
            .unwrap(),
        format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n")
    );
}

// =================================================================================================
// The remove loss-guard covers EVERY placement-retiring arm: feed drop and the `"off"` switch.
// =================================================================================================

#[test]
fn an_off_switch_over_a_draft_applies_with_the_kept_disclosure() {
    let rig = Rig::new("off-gate");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // CLEAN: the off switch is a reversible file edit — applies immediately.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Applied(data) => {
            assert!(data.applied);
            let text =
                std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE))
                    .unwrap();
            assert!(text.contains("\"off\""), "{text}");
        }
        other => panic!("a clean off switch applies immediately: {other:?}"),
    }
    // Lift the switch back for the drafted arm.
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);

    // DRAFTED: the edited copy is KEPT IN PLACE by the eager uninstall, so nothing is
    // loss-shaped — the switch applies immediately, the note disclosing the kept copy.
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Applied(data) => {
            assert!(data.applied);
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("local edits"), "{note}");
            // The note names the FOLDER the edits stay in. It used to send the reader to
            // `topos list <name>`, which answers "not managed on this machine" once the row is
            // gone — the one command it named could never show the copy it was about.
            assert!(
                note.contains(&format!("they stay in {}", rig.pretty(&placed))),
                "{note}"
            );
            assert!(!note.contains("topos list"), "{note}");
            // The edited copy really is still on disk.
            assert_eq!(
                std::fs::read(placed.join("SKILL.md")).unwrap(),
                b"# my edit\n"
            );
        }
        other => panic!("a drafted off switch applies with the kept disclosure: {other:?}"),
    }
}

#[test]
fn a_feed_drop_applies_immediately_and_uninstalls_in_the_same_invocation() {
    let rig = Rig::new("feeddrop-gate");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    let placed = install_feed_deploy(&rig, &plane, &dir);
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // DRAFTED: the edited copy is KEPT IN PLACE, so nothing is loss-shaped — a bare drop applies
    // immediately, uninstalls in the SAME invocation, and the receipt carries the kept copy and
    // the sugared undo.
    std::fs::write(placed.join("SKILL.md"), b"# my edit\n").unwrap();
    let token = format!("@{WS_NAME}");
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        std::slice::from_ref(&token),
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    let data = match out {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a bare feed drop applies immediately: {other:?}"),
    };
    assert_eq!(
        data.undo,
        vec!["topos", "add", "-g", &format!("@{WS_NAME}")],
        "the undo line spells the sugar at the machine's one host"
    );
    let u = data
        .uninstalled
        .iter()
        .find(|u| u.name == format!("@{WS_NAME}/deploy"))
        .unwrap_or_else(|| panic!("{:?}", data.uninstalled));
    assert!(u.destinations.is_empty(), "{:?}", u.destinations);
    assert_eq!(u.kept, vec![rig.pretty(&placed)]);
    assert!(
        placed.join("SKILL.md").exists(),
        "the edited copy stays in place"
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains(&format!("{HOST}/{WS_NAME}")), "{text}");

    // CLEAN: a fresh unedited install leaves in the SAME invocation — the receipt names the
    // destination that went, and the dir is gone before the command returns. (The kept copy is
    // the person's own file now; they delete it themselves before re-adopting the feed.)
    std::fs::remove_dir_all(&placed).unwrap();
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n"));
    sweep(&ctx, &plane, &dir);
    assert!(placed.join("SKILL.md").exists());
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &[token],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    let data = match out {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean feed drop applies immediately: {other:?}"),
    };
    let u = data
        .uninstalled
        .iter()
        .find(|u| u.name == format!("@{WS_NAME}/deploy"))
        .unwrap_or_else(|| panic!("{:?}", data.uninstalled));
    assert_eq!(u.destinations, vec![rig.pretty(&placed)]);
    assert!(u.kept.is_empty(), "{:?}", u.kept);
    assert!(!placed.exists(), "the copies left in this invocation");
}

// =================================================================================================
// The set split carries the line's fields onto each surviving member.
// =================================================================================================

#[test]
fn a_set_split_carries_the_lines_fields_onto_survivors() {
    let rig = Rig::new("split-fields");
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
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![
                WireChannelSkill {
                    skill_id: "s_deploy".into(),
                    name: "deploy".into(),
                },
                WireChannelSkill {
                    skill_id: "s_other".into(),
                    name: "other".into(),
                },
            ],
        }],
    );
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = {{ dest = [\"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The describe discloses BOTH the split and the field carriage.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { data, .. } => {
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("dest settings carry"), "{note}");
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap();
    let survivor = doc
        .rows
        .iter()
        .find(|r| r.reference == format!("{HOST}/{WS_NAME}/other"))
        .unwrap_or_else(|| panic!("the survivor keeps its own row: {text}"));
    match &survivor.value {
        crate::manifest::document::EntryValue::Fields(f) => {
            assert_eq!(
                f.dest.as_deref(),
                Some(&["~/.codex/skills".to_owned()][..]),
                "{text}"
            );
        }
        other => panic!("the set line's fields ride the member row, got {other:?}: {text}"),
    }
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference.contains("channels/backend")),
        "the set line is gone: {text}"
    );
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/deploy")),
        "the removed member gets no row: {text}"
    );
}

#[test]
fn a_set_split_never_overwrites_a_survivors_explicit_row() {
    // Explicit beats set: a survivor that already has its OWN row keeps it untouched (its pin is
    // a stronger fact than the set's fields); a survivor without one gets the new carried row.
    let rig = Rig::new("split-explicit");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let d = one_file(b"# deploy\n");
    let o = one_file(b"# other\n");
    let p = one_file(b"# pinned\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &d)
        .with_version("s_other", &o)
        .with_version("s_pinned", &p);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &d),
            catalog_entry("s_other", "other", &o),
            catalog_entry("s_pinned", "pinned", &p),
        ],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![
                WireChannelSkill {
                    skill_id: "s_deploy".into(),
                    name: "deploy".into(),
                },
                WireChannelSkill {
                    skill_id: "s_other".into(),
                    name: "other".into(),
                },
                WireChannelSkill {
                    skill_id: "s_pinned".into(),
                    name: "pinned".into(),
                },
            ],
        }],
    );
    let pin = topos_core::digest::to_hex(&p.id);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/channels/backend\" = {{ dest = [\"~/.codex/skills\"] }}\n\
         \"{HOST}/{WS_NAME}/pinned\" = \"{pin}\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The describe names which survivors get new rows and which keep their own.
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &Default::default(),
    )
    .unwrap();
    match out {
        ops::RemoveOutcome::Described { data, .. } => {
            let note = data.items[0].note.clone().unwrap_or_default();
            assert!(note.contains("new rows are written for other"), "{note}");
            assert!(
                note.contains("pinned already has its own row here, kept untouched"),
                "{note}"
            );
        }
        other => panic!("a set split describes first: {other:?}"),
    }
    let out = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        true,
        &Default::default(),
    )
    .unwrap();
    assert!(matches!(out, ops::RemoveOutcome::Applied(_)));
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let doc = crate::manifest::document::parse_manifest(
        &text,
        crate::manifest::document::ManifestScope::Global,
    )
    .unwrap();
    // The explicit survivor KEEPS its pin — never replaced by the set's fields value.
    let kept = doc
        .rows
        .iter()
        .find(|r| r.reference == format!("{HOST}/{WS_NAME}/pinned"))
        .unwrap_or_else(|| panic!("the explicit survivor keeps its row: {text}"));
    assert!(
        matches!(&kept.value, crate::manifest::document::EntryValue::Pin(v) if *v == pin),
        "the explicit pin survives the split, got {:?}: {text}",
        kept.value
    );
    // The row-less survivor gets the NEW carried row; the set line and the removed member go.
    let fresh = doc
        .rows
        .iter()
        .find(|r| r.reference == format!("{HOST}/{WS_NAME}/other"))
        .unwrap_or_else(|| panic!("the row-less survivor gets a new row: {text}"));
    assert!(
        matches!(&fresh.value, crate::manifest::document::EntryValue::Fields(f)
            if f.dest.as_deref() == Some(&["~/.codex/skills".to_owned()][..])),
        "{text}"
    );
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference.contains("channels/backend")),
        "the set line is gone: {text}"
    );
    assert!(
        !doc.rows
            .iter()
            .any(|r| r.reference == format!("{HOST}/{WS_NAME}/deploy")),
        "the removed member gets no row: {text}"
    );
}

// =================================================================================================
// A landed publish is never failed by its LOCAL rewrite half; the retry/update converge it.
// =================================================================================================

#[test]
fn a_landed_publish_survives_a_failed_rewrite_and_the_next_update_converges_it() {
    let rig = Rig::new("rewrite-pending");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());

    // A local skill adopted by path.
    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();

    // The catalog serves the governed copy (for the later converge).
    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(
        vec![catalog_entry(
            added.skill_id.as_deref().unwrap_or_default(),
            "deploy",
            &v,
        )],
        Vec::new(),
    );

    // The GLOBAL manifest is UNPARSEABLE: the rewrite half will fail; the publish must not.
    rig.write_global("this is [[ not toml\n");

    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::publish(
        &ctx,
        Some(&session_connect),
        None,
        "deploy",
        false,
        None,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    let pending = data
        .rewrite_pending
        .expect("the receipt carries the pending rewrite");
    assert!(pending.contains("could not be rewritten"), "{pending}");
    assert!(pending.contains("topos update"), "{pending}");
    assert!(
        data.manifest.is_none() && data.converted_from.is_none(),
        "the receipt never claims a rewrite that did not land"
    );

    // The manifest heals (a fixed file still spelling the PATH line — exactly what the failed
    // rewrite left behind); the NEXT update converges the transfer, idempotently, disclosed.
    let canonical_src = src.canonicalize().unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = \"*\"\n",
        canonical_src.display()
    ));
    let out = ops::manifest_update(
        &ctx,
        &session_connect,
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = crate::message::legacy_lines(&out.advisories)
        .into_iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.advisories));
    assert!(line.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{line}");
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "the canonical reference stands: {text}"
    );
    assert!(
        !text.contains(&canonical_src.display().to_string()),
        "the path line is gone: {text}"
    );

    // Idempotent: the next sweep finds nothing left to converge.
    let out2 = ops::manifest_update(
        &ctx,
        &session_connect,
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    assert!(
        !crate::message::legacy_lines(&out2.advisories)
            .into_iter()
            .any(|w| w.starts_with("GOVERNANCE_CONVERGED")),
        "{:?}",
        out2.warnings
    );
}

/// A landed publish IS the new current, and the row naming it has to say so with no sweep in
/// between. `behind` is decided OFFLINE against the delivery cache — what the plane last SERVED —
/// which only the sweep used to write, so a publish that advanced its local documents alone left
/// `list` tagging the version it had just published `behind`, contradicting the receipt printed a
/// line earlier. A landed push updates the remote-tracking ref; so does this.
#[test]
fn a_landed_publish_leaves_its_own_version_current_before_any_sweep() {
    let rig = Rig::new("publish-served");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());

    // A bundle adopted here and already governed by the workspace: the delivery cache carries the
    // sweep's row for it, naming exactly the version this copy applies (so the row reads current).
    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();
    let id = added.skill_id.clone().expect("the adopt minted a record");
    let base = added.version_id.clone().expect("and a base version");
    // The machine recipe demands it by path — the line the landed publish transfers to the
    // workspace reference.
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = \"*\"\n",
        src.canonicalize().unwrap().display()
    ));
    sync_status::record(
        &rig.fs,
        &rig.layout(),
        &[(
            WS.to_owned(),
            sync_status::WorkspaceSync {
                host: Some(HOST.to_owned()),
                workspace_name: Some(WS_NAME.to_owned()),
                last_delivery_at: Some(1),
                delivered: [(
                    id.clone(),
                    sync_status::DeliveredSkill {
                        name: "deploy".to_owned(),
                        served_version: base.clone(),
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

    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(vec![catalog_entry(&id, "deploy", &v)], Vec::new());
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };

    // Edit the copy and publish it — a clean landed publish (state ①).
    std::fs::write(src.join("SKILL.md"), b"# deploy, sharper\n").unwrap();
    let outcome = ops::publish(
        &ctx,
        Some(&session_connect),
        None,
        "deploy",
        false,
        None,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    assert_ne!(data.version_id, base, "the publish minted a new version");

    // The remote-tracking half: the cache names what was just published, not what it replaced.
    let cache = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    assert_eq!(
        cache.workspaces[WS].delivered[&id].served_version, data.version_id,
        "the delivery cache holds the version this publish landed"
    );

    // THE REPORTED BUG: the very next `list`, with no sweep in between, must not tag the published
    // version `behind` its own publish.
    let listed = crate::ops::list_with(
        &ctx,
        &ops::ListRequest::default(),
        None,
        None,
        crate::ops::RowPage::unlimited(),
    )
    .unwrap();
    let row = listed
        .data
        .scopes
        .iter()
        .flat_map(|s| &s.rows)
        .find(|r| r.skill == "deploy")
        .unwrap_or_else(|| panic!("the published row is listed: {:?}", listed.data.scopes));
    assert_eq!(row.version_id, data.version_id, "{row:?}");
    assert_eq!(
        row.status,
        Some(topos_types::results::SkillStatus::Current),
        "the row agrees with the receipt that just printed: {row:?}"
    );
}

#[test]
fn a_project_scope_pending_rewrite_converges_from_the_projects_own_store() {
    // The pending governance transfer of a bundle tracked in the PROJECT's own store: the
    // converge must search the owning store across the cwd chain — a home-only search would
    // leave a project-scope publish's failed rewrite pending forever.
    let rig = Rig::new("rewrite-pending-proj");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let proj = project("proj-pending", "[bundles]\n");

    // A local skill adopted into the PROJECT's own store (the checkout's engine state).
    let src = proj.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let playout = crate::sidecar::ensure_project_store(&rig.fs, &proj.0).unwrap();
    let added = {
        let mut pctx = rig.ctx_at(Some(&proj.0));
        pctx.layout = playout.clone();
        ops::add(&pctx, &src).unwrap()
    };
    let ctx = rig.ctx_at(Some(&proj.0));

    // The catalog serves the governed copy (for the later converge).
    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(
        vec![catalog_entry(
            added.skill_id.as_deref().unwrap_or_default(),
            "deploy",
            &v,
        )],
        Vec::new(),
    );

    // The PROJECT manifest is UNPARSEABLE: the rewrite half will fail; the publish must not.
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        "this is [[ not toml\n",
    )
    .unwrap();
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::publish(
        &ctx,
        Some(&session_connect),
        None,
        "deploy",
        false,
        None,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    assert!(
        data.rewrite_pending.is_some(),
        "the failed rewrite rides the receipt"
    );

    // The manifest heals, still spelling the PATH line; the NEXT update converges the transfer
    // from the PROJECT store, idempotently, disclosed.
    std::fs::write(
        proj.0.join(crate::manifest::MANIFEST_FILE),
        "[bundles]\n\"./deploy\" = \"*\"\n",
    )
    .unwrap();
    let out = ops::manifest_update(
        &ctx,
        &session_connect,
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = crate::message::legacy_lines(&out.advisories)
        .into_iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.advisories));
    assert!(line.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{line}");
    let text = std::fs::read_to_string(proj.0.join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "the canonical reference stands in the PROJECT file: {text}"
    );
    assert!(!text.contains("./deploy"), "the path line is gone: {text}");
}

#[test]
fn a_removal_that_lands_mid_publish_is_never_silently_undone() {
    // LOCK, THEN RESOLVE. The rewrite's row is a decision read from a file, and it is only true of
    // the file the writer lock guards — so the unlocked probe only picks WHICH file, and the row is
    // re-resolved under the lock. A `topos remove` that completed in that window leaves the person
    // with a state they chose: either serialization order they could have observed ends with the
    // row gone, and "removed, then quietly re-added" is neither. The publish still LANDS remotely
    // (the plane holds it); the receipt discloses that the local half wrote nothing.
    let rig = Rig::new("rewrite-raced");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());

    // A local skill adopted by path, and the manifest row that references it.
    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();
    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(
        vec![catalog_entry(
            added.skill_id.as_deref().unwrap_or_default(),
            "deploy",
            &v,
        )],
        Vec::new(),
    );
    let canonical_src = src.canonicalize().unwrap();
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = \"*\"\n",
        canonical_src.display()
    ));

    // The RACER: a concurrent removal completes between the probe that chose the file (read 1) and
    // the re-resolve under the lock (read 2). topos's own writers serialize on that lock; this one
    // stands for the removal that already finished before the lock was taken.
    let racing = manifest.clone();
    let fs = crate::fs_seam::HookFs::before_nth_read(&manifest, 2, move || {
        std::fs::write(&racing, "[bundles]\n").unwrap();
    });
    let hooked = Ctx {
        fs: &fs,
        ..rig.ctx_at(Some(&rig.work.0))
    };
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::publish(
        &hooked,
        Some(&session_connect),
        None,
        "deploy",
        false,
        None,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    };
    let skipped = data
        .rewrite_skipped
        .expect("the receipt discloses the transfer it did not make");
    assert!(
        skipped.contains(&manifest.display().to_string()),
        "{skipped}"
    );
    assert!(
        data.manifest.is_none() && data.reference.is_none() && data.converted_from.is_none(),
        "the receipt never claims a rewrite that did not land"
    );

    // The removal STANDS: no canonical row was written back over it, and the path row it dropped
    // stays dropped.
    let text = std::fs::read_to_string(&manifest).unwrap();
    assert!(
        !text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "no workspace row was written: {text}"
    );
    assert!(
        !text.contains(&canonical_src.display().to_string()),
        "the racer's removal stands: {text}"
    );
}

#[test]
fn a_genesis_propose_pending_rewrite_still_converges() {
    // A genesis `--propose` never moves `current`, so the local sync stays GENESIS-observed —
    // the converge's safe condition is the single-catalog-home check (the landed proposal minted
    // the catalog entry), never a blanket GENESIS refusal.
    let rig = Rig::new("rewrite-pending-genesis");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());

    let src = rig.work.0.join("deploy");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("SKILL.md"), b"# deploy\n").unwrap();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let added = ops::add(&ctx, &src).unwrap();

    // The catalog holds the proposal's entry (the server-side half of the landed propose).
    let v = one_file(b"# deploy\n");
    let dir = FakeDirectory::new(
        vec![catalog_entry(
            added.skill_id.as_deref().unwrap_or_default(),
            "deploy",
            &v,
        )],
        Vec::new(),
    );

    // The GLOBAL manifest is UNPARSEABLE: the rewrite half fails; the proposal still lands.
    rig.write_global("this is [[ not toml\n");
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(dir.clone()),
        contribute: Box::new(OkPropose),
        governance: Box::new(NoGovernance),
    };
    let outcome = ops::publish(
        &ctx,
        Some(&session_connect),
        None,
        "deploy",
        true,
        None,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    let data = match outcome {
        ops::PublishOutcome::Proposed(d) => d,
        other => panic!("the genesis propose opens a proposal: {other:?}"),
    };
    assert!(
        data.rewrite_pending.is_some(),
        "the failed rewrite rides the proposal receipt"
    );

    // The manifest heals with the PATH line; the update converges the transfer despite the
    // GENESIS-observed local sync.
    let canonical_src = src.canonicalize().unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{}\" = \"*\"\n",
        canonical_src.display()
    ));
    let out = ops::manifest_update(
        &ctx,
        &session_connect,
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    let line = crate::message::legacy_lines(&out.advisories)
        .into_iter()
        .find(|w| w.starts_with("GOVERNANCE_CONVERGED"))
        .unwrap_or_else(|| panic!("the converge line: {:?}", out.advisories));
    assert!(line.contains(&format!("{HOST}/{WS_NAME}/deploy")), "{line}");
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(&format!("{HOST}/{WS_NAME}/deploy")),
        "the canonical reference stands: {text}"
    );
}
