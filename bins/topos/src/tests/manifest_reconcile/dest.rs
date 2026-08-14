//! Destinations. The `dest` field as a row's frozen placement — how it grows and shrinks on the
//! next update, in both scopes — and the `-a <agent>` / `--dest <folder>` selectors on add and
//! remove, including the narrowing that subtracts exactly one and the receipt it prints.

use std::sync::{Arc, Mutex};

use topos_core::digest::FileMode;
use topos_types::requests::{
    WireChannelEntry, WireChannelIndex, WireChannelSkill, WireMe, WireProposalIndex,
    WireSkillIndex, WireSkillLog,
};
use topos_types::results::PullAction;

use crate::error::ClientError;
use crate::ops;
use crate::plane::DirectorySource;
use crate::sessions::Session;

use super::rig::*;

// =================================================================================================
// The `dest` field: a row's frozen destinations — placement, grow, shrink, both scopes.
// =================================================================================================

/// A PERSON-scope workspace row with `dest` places EXACTLY the named folders — the active
/// adapter's default dir gets nothing (detection is ignored for a dest row) — and a hand-edited
/// dest change converges on the next update: a NEW entry installs there (disclosed as the
/// install it is), a REMOVED entry's copy retires through the park-then-verify rail.
#[test]
fn a_dest_row_freezes_placement_and_grows_and_shrinks_on_the_next_update() {
    let rig = Rig::new("dest-freeze");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-a\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    let a = rig.home.0.join("dest-a/deploy");
    assert!(a.join("SKILL.md").exists(), "{:?}", out.data.skills);
    assert!(
        !rig.skills().join("deploy").exists(),
        "the default adapter dir gets nothing — the row froze its destinations"
    );
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::Installed, "{row:?}");
    assert!(
        row.destinations
            .iter()
            .any(|d| d.ends_with("dest-a/deploy")),
        "{row:?}"
    );

    // GROW: a hand-added entry installs there on the next update, said as an install.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-a\", \"~/dest-b\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    let b = rig.home.0.join("dest-b/deploy");
    assert!(b.join("SKILL.md").exists(), "{:?}", out.data.skills);
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::Installed, "{row:?}");
    assert!(
        row.destinations
            .iter()
            .any(|d| d.ends_with("dest-b/deploy")),
        "the grown destination is the one named: {row:?}"
    );

    // SHRINK: the dropped entry's copy retires (park-then-verify); the kept entry stands.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-b\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    assert!(!a.exists(), "the un-named copy left: {:?}", out.data.skills);
    assert!(b.join("SKILL.md").exists(), "the named copy stands");
    let removed = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert!(
        removed
            .destinations
            .iter()
            .any(|d| d.ends_with("dest-a/deploy")),
        "{removed:?}"
    );
}

/// The `"*"` token in a `dest` array is the row's DEFAULT REACH, answered at plan time on every
/// run: the named entry lands where it always did, the default half lands where a row with no
/// `dest` would, and an agent installed AFTER the row was written is reached without the row
/// changing. Dropping the token narrows to the named entry alone, which is what a dest row means.
#[test]
fn the_default_reach_token_places_beside_the_named_entry_and_keeps_answering_detection() {
    let rig = Rig::new("dest-token");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let starred =
        format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"*\", \"~/dest-a\"] }}\n");
    rig.write_global(&starred);
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let named = rig.home.0.join("dest-a/deploy");
    assert!(named.join("SKILL.md").exists(), "the named entry landed");
    assert!(
        rig.skills().join("deploy/SKILL.md").exists(),
        "and so did the default reach — the token is not a narrowing"
    );

    // A NEW agent appears. The row is untouched; the token answers detection again.
    std::fs::create_dir_all(rig.home.0.join(".codex")).unwrap();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        rig.home.0.join(".codex/skills/deploy/SKILL.md").exists(),
        "the newly detected agent is reached: {:?}",
        out.data.skills
    );
    assert!(named.join("SKILL.md").exists(), "the named entry stands");

    // Dropping the token narrows the row to what it names — the ordinary dest shrink.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-a\"] }}\n"
    ));
    sweep(&ctx, &plane, &dir);
    assert!(named.join("SKILL.md").exists());
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert!(!rig.skills().join("deploy").exists());
}

/// A dest SHRINK meets the keep-edited-in-place discipline: an EDITED copy at the dropped
/// destination is snapshotted and KEPT on disk (its record released), never swept up — exactly
/// like the feed-drop receipts.
#[test]
fn a_dest_shrink_keeps_an_edited_copy_in_place() {
    let rig = Rig::new("dest-keep");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-a\", \"~/dest-b\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let a = rig.home.0.join("dest-a/deploy");
    let b = rig.home.0.join("dest-b/deploy");
    assert!(a.join("SKILL.md").exists() && b.join("SKILL.md").exists());

    // The person edits the copy the shrink is about to drop.
    std::fs::write(a.join("SKILL.md"), b"# my edit\n").unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-b\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read(a.join("SKILL.md")).unwrap(),
        b"# my edit\n",
        "the edited copy is the person's own — kept in place"
    );
    let removed = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy" && s.action == PullAction::Removed)
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert!(
        removed.kept.iter().any(|d| d.ends_with("dest-a/deploy")),
        "the kept copy is named: {removed:?}"
    );
}

