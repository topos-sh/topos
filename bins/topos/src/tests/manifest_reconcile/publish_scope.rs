//! PUBLISH SCOPE — which copy of a two-scope bundle a publish ships, and what it says about the
//! one it left. The fixture is the live shape: the machine's feed row and a checkout's own row
//! deliver ONE bundle into TWO stores, each with its own placements, lock, and history.

use std::sync::{Arc, Mutex};

use crate::ctx::Ctx;
use crate::error::ClientError;
use crate::ops;
use crate::sessions::Session;

use super::rig::*;

// =================================================================================================
// PUBLISH SCOPE — which copy of a two-scope bundle a publish ships, and what it says about the one
// it left. The fixture is the live shape: the machine's feed row and a checkout's own row deliver
// ONE bundle into TWO stores, each with its own placements, lock, and history.
// =================================================================================================

/// The two-store fixture, converged: the checkout plus the MACHINE and PROJECT placement dirs the
/// sweep wrote.
fn both_scopes(
    tag: &str,
    rig: &Rig,
    plane: &FakePlane,
    dir: &FakeDirectory,
) -> (Scratch, std::path::PathBuf, std::path::PathBuf) {
    rig.write_global(&format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"));
    let proj = project(
        tag,
        &format!("[bundles]\n\"{HOST}/{WS_NAME}/deploy\" = \"*\"\n"),
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep_both(&ctx, plane, dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // The person row names its scope `person`; a project row names the checkout it came from.
    let placed = |person: bool| -> std::path::PathBuf {
        out.data
            .skills
            .iter()
            .find(|r| r.skill == "deploy" && (r.scope.as_deref() == Some("person")) == person)
            .and_then(|r| r.destinations.first())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| panic!("no placement (person={person}): {:?}", out.data.skills))
    };
    // The person row PRINTS the machine-truth `~/…` spelling, which is a display and not a
    // joinable path — the filesystem dir is the rig's own row-resolved skills root (the same
    // resolution the planner ran). The project row still prints a real path inside the checkout.
    let spelled = placed(true);
    assert!(
        spelled.to_string_lossy().starts_with("~/"),
        "the person destination prints the machine spelling: {spelled:?}"
    );
    let machine = rig.skills().join("deploy");
    let project_dir = placed(false);
    (proj, machine, project_dir)
}

/// The publish seams these tests share: a plane that serves the version, a catalog that names it,
/// and a contribute lane that lands whatever it is handed.
struct PublishSeams {
    plane: FakePlane,
    dir: FakeDirectory,
}
fn publish_seams(v: &Version) -> PublishSeams {
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log).with_version("s_deploy", v);
    plane.serves(vec![delivered("s_deploy", "deploy", v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", v)], Vec::new());
    PublishSeams { plane, dir }
}

/// `topos publish deploy [-g] --yes` through the real op, over the shared seams.
fn publish_at(
    ctx: &Ctx<'_>,
    seams: &PublishSeams,
    scope: ops::StoreScope,
) -> Result<ops::PublishOutcome, ClientError> {
    publish_arm(ctx, seams, scope, false)
}

/// [`publish_at`] with the arm chosen: the direct publish, or `--propose`. The contribute lane
/// matches it — a landed pointer move, or a proposal opened for review.
fn publish_arm(
    ctx: &Ctx<'_>,
    seams: &PublishSeams,
    scope: ops::StoreScope,
    propose: bool,
) -> Result<ops::PublishOutcome, ClientError> {
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(seams.plane.clone()),
        directory: Box::new(seams.dir.clone()),
        contribute: if propose {
            Box::new(OkPropose) as Box<dyn crate::plane::ContributeSource>
        } else {
            Box::new(OkPublish)
        },
        governance: Box::new(NoGovernance),
    };
    ops::publish(
        ctx,
        Some(&session_connect),
        None,
        "deploy",
        propose,
        None,
        None,
        None,
        &ops::Selection::default(),
        scope,
    )
}

/// The bare `topos publish deploy [-g]` describe, through the real op.
fn describe_at(
    ctx: &Ctx<'_>,
    seams: &PublishSeams,
    scope: ops::StoreScope,
) -> Result<ops::PublishPreview, ClientError> {
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(seams.plane.clone()),
        directory: Box::new(seams.dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    ops::publish_describe(
        ctx,
        Some(&session_connect),
        None,
        "deploy",
        false,
        None,
        None,
        &ops::Selection::default(),
        scope,
    )
}

fn landed(outcome: ops::PublishOutcome) -> topos_types::results::PublishData {
    match outcome {
        ops::PublishOutcome::Published(d) => d,
        other => panic!("the publish LANDED: {other:?}"),
    }
}

/// THE DRAFT RULE, alone: a draft is bytes ahead of the LOCK, whether or not any copy still scans
/// `Modified`. A freshly adopted skill is the other side of the same rule — its lock IS the bytes
/// it was adopted from, so nothing is ahead of it and there is nothing to publish.
#[test]
fn a_settled_copy_is_a_draft_and_a_genesis_adopt_is_not() {
    let rig = Rig::new("pubscope-draftrule");
    let src = rig.work.0.join("deploy");
    skill_source(&src, b"# deploy\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let id = sid(&ops::add(&ctx, &src)
        .unwrap()
        .skill_id
        .expect("the adopt minted a record"));
    let sp = rig.layout().published(&id);
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    assert!(
        !ops::store_has_draft(&ctx, &id, &lock),
        "a genesis adopt is at its own lock"
    );

    // The copy moves ahead and its baseline moves with it: nothing scans `Modified`, and it is
    // still unshipped work.
    let edited = b"# deploy\nedited\n";
    std::fs::write(src.join("SKILL.md"), edited).unwrap();
    let mut map = crate::doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    for state in &mut map.placement_state {
        state.materialized_sha = Some(topos_core::digest::to_hex(&one_file(edited).digest));
    }
    crate::doc::write_map(&rig.fs, &sp.map, &map).unwrap();
    assert!(ops::store_has_draft(&ctx, &id, &lock));
    assert_eq!(
        ops::store_draft(&ctx, &id, &lock).map(|d| d.dir),
        Some(src.canonicalize().unwrap()),
        "and the folder it names is the one holding those bytes"
    );
}

/// **THE LIVE DEFECT.** A project draft the last sweep SETTLED — every copy re-recorded, so each
/// one scans `Clean` at a digest the lock does not name — is still unshipped work. A bare publish
/// from inside the checkout ships THOSE bytes; it used to resolve the clean machine twin and
/// refuse with "the draft matches current" while the real draft sat there.
#[test]
fn a_settled_project_draft_is_what_a_bare_publish_ships() {
    let rig = Rig::new("pubscope-settled");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, machine_dir, project_dir) =
        both_scopes("pubscope-settled-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    // The edit, SETTLED: the draft is the copy's own recorded baseline — the shape a sweep leaves
    // behind once it has fanned a draft across the scope's copies (and the shape a landed merge
    // leaves). Every copy then scans CLEAN, at a digest the lock does not name; a draft detector
    // keyed on `Modified` alone sees nothing here at all.
    let edited = b"# deploy\nproject edit\n";
    std::fs::write(project_dir.join("SKILL.md"), edited).unwrap();
    let draft_hex = topos_core::digest::to_hex(&one_file(edited).digest);
    let playout =
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).expect("the checkout's store");
    let sp = playout.published(&sid("s_deploy"));
    let mut map = crate::doc::read_map(&rig.fs, &sp.map).unwrap().unwrap();
    for state in &mut map.placement_state {
        state.materialized_sha = Some(draft_hex.clone());
    }
    crate::doc::write_map(&rig.fs, &sp.map, &map).unwrap();
    let lock: topos_types::persisted::Lock =
        crate::doc::read_doc(&rig.fs, &sp.lock).unwrap().unwrap();
    assert_ne!(
        lock.bundle_digest, draft_hex,
        "the lock still names current"
    );
    assert_eq!(
        std::fs::read(machine_dir.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the machine twin is pristine"
    );

    let data = landed(publish_at(&ctx, &seams, ops::StoreScope::Here).unwrap());
    assert_eq!(
        data.bundle_digest,
        topos_core::digest::to_hex(&one_file(edited).digest),
        "the PROJECT draft is what shipped"
    );
    // Same scope as the one the command stood in — nothing was chosen between, so no folder line.
    assert_eq!(data.from_placement, None, "{data:?}");
    assert!(data.other_scope_draft.is_none(), "{data:?}");
}

/// The inverse: the edits are on the MACHINE and the checkout's copy is clean. A bare publish from
/// inside the checkout ships the machine copy — and both surfaces name the folder it came from,
/// with the reason, because that is the whole surprise.
#[test]
fn a_machine_draft_ships_from_inside_a_clean_checkout_and_says_so() {
    let rig = Rig::new("pubscope-machine");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, machine_dir, _project_dir) =
        both_scopes("pubscope-machine-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    let edited = b"# deploy\nmachine edit\n";
    std::fs::write(machine_dir.join("SKILL.md"), edited).unwrap();

    // The DESCRIBE names the folder and why it is that one.
    let preview = describe_at(&ctx, &seams, ops::StoreScope::Here).unwrap();
    let ops::PublishPreview::Describe(d) = preview else {
        panic!("a drafted copy previews the ship");
    };
    assert!(d.from_machine, "{d:?}");
    assert!(d.other_scope_draft.is_none(), "{d:?}");
    let from = d.from_placement.clone().expect("the folder is named");
    assert_eq!(
        from, "~/.claude/skills/deploy",
        "the folder named is the MACHINE copy's, in the machine spelling receipts print"
    );
    let text = crate::render::publish_describe_tty(&d, &["topos".into(), "publish".into()]);
    assert!(
        text.contains(&format!(
            "\n  from {from} — the machine copy carries the edits; this project's copy has none."
        )),
        "{text}"
    );
    assert!(
        text.contains("\nreview: topos diff -g deploy"),
        "the read is spelled for the copy that resolved: {text}"
    );

    // And the RECEIPT names it beside what landed.
    let data = landed(publish_at(&ctx, &seams, ops::StoreScope::Here).unwrap());
    assert_eq!(
        data.bundle_digest,
        topos_core::digest::to_hex(&one_file(edited).digest),
        "the MACHINE draft is what shipped"
    );
    assert_eq!(data.from_placement.as_deref(), Some(from.as_str()));
    assert!(data.from_machine, "{data:?}");
    let receipt = crate::render::publish_tty(&data);
    assert!(receipt.contains(&format!("(from {from})")), "{receipt}");
}

/// BOTH scopes drafted: the copy you STAND in ships, and the other is disclosed with the one
/// command that shares it. `-g` ships that other one, and the disclosure mirrors.
#[test]
fn a_double_draft_ships_where_you_stand_and_discloses_the_other() {
    let rig = Rig::new("pubscope-double");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, machine_dir, project_dir) =
        both_scopes("pubscope-double-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    let here = b"# deploy\nproject edit\n";
    let there = b"# deploy\nmachine edit\n";
    std::fs::write(project_dir.join("SKILL.md"), here).unwrap();
    std::fs::write(machine_dir.join("SKILL.md"), there).unwrap();

    let data = landed(publish_at(&ctx, &seams, ops::StoreScope::Here).unwrap());
    assert_eq!(
        data.bundle_digest,
        topos_core::digest::to_hex(&one_file(here).digest),
        "the STANDING copy ships"
    );
    let other = data
        .other_scope_draft
        .clone()
        .expect("the machine copy is disclosed");
    assert!(other.machine, "{other:?}");
    let receipt = crate::render::publish_tty(&data);
    // `current` just moved past that copy, so sharing it takes two steps — a bare publish of it
    // would be refused by the lineage fence until it is updated onto what just landed.
    assert!(
        receipt.contains(&format!(
            "\nyour machine copy in {} keeps its edits — update it onto this version, then share \
             it (topos update -g deploy, then topos publish -g deploy).",
            other.folder
        )),
        "{receipt}"
    );

    // `-g` ships the machine copy — and now the checkout's edits are the ones left behind.
    let data = landed(publish_at(&ctx, &seams, ops::StoreScope::Machine).unwrap());
    assert_eq!(
        data.bundle_digest,
        topos_core::digest::to_hex(&one_file(there).digest),
        "`-g` ships the MACHINE copy"
    );
    let other = data
        .other_scope_draft
        .clone()
        .expect("the project copy is disclosed");
    assert!(!other.machine, "{other:?}");
    assert!(
        crate::render::publish_tty(&data).contains(&format!(
            "\nyour project copy in {} keeps its edits — update it onto this version, then share \
             it (topos update deploy, then topos publish deploy).",
            other.folder
        )),
        "{}",
        crate::render::publish_tty(&data)
    );
}

/// A copy that already matches `current` is a SUCCESS with nothing to ship — and when the edits
/// are in the OTHER scope's copy, the answer points across. Both halves of the verb agree.
#[test]
fn an_already_published_copy_settles_as_a_success_and_points_at_the_other_scope() {
    let rig = Rig::new("pubscope-nochange");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, _machine_dir, project_dir) =
        both_scopes("pubscope-nochange-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    // Nothing edited anywhere: no changes, and nothing to point at.
    let quiet = match publish_at(&ctx, &seams, ops::StoreScope::Here).unwrap() {
        ops::PublishOutcome::NoChanges(d) => d,
        other => panic!("a converged copy has nothing to ship: {other:?}"),
    };
    assert_eq!(quiet.skill, "deploy");
    assert!(quiet.other_scope_draft.is_none(), "{quiet:?}");

    // The edits are in the CHECKOUT, and `-g` looks only at the machine: the success points across.
    std::fs::write(project_dir.join("SKILL.md"), b"# deploy\nproject edit\n").unwrap();
    let across = match publish_at(&ctx, &seams, ops::StoreScope::Machine).unwrap() {
        ops::PublishOutcome::NoChanges(d) => d,
        other => panic!("the machine copy matches current: {other:?}"),
    };
    let other = across
        .other_scope_draft
        .clone()
        .expect("the checkout's draft is named");
    assert!(!other.machine, "{other:?}");
    assert_eq!(
        crate::render::publish_no_changes_tty(&across),
        "'deploy' is already published — your copy matches current\nyour edits are in this \
         project's copy — share them: topos publish deploy"
    );
    // The bare DESCRIBE gives the same answer — never an apply scaffold around a no-op.
    let preview = describe_at(&ctx, &seams, ops::StoreScope::Machine).unwrap();
    let ops::PublishPreview::NoChanges(d) = preview else {
        panic!("the describe settles the same way the apply does");
    };
    assert!(d.other_scope_draft.is_some(), "{d:?}");
}

/// `-g` on a name only the CHECKOUT tracks is an honest miss — the same shape `update -g` gives.
/// Answering it from the project copy would ship bytes the flag deliberately excluded.
#[test]
fn a_g_publish_misses_a_project_only_name() {
    let rig = Rig::new("pubscope-gmiss");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let proj = project_custody("pubscope-gmiss-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    std::fs::write(
        proj.0.join(".claude/skills/deploy/SKILL.md"),
        b"# deploy\nedited\n",
    )
    .unwrap();

    for err in [
        publish_at(&ctx, &seams, ops::StoreScope::Machine).unwrap_err(),
        match describe_at(&ctx, &seams, ops::StoreScope::Machine) {
            Ok(o) => panic!("`-g` must not reach the checkout's copy: {o:?}"),
            Err(e) => e,
        },
    ] {
        assert!(
            matches!(&err, ClientError::NoSuchSkill { name } if name == "deploy"),
            "got {err:?}"
        );
    }
    // Standing where the copy IS still ships it.
    assert!(matches!(
        publish_at(&ctx, &seams, ops::StoreScope::Here).unwrap(),
        ops::PublishOutcome::Published(_)
    ));
}

/// A byte copy of a record directory under a second id — the shape a store carrying two records
/// under ONE name has, which is the shape that makes it unable to answer for that name.
fn duplicate_record(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let (src, dst) = (entry.path(), to.join(entry.file_name()));
        if entry.file_type().unwrap().is_dir() {
            duplicate_record(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// A project store that cannot answer for the name — two records under it — must not fail a `-g`
/// publish. The machine store is the authoritative answer under that flag, so the cwd chain is
/// consulted only for the disclosure, and a store with no answer simply gives none.
#[test]
fn a_poisoned_project_store_never_fails_a_g_publish() {
    let rig = Rig::new("pubscope-poison");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, machine_dir, _project_dir) =
        both_scopes("pubscope-poison-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    let edited = b"# deploy\nmachine edit\n";
    std::fs::write(machine_dir.join("SKILL.md"), edited).unwrap();

    // The checkout's store now answers "deploy" twice.
    let playout =
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).expect("the checkout's store");
    let skills = playout.skills_dir();
    duplicate_record(&skills.join("s_deploy"), &skills.join("s_deploytwin"));
    // Standing in the checkout, that ambiguity IS the answer — the store the command stands in
    // cannot say which copy it means.
    assert!(
        matches!(
            publish_at(&ctx, &seams, ops::StoreScope::Here).unwrap_err(),
            ClientError::AmbiguousName { .. }
        ),
        "the poison is real"
    );

    // `-g` ships the machine copy regardless: the broken store neither fails the resolution nor
    // changes what it ships. Both halves of the verb agree.
    let preview = describe_at(&ctx, &seams, ops::StoreScope::Machine).unwrap();
    let ops::PublishPreview::Describe(d) = preview else {
        panic!("the machine copy is drafted, so the ship previews");
    };
    assert_eq!(d.from_placement, None, "{d:?}");
    assert!(d.other_scope_draft.is_none(), "{d:?}");
    let data = landed(publish_at(&ctx, &seams, ops::StoreScope::Machine).unwrap());
    assert_eq!(
        data.bundle_digest,
        topos_core::digest::to_hex(&one_file(edited).digest),
        "the MACHINE draft is what shipped"
    );
    // No answer means no disclosure — never a guess about a copy that could not be read.
    assert_eq!(data.from_placement, None, "{data:?}");
    assert!(!data.from_machine, "{data:?}");
    assert!(data.other_scope_draft.is_none(), "{data:?}");
}

/// A `--propose` carries the SAME scope disclosure a landed publish does: a proposal ships bytes
/// too, so which folder they came from and what the other scope's copy keeps are the same two
/// facts — stated in the same words on the receipt the reader meets.
#[test]
fn a_cross_scope_proposal_carries_the_disclosure_a_landed_publish_does() {
    let rig = Rig::new("pubscope-propose");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, machine_dir, project_dir) =
        both_scopes("pubscope-propose-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    std::fs::write(machine_dir.join("SKILL.md"), b"# deploy\nmachine edit\n").unwrap();
    std::fs::write(project_dir.join("SKILL.md"), b"# deploy\nproject edit\n").unwrap();

    let data = match publish_arm(&ctx, &seams, ops::StoreScope::Machine, true).unwrap() {
        ops::PublishOutcome::Proposed(d) => d,
        other => panic!("the proposal OPENED: {other:?}"),
    };
    assert!(data.from_machine, "{data:?}");
    assert_eq!(
        data.from_placement.as_deref(),
        Some("~/.claude/skills/deploy"),
        "the folder named is the MACHINE copy's, in the machine spelling receipts print"
    );
    let other = data
        .other_scope_draft
        .clone()
        .expect("the checkout's draft is disclosed");
    assert!(!other.machine, "{other:?}");

    let receipt = crate::render::propose_tty(&data);
    assert!(
        receipt.contains("(from ~/.claude/skills/deploy) for review."),
        "{receipt}"
    );
    // The PROPOSAL's own way out: no pointer moved, so that copy is still an ordinary draft on the
    // unchanged `current` and one publish shares it.
    assert!(
        receipt.contains(&format!(
            "\nyour project copy in {} keeps its edits (topos publish deploy shares it).",
            other.folder
        )),
        "{receipt}"
    );
}

/// **IDENTITY, NEVER THE NAME.** Two scopes can track two DIFFERENT bundles under one display
/// name — the same skill followed from two workspaces, a local one beside a delivered one. Neither
/// is the other's second copy, so nothing may bind them: `-g` must not point at the checkout's
/// stranger, and a bare publish must not disclose the machine's. The rule the sibling surfaces
/// already keep (`list`'s twin, the machine-copy add disclosure), kept here.
#[test]
fn two_bundles_sharing_a_name_never_bind_across_scopes() {
    let rig = Rig::new("pubscope-identity");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, machine_dir, project_dir) =
        both_scopes("pubscope-identity-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    // The checkout's record becomes a DIFFERENT bundle that merely shares the name.
    let playout =
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).expect("the checkout's store");
    let skills = playout.skills_dir();
    duplicate_record(&skills.join("s_deploy"), &skills.join("s_deploytwin"));
    std::fs::remove_dir_all(skills.join("s_deploy")).unwrap();
    // …and it is the one carrying edits. The machine's copy is pristine.
    let stranger = b"# deploy\nanother bundle's edit\n";
    std::fs::write(project_dir.join("SKILL.md"), stranger).unwrap();
    assert_eq!(
        std::fs::read(machine_dir.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the machine twin is pristine"
    );

    // `-g`: the machine copy matches current and there is NOTHING across the way to point at —
    // the drafted folder in the checkout belongs to somebody else's bundle.
    let quiet = match publish_at(&ctx, &seams, ops::StoreScope::Machine).unwrap() {
        ops::PublishOutcome::NoChanges(d) => d,
        other => panic!("the machine copy matches current: {other:?}"),
    };
    assert!(quiet.other_scope_draft.is_none(), "{quiet:?}");
    assert_eq!(
        crate::render::publish_no_changes_tty(&quiet),
        "'deploy' is already published — your copy matches current"
    );

    // Bare: the standing scope's own bundle ships — and the machine's same-named copy is neither
    // the folder it came from nor a copy it left behind.
    let data = landed(publish_at(&ctx, &seams, ops::StoreScope::Here).unwrap());
    assert_eq!(
        data.bundle_digest,
        topos_core::digest::to_hex(&one_file(stranger).digest),
        "the copy you stand in is what shipped"
    );
    assert_eq!(data.from_placement, None, "{data:?}");
    assert!(!data.from_machine, "{data:?}");
    assert!(data.other_scope_draft.is_none(), "{data:?}");
    let receipt = crate::render::publish_tty(&data);
    assert!(!receipt.contains("machine copy"), "{receipt}");
}

/// A copy BEHIND the served current is refused, and the one command the refusal offers must drive
/// THE COPY THAT REFUSED. `-g` from inside a checkout resolves the machine store, so its refusal
/// spells `-g` too — offered bare, the update would run the project's copy and the publish would
/// refuse all over again.
#[test]
fn a_behind_copy_refuses_in_the_scope_the_flag_resolved() {
    let rig = Rig::new("pubscope-behind");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, machine_dir, _project_dir) =
        both_scopes("pubscope-behind-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));
    std::fs::write(machine_dir.join("SKILL.md"), b"# deploy\nmachine edit\n").unwrap();

    // The MACHINE store alone learns of a newer current it has not applied.
    let sp = rig.layout().published(&sid("s_deploy"));
    let mut sync: topos_types::persisted::SyncState =
        crate::doc::read_doc(&rig.fs, &sp.sync).unwrap().unwrap();
    sync.observed = sync.applied + 1;
    sync.observed_version_id = "b".repeat(64);
    crate::doc::write_doc(&rig.fs, &sp.sync, &sync).unwrap();

    let refusals = [
        publish_at(&ctx, &seams, ops::StoreScope::Machine).unwrap_err(),
        describe_at(&ctx, &seams, ops::StoreScope::Machine).unwrap_err(),
    ];
    for err in &refusals {
        assert!(
            matches!(
                err,
                ClientError::PublishBehind { global: true, skill } if skill == "deploy"
            ),
            "the refusal names the MACHINE copy: {err:?}"
        );
        assert!(
            crate::render::err_tty(err).ends_with("\n  update first: topos update -g deploy"),
            "{}",
            crate::render::err_tty(err)
        );
    }
}

/// The SAME edit made in both scopes is ONE edit. This publish carries those bytes, the other copy
/// already holds them, and the next sweep settles it clean — so every sentence the cross-scope
/// disclosure could print there would be false. It prints nothing.
#[test]
fn an_identical_edit_in_both_scopes_earns_no_disclosure() {
    let rig = Rig::new("pubscope-same");
    rig.seed_session();
    let v = one_file(b"# deploy\n");
    let seams = publish_seams(&v);
    let (proj, machine_dir, project_dir) =
        both_scopes("pubscope-same-repo", &rig, &seams.plane, &seams.dir);
    let ctx = rig.ctx_at(Some(&proj.0));

    let same = b"# deploy\nthe same edit\n";
    std::fs::write(project_dir.join("SKILL.md"), same).unwrap();
    std::fs::write(machine_dir.join("SKILL.md"), same).unwrap();

    let preview = describe_at(&ctx, &seams, ops::StoreScope::Here).unwrap();
    let ops::PublishPreview::Describe(d) = preview else {
        panic!("there are bytes to ship");
    };
    assert!(d.other_scope_draft.is_none(), "{d:?}");
    let data = landed(publish_at(&ctx, &seams, ops::StoreScope::Here).unwrap());
    assert_eq!(
        data.bundle_digest,
        topos_core::digest::to_hex(&one_file(same).digest)
    );
    assert!(data.other_scope_draft.is_none(), "{data:?}");
    let receipt = crate::render::publish_tty(&data);
    assert!(!receipt.contains("keeps its edits"), "{receipt}");
}

/// A GENESIS publish has nothing to diff against — the lock names exactly the bytes the adopt
/// recorded — so the describe withholds the `review:` line rather than offering a command that
/// prints nothing.
#[test]
fn a_genesis_publish_offers_no_read_of_an_empty_diff() {
    let rig = Rig::new("pubscope-genesis");
    rig.seed_session();
    let src = rig.work.0.join("fresh");
    skill_source(&src, b"# fresh\n");
    let ctx = rig.ctx_at(Some(&rig.work.0));
    ops::add(&ctx, &src).unwrap();

    let seams = publish_seams(&one_file(b"# fresh\n"));
    let session_connect = |_s: &Session| ops::SessionTransports {
        plane: Box::new(seams.plane.clone()),
        directory: Box::new(seams.dir.clone()),
        contribute: Box::new(OkPublish),
        governance: Box::new(NoGovernance),
    };
    let preview = ops::publish_describe(
        &ctx,
        Some(&session_connect),
        None,
        "fresh",
        false,
        None,
        None,
        &ops::Selection::default(),
        ops::StoreScope::Here,
    )
    .unwrap();
    let ops::PublishPreview::Describe(d) = preview else {
        panic!("a first publish previews");
    };
    assert!(d.review.is_none(), "{d:?}");
    let text = crate::render::publish_describe_tty(&d, &["topos".into(), "publish".into()]);
    assert!(!text.contains("\nreview: "), "{text}");
}