/// The SAME loss lead over FOLDERS. A hand-narrowed skill row retires the copies it stopped
/// naming, and the receipt says what the bundle still delivers to before it lists them — a shared
/// folder names no single agent, so the folder IS the line.
#[test]
fn a_hand_narrowed_skill_row_leads_the_receipt_with_the_copies_it_retired() {
    let rig = Rig::new("dest-narrow-lead");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-a\", \"~/dest-b\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    assert!(rig.home.0.join("dest-a/deploy/SKILL.md").exists());
    assert!(rig.home.0.join("dest-b/deploy/SKILL.md").exists());

    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/dest-b\"] }}\n"
    ));
    let out = sweep(&ctx, &plane, &dir);
    let receipt = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert!(
        receipt.contains(
            "deploy now delivers only to ~/dest-b/deploy — removed its copies from:\n  \
             ~/dest-a/deploy\n"
        ),
        "{receipt}"
    );
    assert!(
        !receipt.contains("- @acme/deploy   removed"),
        "the loss is said once: {receipt}"
    );
}

/// A PROJECT-scope dest row places inside the checkout at the named relative folder — the same
/// dest mechanism that replaced the old path-override seam.
#[test]
fn a_project_dest_row_places_inside_the_checkout_at_the_named_folder() {
    let rig = Rig::new("dest-proj");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let proj = project(
        "dest-proj-co",
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"tools/ai\"] }}\n"),
    );
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        proj.0.join("tools/ai/deploy/SKILL.md").exists(),
        "{:?} / {:?}",
        out.data.skills,
        out.warnings
    );
    assert!(
        !proj.0.join(".claude/skills/deploy").exists(),
        "the default project dirs get nothing — the row froze its destination"
    );
}

/// A REPO-SET row's dest aims every member's converge slot at the named folder — the dest-keyed
/// generalization of the old per-slug slots.
#[test]
fn a_repo_set_rows_dest_aims_its_members_at_the_named_folder() {
    let rig = Rig::new("dest-set");
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
    // The row is written by hand (demand is the row); the sweep converges it.
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        "o/r",
        true,
        true,
        &Default::default(),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("--yes applies"),
    }
    // Re-aim the whole set: the row's dest replaces the default dir for every member.
    rig.write_global("[bundles]\n\"github.com/o/r\" = { dest = [\"~/team-skills\"] }\n");
    git.serve(build_repo_targz(
        "o-r-bbbbbbbbbbbb2",
        &[
            ("skills/alpha/SKILL.md", b"# alpha v2\n"),
            ("skills/beta/SKILL.md", b"# beta v2\n"),
        ],
    ));
    let out = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        Some(&git as &dyn crate::git_source::GitTarballSource),
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap();
    for name in ["alpha", "beta"] {
        assert_eq!(
            std::fs::read(rig.home.0.join("team-skills").join(name).join("SKILL.md")).unwrap(),
            format!("# {name} v2\n").into_bytes(),
            "{name}: {:?} / {:?}",
            out.data.skills,
            out.warnings
        );
    }
}

// =================================================================================================
// `-a <agent>` / `--dest <folder>` — destinations on add and remove (the dest selectors).
// =================================================================================================

/// `topos add -g @ws/skill -a codex`: the row freezes to codex's folder, the copy lands there in
/// the same invocation, and the receipt is the FINAL destination shape, byte for byte.
#[test]
fn an_agent_selected_add_freezes_the_row_and_prints_the_destination_receipt() {
    let (rig, plane, dir, v) = add_rig("dest-add");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The row is frozen to exactly the selected destination.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#""acme.test/eng/deploy" = { dest = ["~/.codex/skills"] }"#),
        "{text}"
    );
    // The copy landed there in this invocation.
    assert!(
        rig.home.0.join(".codex/skills/deploy/SKILL.md").exists(),
        "the narrowed update lands the copy at the selected folder"
    );
    assert_eq!(data.dest, vec!["~/.codex/skills".to_owned()]);
    // The FINAL receipt copy, byte for byte.
    assert_eq!(
        crate::render::add_tty(&data),
        "+ @eng/deploy   installed (~/.codex/skills)\n\
         source: acme.test/eng/deploy\n\
         (undo: topos remove -g deploy)"
    );
}

/// The same `-a` add when the plane cannot DELIVER (the delivery lane down, no bytes servable):
/// the row lands (the durable demand), but no copy does — so the receipt must NOT claim
/// `installed (…)`; the row-recorded receipt prints instead, with the next-sweep path disclosed.
#[test]
fn an_agent_selected_add_whose_delivery_fails_does_not_claim_installed() {
    let rig = Rig::new("dest-add-offline");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    // The catalog answers (the add resolves the name), but the plane serves NOTHING — no
    // delivery, no version bytes.
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    plane.serve_unreachable();
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The row froze to the destination regardless — the demand is durably recorded.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#""acme.test/eng/deploy" = { dest = ["~/.codex/skills"] }"#),
        "{text}"
    );
    // No copy landed — and the receipt does not claim one did.
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert!(data.dest.is_empty(), "{:?}", data.dest);
    assert_eq!(data.display, None);
    let tty = crate::render::add_tty(&data);
    assert!(!tty.contains("installed"), "{tty}");
    assert!(
        tty.contains("land on the next `topos update`"),
        "the next-sweep path is stated: {tty}"
    );
}

/// A same-named LOCAL row's clean report must not prove a workspace bundle's bytes: the adopted
/// `deploy` reconciles up-to-date in the same scope, but the workspace delivery failed — so the
/// receipt must NOT claim `installed (…)` on the strength of the other row.
#[test]
fn a_same_named_local_rows_report_does_not_prove_the_workspace_add() {
    let rig = Rig::new("dest-add-foreign-proof");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    // The catalog answers (the add resolves the name), but the plane serves NOTHING — no
    // delivery, no version bytes.
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    plane.serve_unreachable();
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    // A same-named ADOPTED folder already stands in the machine recipe.
    let local = rig.home.0.join("src/deploy");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::write(local.join("SKILL.md"), b"# mine\n").unwrap();
    rig.write_global(&format!("[bundles]\n\"{}\" = \"*\"\n", local.display()));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // No workspace copy landed — the local row's up-to-date report is not this bundle's proof.
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert!(data.dest.is_empty(), "{:?}", data.dest);
    assert_eq!(data.display, None);
    let tty = crate::render::add_tty(&data);
    assert!(!tty.contains("installed"), "{tty}");
    assert!(
        tty.contains("land on the next `topos update`"),
        "the next-sweep path is stated: {tty}"
    );
}

/// A directory whose channel index answers ONCE (the add's resolve) and then fails — the
/// transport dropping between the resolve and the SAME invocation's delivering reconcile.
#[derive(Clone)]
struct ChannelsDropAfterFirst {
    inner: FakeDirectory,
    answered: Arc<Mutex<u32>>,
}
impl DirectorySource for ChannelsDropAfterFirst {
    fn me(&self, ws: &str) -> Result<WireMe, ClientError> {
        self.inner.me(ws)
    }
    fn channels_index(&self, ws: &str) -> Result<WireChannelIndex, ClientError> {
        let mut n = self.answered.lock().unwrap();
        *n += 1;
        if *n > 1 {
            return Err(ClientError::Plane("directory unreachable".into()));
        }
        self.inner.channels_index(ws)
    }
    fn skills_index(&self, ws: &str) -> Result<WireSkillIndex, ClientError> {
        self.inner.skills_index(ws)
    }
    fn proposals_index(&self, ws: &str) -> Result<WireProposalIndex, ClientError> {
        self.inner.proposals_index(ws)
    }
    fn skill_log(&self, ws: &str, s: &str) -> Result<WireSkillLog, ClientError> {
        self.inner.skill_log(ws, s)
    }
    fn protect_skill(&self, ws: &str, s: &str, l: &str) -> Result<(), ClientError> {
        self.inner.protect_skill(ws, s, l)
    }
    fn protect_channel(&self, ws: &str, c: &str, l: &str) -> Result<(), ClientError> {
        self.inner.protect_channel(ws, c, l)
    }
}

/// A channel add with `-a` whose expansion FAILS in the delivering reconcile must not borrow an
/// unrelated feed bundle's clean row as its proof: the same untargeted sweep reconciles the
/// feed's `other` up-to-date, but THIS channel proved nothing — the row lands (the durable
/// demand), the receipt does not claim `installed (…)`, and the next-sweep path prints.
#[test]
fn a_channel_add_whose_expansion_fails_does_not_borrow_the_feeds_proof() {
    let rig = Rig::new("dest-add-channel-fail");
    rig.seed_session();
    let v = one_file(b"# other\n");
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new()))).with_version("s_other", &v);
    plane.serves(vec![delivered("s_other", "other", &v)]);
    let inner = FakeDirectory::new(
        vec![catalog_entry("s_other", "other", &v)],
        vec![WireChannelEntry {
            name: "backend".into(),
            mode: "open".into(),
            builtin: false,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_other".into(),
                name: "other".into(),
            }],
        }],
    );
    rig.seed_feed();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    // The feed bundle stands CURRENT before the add — the unrelated up-to-date row the gate
    // must not borrow.
    let primed = sweep_scoped(&ctx, &plane, &inner, ops::UpdateScope::Machine);
    assert!(
        primed
            .data
            .skills
            .iter()
            .any(|s| s.skill == "other" && s.action == PullAction::Installed),
        "the fixture needs the feed delivering: {:?}",
        primed.data.skills
    );
    let wrapper = ChannelsDropAfterFirst {
        inner: inner.clone(),
        answered: Arc::new(Mutex::new(0)),
    };
    let session_connect = move |_s: &Session| ops::SessionTransports {
        plane: Box::new(plane.clone()),
        directory: Box::new(wrapper.clone()),
        contribute: Box::new(NoContribute),
        governance: Box::new(NoGovernance),
    };
    let data = match ops::add_reference(
        &ctx,
        &session_connect,
        None,
        &format!("@{WS_NAME}/channels/backend"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The row froze to the destination regardless — the demand is durably recorded.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#""acme.test/eng/channels/backend" = { dest = ["~/.codex/skills"] }"#),
        "{text}"
    );
    // The channel expansion failed — nothing of ITS proved, whatever the feed reconciled.
    assert!(data.dest.is_empty(), "{:?}", data.dest);
    assert_eq!(data.display, None);
    let tty = crate::render::add_tty(&data);
    assert!(!tty.contains("installed"), "{tty}");
    assert!(
        tty.contains("land on the next `topos update`"),
        "the next-sweep path is stated: {tty}"
    );
}

/// A re-add whose own reconcile MERGES a local draft onto the new current landed bytes — the
/// merged draft stands placed at the destination, so the receipt's `installed (…)` claim stands
/// (a merged row is not a failed placement).
#[test]
fn an_agent_selected_add_whose_reconcile_merges_keeps_the_destination_receipt() {
    let rig = Rig::new("dest-add-merged");
    rig.seed_session();
    let v1 = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n\nsteps\n")]);
    let v2 = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy v2\n\nsteps\n")]);
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())))
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir1 = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir1),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    assert_eq!(data.dest, vec!["~/.codex/skills".to_owned()]);
    let placed = rig.home.0.join(".codex/skills/deploy/SKILL.md");
    // The person edits their copy (a draft), and the team moves current — a real pointer move.
    std::fs::write(&placed, b"# deploy\n\nsteps\nmine\n").unwrap();
    let mut moved = delivered("s_deploy", "deploy", &v2);
    moved.generation = 2;
    plane.serves(vec![moved]);
    let mut moved_cat = catalog_entry("s_deploy", "deploy", &v2);
    moved_cat.generation = 2;
    let dir2 = FakeDirectory::new(vec![moved_cat], Vec::new());
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir2),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The merge landed BOTH edits at the destination — the receipt may say so.
    let text = std::fs::read_to_string(&placed).unwrap();
    assert!(
        text.contains("# deploy v2") && text.contains("mine"),
        "the merged draft stands placed: {text}"
    );
    assert_eq!(data.dest, vec!["~/.codex/skills".to_owned()]);
    let tty = crate::render::add_tty(&data);
    assert!(tty.contains("installed (~/.codex/skills)"), "{tty}");
    assert!(!tty.contains("could not be placed"), "{tty}");
}

/// A re-add whose own reconcile ends in a merge CONFLICT clears the destination claim — but the
/// note must not promise the next update (a standing conflict is not a transient miss): it says
/// what the row reports and where the detail lives.
#[test]
fn an_agent_selected_add_whose_reconcile_conflicts_names_the_standing_state() {
    let rig = Rig::new("dest-add-conflicted");
    rig.seed_session();
    let v1 = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy\n\nsteps\n")]);
    let v2 = mk_version(&[("SKILL.md", FileMode::Regular, b"# deploy THEIRS\n\nsteps\n")]);
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())))
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir1 = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    match ops::add_reference(
        &ctx,
        &connect(&plane, &dir1),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(_) => {}
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    }
    let placed = rig.home.0.join(".codex/skills/deploy/SKILL.md");
    // BOTH sides edit the same line — the three-way merge cannot resolve it.
    std::fs::write(&placed, b"# deploy MINE\n\nsteps\n").unwrap();
    let mut moved = delivered("s_deploy", "deploy", &v2);
    moved.generation = 2;
    plane.serves(vec![moved]);
    let mut moved_cat = catalog_entry("s_deploy", "deploy", &v2);
    moved_cat.generation = 2;
    let dir2 = FakeDirectory::new(vec![moved_cat], Vec::new());
    let data = match ops::add_reference(
        &ctx,
        &connect(&plane, &dir2),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        ops::AddRefOutcome::Described { .. } => panic!("a workspace reference applies"),
    };
    // The conflicted state is not a placed install — the destination claim clears …
    assert!(data.dest.is_empty(), "{:?}", data.dest);
    assert_eq!(data.display, None);
    // … and the note names the STANDING state instead of promising the next update.
    let note = data.note.clone().unwrap_or_default();
    assert!(note.contains("'deploy' reports conflicted here"), "{note}");
    assert!(
        note.contains("`topos list deploy` has the detail"),
        "{note}"
    );
    assert!(!note.contains("land on the next"), "{note}");
}

/// `topos remove -g <skill> -a codex` over a three-destination row: the codex folder is
/// subtracted and its copy leaves; the row keeps the rest; the receipt is the FINAL narrow
/// shape, byte for byte, and its undo re-adds exactly what left.
#[test]
fn a_narrowed_remove_subtracts_one_destination_and_prints_the_final_receipt() {
    let (rig, plane, dir, v) = add_rig("dest-narrow");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\", \"~/.cursor/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    for agent in [".claude", ".codex", ".cursor"] {
        assert!(
            rig.home
                .0
                .join(agent)
                .join("skills/deploy/SKILL.md")
                .exists(),
            "{agent}: the dest row installed everywhere it names"
        );
    }
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a narrow applies immediately: {other:?}"),
    };
    // The row keeps the remaining two destinations; the codex copy is gone, the others stay.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#"dest = ["~/.claude/skills", "~/.cursor/skills"]"#),
        "{text}"
    );
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert!(rig.home.0.join(".claude/skills/deploy/SKILL.md").exists());
    // The receipt facts, typed.
    let u = &data.uninstalled[0];
    assert_eq!(u.name, format!("@{WS_NAME}/deploy"));
    assert_eq!(u.destinations, vec!["~/.codex/skills".to_owned()]);
    assert_eq!(u.remaining, Some(2));
    // The FINAL receipt copy, byte for byte.
    assert_eq!(
        crate::render::remove_applied_tty(&data),
        "- @eng/deploy   removed (~/.codex/skills) — 2 folders remain\n(undo: topos add -g \
         @eng/deploy -a codex)"
    );
}

/// The narrow when the reconcile cannot move the copy (a hand-written dest row on a machine the
/// plane never delivered to, the plane unreachable): the row's subtraction lands (the durable
/// edit), but NOTHING was uninstalled — so the receipt must NOT claim `removed (…)`; the row-edit
/// line prints with the next-sweep path instead.
#[test]
fn an_offline_narrow_does_not_claim_the_copy_left() {
    let rig = Rig::new("dest-narrow-offline");
    rig.seed_session();
    // The plane serves nothing at all — no delivery snapshot, no version bytes, empty catalog.
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    plane.serve_unreachable();
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\", \"~/.cursor/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a narrow applies immediately: {other:?}"),
    };
    // The row edit landed: the manifest keeps the remaining two destinations.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#"dest = ["~/.claude/skills", "~/.cursor/skills"]"#),
        "{text}"
    );
    // Nothing was uninstalled — and the receipt does not claim anything was.
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
    let tty = crate::render::remove_applied_tty(&data);
    assert!(!tty.contains("removed ("), "{tty}");
    assert!(
        tty.contains("leaves on the next `topos update`"),
        "the next-sweep path is stated: {tty}"
    );
    assert!(
        tty.contains("the row keeps 2 folders"),
        "the remaining count survives the fallback: {tty}"
    );
}

/// Narrowing a dest-less row with NO recorded copies refuses with the honest zero-state — never
/// the old trailing-off "its destinations here are " with nothing after it.
#[test]
fn narrowing_a_row_with_no_recorded_copies_refuses_with_the_zero_state() {
    let (rig, plane, dir, _v) = add_rig("dest-narrow-zero");
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("'deploy' has no recorded copies in this scope yet"),
        "{msg}"
    );
    assert!(msg.contains("run `topos update` first"), "{msg}");
    assert!(msg.contains("topos remove -g deploy"), "{msg}");
    // The manifest row is untouched — the refusal changed nothing.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("deploy"), "{text}");
}

/// The same zero-state over a FEED-delivered skill (no row anywhere): "its row was added" would
/// be a fabrication — there is no row to remove. The honest variant names what stands (the feed
/// delivers it) and the whole-machine end that actually exists (the `"off"` switch).
#[test]
fn narrowing_a_feed_delivered_skill_with_no_copies_names_the_off_switch() {
    let (rig, plane, dir, v) = add_rig("dest-narrow-feed-zero");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.seed_feed();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(
        msg.contains("'deploy' is feed-delivered and has no recorded copies in this scope yet"),
        "{msg}"
    );
    assert!(msg.contains("run `topos update` first"), "{msg}");
    assert!(
        msg.contains("`topos remove -g deploy` to switch it off entirely"),
        "{msg}"
    );
    assert!(!msg.contains("its row was added"), "{msg}");
    // The manifest is untouched — no `"off"` switch was written by the refusal.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("off"), "{text}");
}

/// Removing the LAST destination removes the whole row — a row is never left bare by
/// subtraction (bare means default reach and would resurrect copies).
#[test]
fn removing_the_last_destination_removes_the_row_entirely() {
    let (rig, plane, dir, v) = add_rig("dest-last");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join(".codex/skills/deploy/SKILL.md").exists());
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("deploy"), "the whole row is gone: {text}");
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    // The whole-row undo reconstructs the destination that left.
    assert_eq!(
        data.undo,
        vec!["topos", "add", "-g", "@eng/deploy", "-a", "codex"]
    );
}

/// A bare `remove -g <skill>` of a dest row removes the row AND every copy it placed, in the
/// same invocation — the FINAL whole-row receipt, byte for byte, with the `-a` reconstruction.
#[test]
fn a_whole_row_remove_uninstalls_eagerly_and_reconstructs_the_undo() {
    let (rig, plane, dir, v) = add_rig("dest-whole");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"~/.claude/skills\", \
         \"~/.codex/skills\"] }}\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
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
    assert!(!rig.home.0.join(".claude/skills/deploy").exists());
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    assert_eq!(
        data.undo,
        vec![
            "topos",
            "add",
            "-g",
            "@eng/deploy",
            "-a",
            "claude-code",
            "-a",
            "codex"
        ]
    );
    // The FINAL receipt copy, byte for byte.
    assert_eq!(
        crate::render::remove_applied_tty(&data),
        "- @eng/deploy   removed (2 folders)\n(undo: topos add -g @eng/deploy -a claude-code -a \
         codex)"
    );
}

/// Narrowing a row that names NO destinations first materializes the CURRENT resolved set —
/// the result is a frozen row of the remaining destinations.
#[test]
fn narrowing_a_no_dest_row_freezes_the_remainder() {
    let (rig, plane, dir, v) = add_rig("dest-materialize");
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    // codex DETECTED (its home dir exists) so the default plan includes its native folder.
    std::fs::create_dir_all(rig.home.0.join(".codex")).unwrap();
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(rig.home.0.join(".codex/skills/deploy/SKILL.md").exists());
    let data = match ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["deploy".into()],
        None,
        false,
        &sel(&["codex"], &[]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("{other:?}"),
    };
    // The codex copy left; the row is now FROZEN to the remaining destination(s).
    assert!(!rig.home.0.join(".codex/skills/deploy").exists());
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(text.contains("dest = ["), "the remainder is frozen: {text}");
    assert!(!text.contains("~/.codex/skills"), "{text}");
    // The materialize disclosure rides the receipt.
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(note.contains("the row reached every agent"), "{note}");
    let u = &data.uninstalled[0];
    assert_eq!(u.destinations, vec!["~/.codex/skills".to_owned()]);
}

/// A row the subtraction leaves standing for nothing but its DEFAULT REACH says exactly what the
/// set line beside it already says — so it goes, and the file lands back where the `-a` add found
/// it. Set coverage is PROVEN from the delivery cache; without a set the row survives as the plain
/// `"*"` row, because dropping the only demand is not a narrowing.
#[test]
fn a_row_left_at_its_default_reach_goes_when_a_set_still_delivers_it_and_stays_when_none_does() {
    let rig = Rig::new("dest-collapse");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new()))).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &v)],
        vec![WireChannelEntry {
            name: "everyone".into(),
            mode: "open".into(),
            builtin: true,
            included: true,
            skills: vec![WireChannelSkill {
                skill_id: "s_deploy".into(),
                name: "deploy".into(),
            }],
        }],
    );
    // The CHANNEL line delivers it; the explicit row beside it is what the `-a` add extends.
    let channel = format!("[bundles]\n\"{HOST}/{WS_NAME}/channels/everyone\" = \"*\"\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    rig.write_global(&format!("{channel}\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let manifest = rig.layout().home().join(crate::manifest::MANIFEST_FILE);

    let added = applied_dest_add(
        &ctx,
        &plane,
        &dir,
        &format!("{HOST}/{WS_NAME}/deploy"),
        &["~/dest-x"],
    );
    assert!(added.dest_change.is_some_and(|c| c.default_reach));
    let subtract = |ctx: &crate::ctx::Ctx<'_>| match ops::remove_global(
        ctx,
        &connect(&plane, &dir),
        // The canonical reference: a bare name is ambiguous while the channel also carries it.
        &[format!("{HOST}/{WS_NAME}/deploy")],
        None,
        false,
        &sel(&[], &["~/dest-x"]),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("a clean subtraction applies immediately: {other:?}"),
    };
    subtract(&ctx);
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        format!("{channel}"),
        "the redundant row went — the channel line already says what it said"
    );

    // WITHOUT the set line, the same subtraction keeps the row: it is the only demand there is.
    let alone = format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n");
    rig.write_global(&alone);
    applied_dest_add(
        &ctx,
        &plane,
        &dir,
        &format!("{HOST}/{WS_NAME}/deploy"),
        &["~/dest-x"],
    );
    subtract(&ctx);
    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        alone,
        "no set covers it, so the row stays — spelled the way the add found it"
    );
}

/// Dropping an explicit row whose bundle the FEED still delivers: the row edit lands, the copies
/// CORRECTLY stay (the feed still demands them) — and the receipt says exactly that, with the off
/// switch, instead of the stock "the copies it placed leave this machine now" lie.
#[test]
fn removing_a_row_the_feed_still_delivers_says_the_copies_stay() {
    let rig = Rig::new("row-feed-stays");
    rig.seed_session();
    // BOTH the feed line and an explicit row for the same bundle.
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}\" = \"*\"\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"
    ));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let placed = rig.skills().join("deploy/SKILL.md");
    assert!(placed.exists());

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
        other => panic!("a clean row drop applies immediately: {other:?}"),
    };
    // The row is gone; the feed line survives; the copy stays because the feed still demands it.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(!text.contains("/deploy\""), "the row left: {text}");
    assert!(
        text.contains(&format!("\"{HOST}/{WS_NAME}\"")),
        "the feed line stands: {text}"
    );
    assert!(placed.exists(), "the feed-demanded copy stays in place");
    assert!(data.uninstalled.is_empty(), "{:?}", data.uninstalled);
    // The receipt: the honest copies-stay line with the off switch — never the copies-leave claim.
    let note = data.items[0].note.clone().unwrap_or_default();
    assert!(
        note.contains(&format!("{WS_NAME}'s feed still delivers it here")),
        "{note}"
    );
    assert!(
        note.contains("`topos remove -g deploy` switches it off"),
        "{note}"
    );
    let tty = crate::render::remove_applied_tty(&data);
    assert!(tty.contains("the copies stay in place"), "{tty}");
    assert!(!tty.contains("leave this machine now"), "{tty}");
}

/// The unknown-agent refusal: the FINAL copy shape — real registry slugs, alphabetical, ellipsis
/// past the handful — and the TTY closes with `nothing changed`.
#[test]
fn an_unknown_agent_refuses_with_the_registry_list_and_nothing_changed() {
    let (rig, plane, dir, _v) = add_rig("dest-unknown");
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}/deploy"),
        true,
        false,
        &sel(&["codx"], &[]),
        None,
    )
    .unwrap_err();
    assert_eq!(err.code(), "UNKNOWN_AGENT");
    let mut slugs: Vec<&str> = topos_harness::registry::known_harnesses()
        .iter()
        .map(|h| h.slug)
        .collect();
    slugs.sort_unstable();
    assert_eq!(
        crate::render::err_tty(&err),
        format!(
            "error: unknown agent: codx — known: {}, …\nnothing changed",
            slugs[..4].join(", ")
        )
    );
    // Nothing was written.
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert_eq!(text, "[bundles]\n");
}

/// A selection over a whole FEED refuses (a feed reaches every agent by nature), teaching the
/// narrower row — and the TTY closes with `nothing changed`.
#[test]
fn a_selection_over_a_feed_refuses_whole() {
    let (rig, plane, dir, _v) = add_rig("dest-feed");
    rig.seed_feed();
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}"),
        true,
        false,
        &sel(&["codex"], &[]),
        None,
    )
    .unwrap_err();
    assert_eq!(err.code(), "INVALID_ARGUMENT");
    let tty = crate::render::err_tty(&err);
    assert!(tty.contains("whole feed"), "{tty}");
    assert!(tty.ends_with("nothing changed"), "{tty}");
}

/// A feed add states its one fact ONCE. The receipt's closing sentence is where "this machine now
/// takes whatever <ws> gives you" belongs, so the row write adds no `note:` line saying the same
/// thing one line above it. The IDEMPOTENT re-add keeps its own note — "already adopting …" is a
/// fact that sentence does not carry.
#[test]
fn a_feed_add_states_what_this_machine_now_takes_exactly_once() {
    let (rig, plane, dir, _v) = add_rig("feed-once");
    rig.write_global("[bundles]\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let add = || match ops::add_reference(
        &ctx,
        &connect(&plane, &dir),
        None,
        &format!("@{WS_NAME}"),
        true,
        false,
        &Default::default(),
        None,
    )
    .unwrap()
    {
        ops::AddRefOutcome::Applied(d) => *d,
        other => panic!("a feed row applies immediately: {other:?}"),
    };

    let data = add();
    assert!(
        data.note.is_none(),
        "the closing sentence is the ONE place this fact is stated: {:?}",
        data.note
    );
    let tty = crate::render::add_tty(&data);
    assert_eq!(
        tty.matches("takes whatever").count(),
        1,
        "the same fact, twice: {tty}"
    );
    assert!(
        tty.contains(&format!(
            "This machine now takes whatever {WS_NAME} gives you"
        )),
        "{tty}"
    );

    // The re-add rewrites nothing — and says so; the closing sentence still prints once.
    let data = add();
    let note = data.note.as_deref().expect("the no-op discloses itself");
    assert!(note.contains("already adopting"), "{note}");
    assert!(note.contains("nothing changed"), "{note}");
    let tty = crate::render::add_tty(&data);
    assert_eq!(tty.matches("takes whatever").count(), 1, "{tty}");
}

/// The shared-copy refusal, byte for byte: subtraction cannot narrow one shared folder, and the
/// two ways out print as aligned command lines (the `-a` list = the covered agents that read the
/// shared copy, minus the one being removed).
#[test]
fn a_shared_only_copy_refuses_per_agent_removal_with_both_ways_out() {
    let (rig, plane, _dir, v) = add_rig("dest-shared");
    let v2 = one_file(b"# coolify\n");
    let plane = plane.with_version("s_cool", &v2);
    plane.serves(vec![
        delivered("s_deploy", "deploy", &v),
        delivered("s_cool", "coolify-deploy", &v2),
    ]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &v),
            catalog_entry("s_cool", "coolify-deploy", &v2),
        ],
        Vec::new(),
    );
    // amp + cline DETECTED — both read the shared `~/.agents/skills` dir per the coverage table,
    // so the machine plan places ONE shared copy.
    std::fs::create_dir_all(rig.home.0.join(".config/amp")).unwrap();
    std::fs::create_dir_all(rig.home.0.join(".cline")).unwrap();
    rig.write_global(&format!(
        "[bundles]\n\"{HOST}/{WS_NAME}/coolify-deploy\" = \"*\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        rig.home
            .0
            .join(".agents/skills/coolify-deploy/SKILL.md")
            .exists(),
        "the covered agents share one copy"
    );
    let err = ops::remove_global(
        &ctx,
        &connect(&plane, &dir),
        &["coolify-deploy".into()],
        None,
        false,
        &sel(&["amp"], &[]),
    )
    .unwrap_err();
    assert_eq!(err.code(), "SHARED_COPY_ONLY");
    // The FINAL copy shape, byte for byte: the statement, then the two aligned ways out.
    assert_eq!(
        crate::render::err_tty(&err),
        "coolify-deploy has no amp-only copy — its one copy is \
         ~/.agents/skills/coolify-deploy, which several agents read\n  topos remove -g \
         coolify-deploy              remove it for every agent\n  topos add -g \
         @eng/coolify-deploy -a cline   keep it per-agent instead, then re-run"
    );
    // No hint block repeats the two commands (they are the copy) — but the agent surface gets
    // BOTH ways out as structured next actions, byte-identical argvs.
    assert!(crate::render::err_hint_tty("remove", &[], &err).is_none());
    let actions = crate::render::next_actions("remove", &[], &err);
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert_eq!(
        actions[0].argv,
        vec!["topos", "remove", "-g", "coolify-deploy"]
    );
    assert_eq!(
        actions[1].argv,
        vec!["topos", "add", "-g", "@eng/coolify-deploy", "-a", "cline"]
    );
}

/// A forge import unions `-a` and `--dest`: the row records BOTH destinations, and the member
/// lands at each in the same apply.
#[test]
fn a_forge_import_unions_agent_and_dest_selectors() {
    let (rig, plane, dir, _v) = add_rig("dest-forge-union");
    rig.write_global("[bundles]\n");
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
        &["codex".to_owned()],
        &["~/team-skills".to_owned()],
        true,
        true,
    )
    .unwrap()
    {
        ops::AddManyOutcome::Applied(items) => {
            assert_eq!(items.len(), 2, "one landing per destination slot");
            assert_eq!(
                items[0].dest,
                vec!["~/.codex/skills".to_owned(), "~/team-skills".to_owned()]
            );
        }
        ops::AddManyOutcome::Described { .. } => panic!("--yes applies"),
    }
    assert!(
        rig.home.0.join(".codex/skills/deploy/SKILL.md").exists(),
        "the agent slot landed"
    );
    assert!(
        rig.home.0.join("team-skills/deploy/SKILL.md").exists(),
        "the literal folder landed"
    );
    let text =
        std::fs::read_to_string(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    assert!(
        text.contains(r#"dest = ["~/.codex/skills", "~/team-skills"]"#),
        "the union rides the row: {text}"
    );
}

/// A LOCAL adopt with a selection: the adopted folder stays the working copy, and the row's
/// `dest` places a managed COPY at the selected folder through the ordinary sweep.
#[test]
fn a_local_adopt_with_dest_places_a_copy_at_the_selected_folder() {
    let (rig, plane, dir, _v) = add_rig("dest-local-adopt");
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
    // The managed copy landed at the selected folder; the source is untouched.
    assert_eq!(
        std::fs::read(rig.home.0.join(".codex/skills/my-skill/SKILL.md")).unwrap(),
        b"# mine\n"
    );
    assert_eq!(std::fs::read(src.join("SKILL.md")).unwrap(), b"# mine\n");
    // The sweep is idempotent — a second run moves nothing new.
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        out.data
            .skills
            .iter()
            .all(|r| r.action != PullAction::Installed),
        "{:?}",
        out.data.skills
    );
}

// =================================================================================================
// A retirement marker never outlives the row that claims the record.
// =================================================================================================

/// `topos remove -g <name> --yes` — the machine row drop, applied.
fn remove_g(
    ctx: &crate::ctx::Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    name: &str,
) -> topos_types::results::RemoveData {
    match ops::remove_global(
        ctx,
        &connect(plane, dir),
        &[name.to_owned()],
        None,
        true,
        &Default::default(),
    )
    .unwrap()
    {
        ops::RemoveOutcome::Applied(data) => data,
        other => panic!("--yes applies: {other:?}"),
    }
}

/// THE RE-DEMAND, end to end. A folder adopted in place, then removed, then RETIRED by the sweep's
/// one-time resolution, comes back through `add <path> --dest <folder>` — the destination-only door
/// — and the marker goes with the row that claims it. Without that lift the row stood over a record
/// every store walker skips: the copies landed, and the NEXT remove uninstalled nothing at all
/// while its receipt said the row was gone — the person's dirs left holding files topos had
/// stopped managing, with no line saying so.
#[test]
fn a_dest_re_add_revives_the_retired_record_so_the_next_remove_uninstalls_its_copies() {
    let rig = Rig::new("dest-revive");
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
    let marker = rig.layout().published(&record).retired;

    // The row goes; the sweep that follows resolves the leftover once and retires the record.
    remove_g(&ctx, &plane, &dir, "quaggamap");
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        marker.exists(),
        "the one-time resolution retired the record"
    );

    // THE DOOR: naming the same folder with a destination re-binds THAT record, and the claim
    // lifts the marker then and there — before any sweep gets a turn.
    let scope = ops::add_scope(&ctx, true).unwrap();
    let data = ops::extend_folder_dest(&ctx, &scope, &src, &sel(&[], &["~/dest-a"]))
        .unwrap()
        .expect("the folder is still tracked here");
    assert_eq!(
        data.skill_id.as_deref(),
        Some(record.as_str()),
        "the re-add binds the record that is already there"
    );
    assert!(
        !marker.exists(),
        "a live row outranks a stale marker: the record is back on every surface"
    );

    // The copy lands where the row asks…
    sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    let copy = rig.home.0.join("dest-a/quaggamap");
    assert!(copy.join("SKILL.md").exists(), "the row's copy landed");

    // …and the removal that follows really uninstalls it, naming the folder it emptied.
    let data = remove_g(&ctx, &plane, &dir, "quaggamap");
    assert!(!copy.exists(), "the managed copy left: {data:?}");
    assert!(
        data.uninstalled
            .iter()
            .any(|u| u.destinations.iter().any(|d| d.ends_with("quaggamap"))),
        "the receipt names what it took away: {:?}",
        data.uninstalled
    );
    assert!(
        src.join("SKILL.md").exists(),
        "the adopted source is the person's own — never deleted"
    );
}

/// THE REMOVE DOOR alone, from the contradictory state and with NO sweep in between: a machine
/// already carrying a marker under a live row (the state older builds could reach) still uninstalls
/// what that row placed. The row is the demand — resolving the record through the marker instead
/// would leave the copy on disk behind a receipt that says the row is gone.
#[test]
fn a_remove_uninstalls_the_copies_of_a_rowed_record_that_still_carries_the_marker() {
    let rig = Rig::new("dest-revive-remove");
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
    let copy = rig.home.0.join("dest-a/quaggamap");
    assert!(copy.join("SKILL.md").exists());

    // The contradiction, written directly: the marker under a row that still asks for the bundle.
    let record = sid(added.skill_id.as_deref().expect("an adopt records its id"));
    crate::sidecar::retire_record(&rig.fs, &rig.layout(), &record, rig_now(&rig)).unwrap();

    let data = remove_g(&ctx, &plane, &dir, "quaggamap");
    assert!(
        !copy.exists(),
        "the row's copy left with the row: {:?}",
        data.uninstalled
    );
    assert!(
        data.uninstalled
            .iter()
            .any(|u| u.destinations.iter().any(|d| d.ends_with("quaggamap"))),
        "…and the receipt names it: {:?}",
        data.uninstalled
    );
    assert!(
        src.join("SKILL.md").exists(),
        "the adopted source is the person's own — never deleted"
    );
}
