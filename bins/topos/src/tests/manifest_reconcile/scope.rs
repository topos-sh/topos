//! The two UNBLENDED scopes. The person scope's complete file (and the nothing an absent file
//! demands), the project scope where the nearest file governs whole, and the scope rule itself: an
//! `update` converges where the invocation STANDS, `-g` is the machine, and only the background
//! hook sweep covers both.

use std::sync::{Arc, Mutex};

use topos_types::requests::{WireChannelEntry, WireChannelSkill};
use topos_types::results::PullAction;

use crate::error::ClientError;
use crate::plane::DeliverySnapshot;
use crate::sessions::{self, SESSION_ACTIVE, Session};
use crate::{ops, sync_status};

use super::rig::*;

// =================================================================================================
// The person scope: the complete file (and the nothing an absent file demands).
// =================================================================================================

#[test]
fn the_feed_row_adopts_the_workspaces_feed() {
    let rig = Rig::new("feedrow");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log.clone()).with_version("s_deploy", &v);
    let mut ds = delivered("s_deploy", "deploy", &v);
    ds.assigned_by = Some("Ada".into());
    plane.serve(DeliverySnapshot {
        skills: vec![ds],
        declined: vec![("s_old".into(), "retired".into())],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    // The feed row (the line login wrote on the first connection) takes the workspace's whole
    // feed — installed silently (the login was the acceptance), into the home dirs.
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    // The FIRST materialization reads `installed`, leads with the workspace-qualified name, and
    // names its destinations — never an agent.
    assert_eq!(row.action, PullAction::Installed, "{:?}", out.warnings);
    assert_eq!(row.scope.as_deref(), Some("person"));
    assert_eq!(
        row.display.as_deref(),
        Some(&format!("@{WS_NAME}/deploy")[..])
    );
    assert_eq!(row.destinations.len(), 1, "{:?}", row.destinations);
    assert!(rig.skills().join("deploy/SKILL.md").exists());
    // The applied report went out; the offline cache carries the identity, the attribution, and
    // the caller's declines.
    assert!(log.lock().unwrap().iter().any(|l| l == "report s_deploy"));
    let status = sync_status::read(&rig.fs, &rig.layout()).unwrap();
    let ws = &status.workspaces[WS];
    assert_eq!(ws.host.as_deref(), Some(HOST));
    assert_eq!(ws.workspace_name.as_deref(), Some(WS_NAME));
    assert_eq!(ws.delivered["s_deploy"].name, "deploy");
    assert_eq!(ws.delivered["s_deploy"].assigned_by.as_deref(), Some("Ada"));
    assert!(!ws.delivered["s_deploy"].via_manifest);
    assert_eq!(
        ws.declined.get("s_old").map(String::as_str),
        Some("retired")
    );
    // Nothing is loud: the feed row adopts everything, so there is nothing to disclose.
    assert!(
        !crate::message::legacy_lines(&out.advisories)
            .into_iter()
            .any(|w| w.starts_with("GLOBAL_MANIFEST")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn no_global_file_delivers_nothing_and_cleans_what_was_delivered() {
    // The old contract ("no file behaves as one feed row per connected workspace") is DEAD: with
    // no file nothing is demanded machine-wide — nothing lands, and a copy an earlier recipe
    // placed retires exactly as a deleted feed row's would (bytes kept: the absence is this
    // machine's own choice, not a feed withdrawal).
    let rig = Rig::new("nofile");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Delivered while the feed row stood…
    rig.seed_feed();
    sweep(&ctx, &plane, &dir);
    let placed = rig.skills().join("deploy");
    assert!(placed.exists());

    // …then the whole file is deleted. The next sweep delivers nothing and RETIRES the
    // placement — cleanup semantics do not depend on the file's existence.
    std::fs::remove_file(rig.layout().home().join(crate::manifest::MANIFEST_FILE)).unwrap();
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !out.data
            .skills
            .iter()
            .any(|s| s.skill == "deploy" && !matches!(s.action, PullAction::Removed)),
        "nothing is delivered person-scope with no file: {:?}",
        out.data.skills
    );
    assert!(
        !placed.exists(),
        "the placement retired: {:?}",
        out.warnings
    );
    // The bytes stay — this machine's own choice keeps them, like an `"off"` row's clean.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(rig.layout().skill_dir(&sid).exists());

    // A machine that never had the file delivers nothing at all.
    let fresh = Rig::new("nofile-fresh");
    fresh.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = fresh.ctx_at(Some(&fresh.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        !fresh.skills().join("deploy").exists(),
        "{:?}",
        out.data.skills
    );
    assert!(
        out.data.skills.is_empty(),
        "nothing demanded, nothing moved: {:?}",
        out.data.skills
    );
}

#[test]
fn a_global_file_withholds_the_feed_and_says_so_loudly() {
    let rig = Rig::new("filewithholds");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let deploy = one_file(b"# deploy\n");
    let other = one_file(b"# other\n");
    // The file names ONE bundle and no feed row — it is a complete recipe, so the rest of what the
    // workspace assigns does not flow here.
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/other\" = \"latest\"\n"
    ));
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
    let out = sweep(&ctx, &plane, &dir);

    assert!(
        rig.skills().join("other/SKILL.md").exists(),
        "the file's own row delivers: {:?}",
        out.warnings
    );
    assert!(
        !rig.skills().join("deploy").exists(),
        "no feed row, no feed"
    );
    let loud = crate::message::legacy_lines(&out.advisories)
        .into_iter()
        .find(|w| w.starts_with("GLOBAL_MANIFEST"))
        .expect("the loud line");
    assert!(loud.contains(&format!("{HOST}/{WS_NAME}")), "{loud}");
    assert!(
        loud.contains("names 1 bundles from this workspace"),
        "{loud}"
    );
    assert!(
        loud.contains("1 more that it assigns you are not listed there"),
        "{loud}"
    );
    assert!(loud.contains(&format!("topos add -g @{WS_NAME}")), "{loud}");
}

/// An exchange that lands EMPTY says so. Without the line the receipt names only what moved, so a
/// person who was just told the feed would be applied reads the silence as a failed apply.
#[test]
fn an_exchange_that_serves_nothing_says_so_without_counting_as_a_failure() {
    let rig = Rig::new("emptyserve");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    let line = crate::message::legacy_lines(&out.disclosures)
        .into_iter()
        .find(|d| d.starts_with("NOTHING_ASSIGNED"))
        .unwrap_or_else(|| panic!("the empty-serve line: {:?}", out.disclosures));
    assert!(line.contains(&format!("{HOST}/{WS_NAME}")), "{line}");
    assert!(line.contains("nothing is assigned to you yet"), "{line}");
    // A DISCLOSURE: the exchange worked, so nothing here may read as broken.
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(out.data.skills.is_empty(), "{:?}", out.data.skills);
}

/// The line is the WORKSPACE's fact, said ONCE however many times a recipe adopts the address.
/// One feed row names an ADDRESS, and a workspace whose id was re-minted (the old session row
/// surviving beside the new one) has two live sessions on that address — the row drives the feed
/// reconcile once per session, and the receipt must still carry exactly one line.
#[test]
fn one_address_adopted_twice_earns_one_empty_exchange_line() {
    let rig = Rig::new("emptytwice");
    rig.seed_session();
    rig.seed_feed();
    sessions::upsert_session(
        &rig.fs,
        &rig.layout(),
        Session {
            host: HOST.into(),
            base_url: format!("https://{HOST}/api"),
            // A DIFFERENT opaque id for the SAME address — the session file keys on (host, id),
            // so both rows live, and both name `acme.test/eng`.
            workspace_id: "w_eng_reminted".into(),
            workspace_name: WS_NAME.into(),
            display_name: "Engineering".into(),
            session_id: "sn_2".into(),
            credential: "cred-2".into(),
            status: SESSION_ACTIVE.into(),
            logged_in_at: 2,
        },
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    plane.serves(Vec::new());
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert_eq!(
        crate::message::legacy_lines(&out.disclosures)
            .into_iter()
            .filter(|d| d.starts_with("NOTHING_ASSIGNED"))
            .count(),
        1,
        "{:?}",
        out.disclosures
    );
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
}

/// Bundles that ARRIVED and were skipped here are a local choice, not an empty workspace — the
/// line must not fire on an `"off"` row's work.
#[test]
fn a_served_feed_never_earns_the_empty_exchange_line() {
    let rig = Rig::new("servedfeed");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let noisy = one_file(b"# noisy\n");
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n\n[skills]\n\"{HOST}/{WS_NAME}/noisy\" = \"off\"\n"
    ));
    let plane = FakePlane::new(log).with_version("s_noisy", &noisy);
    plane.serves(vec![delivered("s_noisy", "noisy", &noisy)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_noisy", "noisy", &noisy)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);

    assert!(
        !rig.skills().join("noisy").exists(),
        "the switch withholds it"
    );
    assert!(
        !crate::message::legacy_lines(&out.disclosures)
            .into_iter()
            .any(|d| d.starts_with("NOTHING_ASSIGNED")),
        "the workspace assigned something: {:?}",
        out.disclosures
    );
}

#[test]
fn an_off_row_withholds_exactly_its_bundle_from_a_flowing_feed() {
    let rig = Rig::new("offrow");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let deploy = one_file(b"# deploy\n");
    let noisy = one_file(b"# noisy\n");
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n\n[skills]\n\"{HOST}/{WS_NAME}/noisy\" = \"off\"\n"
    ));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &deploy)
        .with_version("s_noisy", &noisy);
    plane.serves(vec![
        delivered("s_deploy", "deploy", &deploy),
        delivered("s_noisy", "noisy", &noisy),
    ]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &deploy),
            catalog_entry("s_noisy", "noisy", &noisy),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(
        rig.skills().join("deploy/SKILL.md").exists(),
        "the feed flows: {:?}",
        out.warnings
    );
    assert!(
        !rig.skills().join("noisy").exists(),
        "the one switch is the one exception"
    );
    // A flowing feed is not a withheld one — no loud line here.
    assert!(
        !crate::message::legacy_lines(&out.advisories)
            .into_iter()
            .any(|w| w.starts_with("GLOBAL_MANIFEST")),
        "{:?}",
        out.warnings
    );
}

#[test]
fn an_explicit_pinned_row_beats_the_feeds_version() {
    let rig = Rig::new("pinbeats");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let old = one_file(b"# v1\n");
    let new = one_file(b"# v2\n");
    rig.write_global(&format!(
        "[workspaces]\n\"{HOST}/{WS_NAME}\" = \"latest\"\n\n[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"{}\"\n",
        topos_core::digest::to_hex(&old.id)
    ));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &old)
        .with_version("s_deploy", &new);
    plane.serves(vec![delivered("s_deploy", "deploy", &new)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &new)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read_to_string(rig.skills().join("deploy/SKILL.md")).unwrap(),
        "# v1\n",
        "the row's pin lands, not the feed's current"
    );
    assert_eq!(
        out.data
            .skills
            .iter()
            .filter(|s| s.skill == "deploy")
            .count(),
        1,
        "one identity, one delivery per scope"
    );
}

#[test]
fn a_declined_bundle_a_row_still_delivers_is_disclosed() {
    let rig = Rig::new("declined");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"latest\"\n"
    ));
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    plane.serve(DeliverySnapshot {
        skills: Vec::new(),
        declined: vec![("s_deploy".into(), "deploy".into())],
        ..empty_snapshot()
    });
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(rig.skills().join("deploy/SKILL.md").exists());
    let line = crate::message::legacy_lines(&out.advisories)
        .into_iter()
        .find(|w| w.starts_with("DECLINED_OVERRIDE"))
        .expect("the honest note");
    assert!(line.contains("you declined this on the web"), "{line}");
}

// =================================================================================================
// The project scope: nearest wins whole, and the two scopes never blend.
// =================================================================================================

#[test]
fn a_project_manifest_lands_in_the_checkout_self_ignoring() {
    let rig = Rig::new("project");
    rig.seed_session();
    let proj = project(
        "proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    // The feed delivers NOTHING — the demand is the project file's.
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);

    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row.action, PullAction::Installed, "{:?}", out.warnings);
    assert_eq!(row.scope.as_deref(), Some(&*proj.0.display().to_string()));
    // The bytes live INSIDE the checkout, not the home-scope dirs.
    let placed = proj.0.join(".claude/skills/deploy");
    assert!(placed.join("SKILL.md").exists());
    assert!(!rig.skills().join("deploy").exists());
    // The placed dir SELF-IGNORES (the node_modules model), and NOTHING under `.git/` was written.
    assert_eq!(
        std::fs::read(placed.join(".gitignore")).unwrap(),
        crate::scan::IGNORE_SENTINEL
    );
    assert!(
        std::fs::read_dir(proj.0.join(".git"))
            .unwrap()
            .next()
            .is_none(),
        "nothing under .git/ was written"
    );
    // The engine state lives in the PROJECT's own store, and the store ignores itself whole.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(!rig.layout().skill_dir(&sid).exists());
    let playout =
        crate::sidecar::existing_project_store(&rig.fs, &proj.0).expect("the project store");
    assert!(playout.skill_dir(&sid).exists());
    assert_eq!(
        std::fs::read(proj.0.join(".topos/.gitignore")).unwrap(),
        b"*\n"
    );
    // A second sweep is a clean no-op: the sentinel never reads as an edit.
    let out2 = sweep(&ctx, &plane, &dir);
    assert!(out2.warnings.is_empty(), "{:?}", out2.warnings);
    let row2 = out2
        .data
        .skills
        .iter()
        .find(|s| s.skill == "deploy")
        .unwrap();
    assert_eq!(row2.action, PullAction::UpToDate, "{:?}", out2.data.skills);
}

#[test]
fn the_nearest_project_file_governs_whole() {
    let rig = Rig::new("nearest");
    rig.seed_session();
    let repo = project(
        "proj-outer",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\nrepo-wide = \"latest\"\n"),
    );
    let nested = repo.0.join("services/api");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join(crate::manifest::MANIFEST_FILE),
        format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\napi-only = \"latest\"\n"),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let wide = one_file(b"# repo-wide\n");
    let api = one_file(b"# api-only\n");
    let plane = FakePlane::new(log)
        .with_version("s_wide", &wide)
        .with_version("s_api", &api);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_wide", "repo-wide", &wide),
            catalog_entry("s_api", "api-only", &api),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&nested));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert!(
        nested.join(".claude/skills/api-only/SKILL.md").exists(),
        "the nearest file governs"
    );
    assert!(
        !repo.0.join(".claude/skills/repo-wide").exists(),
        "the ancestor's rows never blend in from below"
    );
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "repo-wide"),
        "{:?}",
        out.data.skills
    );
}

/// Both scopes at once is the BACKGROUND sweep's shape (a hand-run update converges only where it
/// stands), so the unblended property is proven through it.
#[test]
fn the_same_bundle_at_both_scopes_lands_twice() {
    let rig = Rig::new("unblended");
    rig.seed_session();
    rig.seed_feed();
    let proj = project(
        "proj-two",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v);
    // The feed delivers it too — no shadowing: each scope takes what its own recipe says.
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep_both(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    let person_copy = rig.skills().join("deploy");
    let project_copy = proj.0.join(".claude/skills/deploy");
    assert!(person_copy.join("SKILL.md").exists(), "the feed's copy");
    assert!(project_copy.join("SKILL.md").exists(), "the project's copy");
    // TWO rows, one per scope label.
    let mut scopes: Vec<&str> = out
        .data
        .skills
        .iter()
        .filter(|s| s.skill == "deploy")
        .filter_map(|s| s.scope.as_deref())
        .collect();
    scopes.sort_unstable();
    let proj_label = proj.0.display().to_string();
    let mut want = vec!["person", proj_label.as_str()];
    want.sort_unstable();
    assert_eq!(scopes, want, "{:?}", out.data.skills);
    // TWO state trees, each recording only its own scope's placements.
    let sid = crate::id::SkillId::parse("s_deploy").unwrap();
    let playout = crate::sidecar::existing_project_store(&rig.fs, &proj.0).unwrap();
    assert!(rig.layout().skill_dir(&sid).exists());
    assert!(playout.skill_dir(&sid).exists());
    let home_map = crate::doc::read_map(&rig.fs, &rig.layout().published(&sid).map)
        .unwrap()
        .unwrap();
    assert!(
        home_map
            .placements
            .iter()
            .all(|p| !std::path::Path::new(p).starts_with(&proj.0)),
        "{:?}",
        home_map.placements
    );
    let proj_map = crate::doc::read_map(&rig.fs, &playout.published(&sid).map)
        .unwrap()
        .unwrap();
    assert!(
        proj_map
            .placements
            .iter()
            .all(|p| std::path::Path::new(p).starts_with(&proj.0)),
        "{:?}",
        proj_map.placements
    );

    // A draft in the PROJECT copy stays that scope's business.
    std::fs::write(project_copy.join("SKILL.md"), b"# project edit\n").unwrap();
    let out = sweep_both(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    assert_eq!(
        std::fs::read(project_copy.join("SKILL.md")).unwrap(),
        b"# project edit\n"
    );
    assert_eq!(
        std::fs::read(person_copy.join("SKILL.md")).unwrap(),
        b"# deploy\n",
        "the person copy never sees the project draft"
    );
}

#[test]
fn a_channel_expands_and_an_explicit_row_of_the_same_identity_wins() {
    let rig = Rig::new("channel");
    let old = one_file(b"# v1\n");
    let new = one_file(b"# v2\n");
    let other = one_file(b"# other\n");
    rig.seed_session();
    let proj = project(
        "proj-ch",
        &format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"{}\"\n\
             \n[channels]\nbackend = \"latest\"\n",
            topos_core::digest::to_hex(&old.id)
        ),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &old)
        .with_version("s_deploy", &new)
        .with_version("s_other", &other);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &new),
            catalog_entry("s_other", "other", &other),
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
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    // The channel delivered its other member …
    assert!(proj.0.join(".claude/skills/other/SKILL.md").exists());
    // … and the explicit row's PIN decided the shared identity, not the channel's current.
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v1\n"
    );
    assert_eq!(
        out.data
            .skills
            .iter()
            .filter(|s| s.skill == "deploy")
            .count(),
        1
    );
}

#[test]
fn a_workspace_row_without_a_session_is_an_honest_local_line() {
    let rig = Rig::new("nosession");
    // NO session at all — the file references a workspace this install never logged into.
    let proj = project(
        "proj-ns",
        "workspace = \"elsewhere.dev/ops\"\n\n[skills]\ndeploy = \"latest\"\n",
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&ctx, &plane, &dir);
    let w = crate::message::legacy_lines(&out.warnings)
        .into_iter()
        .find(|w| w.starts_with("NOT_AVAILABLE"))
        .expect("the honest line");
    assert!(w.contains("topos login elsewhere.dev/ops"), "{w}");
}

/// A manifest the run would DRIVE that fails to load refuses the run WHOLE — never a
/// success-claiming receipt over a no-op. The refusal is the MANIFEST family (verbatim message
/// naming the file; the TTY closes with `nothing changed`), and the bytes stay: the failure mode
/// of a mistake must be keeping bytes.
///
/// The SWEEP'S BUILT-IN ENSURE hangs off this same refusal, which is why the two live together: a
/// run that refuses at the load closes with `nothing changed`, so nothing may have been placed on
/// the way to saying it — see [`a_load_refusal_is_never_a_soft_failure`].
#[test]
fn a_driven_manifest_that_fails_to_load_refuses_the_run_whole() {
    let rig = Rig::new("badfile");
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
    assert!(placed.exists());

    // A typo the grammar refuses: the driven scope's recipe is unreadable, so the run refuses —
    // no receipt, no partial sweep claims, and nothing cleaned.
    rig.write_global("[skills]\n\"not a reference\" = \"latest\"\n");
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("topos.toml"),
        "the refusal names the file: {msg}"
    );
    let tty = crate::render::err_tty(&err);
    assert_eq!(tty.lines().last(), Some("nothing changed"), "{tty}");
    assert!(
        !tty.contains("updated machine-wide"),
        "a refusal never claims a sweep: {tty}"
    );
    assert!(placed.exists(), "a refused run never cleans");
}

/// The BARE SWEEP re-syncs the built-in `topos` skill, and it does so AFTER the reconcile, gated on
/// the run not having refused locally — because a refused manifest load prints `nothing changed`,
/// and a run that placed a skill folder first would be lying in its own last line.
///
/// The gate is [`ops::quiet_soft_failure`]: a TRANSPORT or AUTH failure still refreshes the
/// built-in (it is rendered from this binary and owes nothing to a reachable plane, so a
/// `self-update` on an offline machine must still land its meta-skill), while every local refusal —
/// the manifest load's two above all — skips it. This asserts the split the dispatch depends on, so
/// moving a manifest refusal into the soft set goes red here rather than silently making that last
/// line false again.
#[test]
fn a_load_refusal_is_never_a_soft_failure() {
    for local in [
        ClientError::ManifestInvalid("topos.toml: not a reference".to_owned()),
        ClientError::ManifestInvalid("topos.toml: unknown field `path`".to_owned()),
    ] {
        assert!(
            !ops::quiet_soft_failure(&local),
            "a load refusal must skip the built-in ensure: {}",
            local.code()
        );
    }
    // The other side of the split: an unreachable plane still refreshes the built-in.
    assert!(ops::quiet_soft_failure(&ClientError::Plane(
        "connection refused".to_owned()
    )));
}
/// A broken manifest in a scope the run does NOT drive never blocks it: the failure degrades to
/// the freeze warning, the driven scope still converges, and the frozen scope's bytes stay.
#[test]
fn a_broken_manifest_in_an_undriven_scope_warns_and_freezes_it() {
    let rig = Rig::new("badfile-undriven");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v = one_file(b"# deploy\n");
    let api = one_file(b"# api\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v)
        .with_version("s_api", &api);
    plane.serves(vec![delivered("s_deploy", "deploy", &v)]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_deploy", "deploy", &v),
            catalog_entry("s_api", "api", &api),
        ],
        Vec::new(),
    );
    let ctx = rig.ctx_at(Some(&rig.work.0));
    sweep(&ctx, &plane, &dir);
    let placed = rig.skills().join("deploy");
    assert!(placed.exists());

    // Break the GLOBAL file, then drive the PROJECT scope only: the machine scope is not this
    // run's to claim, so its fault is a warning and its bytes freeze in place.
    rig.write_global("[skills]\n\"not a reference\" = \"latest\"\n");
    let proj = project(
        "badfile-undriven-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\napi = \"latest\"\n"),
    );
    let pctx = rig.ctx_at(Some(&proj.0));
    let out = sweep(&pctx, &plane, &dir);
    assert!(
        crate::message::legacy_lines(&out.warnings)
            .into_iter()
            .any(|w| w.starts_with("MANIFEST_INVALID")),
        "{:?}",
        out.warnings
    );
    assert!(
        proj.0.join(".claude/skills/api/SKILL.md").exists(),
        "the driven project scope still converged"
    );
    assert!(placed.exists(), "a frozen scope never cleans");
}

// =================================================================================================
// The scope rule: an `update` converges where the invocation STANDS (`-g` = the machine); only the
// background hook sweep covers both, because silent delivery may never narrow to one folder.
// =================================================================================================

/// Two populated scopes on one machine: the project file demands `api`, and the connected feed
/// delivers `deploy`. Every test below asserts WHICH of the two a given invocation converged.
struct TwoScopes {
    proj: Scratch,
    plane: FakePlane,
    dir: FakeDirectory,
}

fn two_scopes(tag: &str) -> TwoScopes {
    let api = one_file(b"# api\n");
    let deploy = one_file(b"# deploy\n");
    let proj = project(
        tag,
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\napi = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_api", &api)
        .with_version("s_deploy", &deploy);
    // The FEED carries `deploy` alone — `api` is the project row's demand and nothing else's.
    plane.serves(vec![delivered("s_deploy", "deploy", &deploy)]);
    let dir = FakeDirectory::new(
        vec![
            catalog_entry("s_api", "api", &api),
            catalog_entry("s_deploy", "deploy", &deploy),
        ],
        Vec::new(),
    );
    TwoScopes { proj, plane, dir }
}

#[test]
fn a_bare_update_inside_a_project_converges_only_that_project() {
    let rig = Rig::new("scope-here-proj");
    rig.seed_session();
    rig.seed_feed();
    let t = two_scopes("scope-here-proj");
    let ctx = rig.ctx_at(Some(&t.proj.0));
    let out = sweep(&ctx, &t.plane, &t.dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    // The folder's own demand landed, inside the checkout, and the receipt names the scope.
    assert!(t.proj.0.join(".claude/skills/api/SKILL.md").exists());
    assert_eq!(
        out.data.scope,
        Some(format!("project {}", t.proj.0.display()))
    );
    // The machine scope was never DRIVEN: no bytes in the home agent dirs, no home store entry.
    assert!(
        !rig.skills().exists(),
        "the feed's copy stayed away from the machine's agent dirs"
    );
    let deploy_id = crate::id::SkillId::parse("s_deploy").unwrap();
    assert!(
        !rig.layout().skill_dir(&deploy_id).exists(),
        "the home store gained no entry"
    );
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "deploy"),
        "{:?}",
        out.data.skills
    );
    // The machine's demand is PENDING, not dropped — the machine-scoped run still delivers it.
    let out = sweep_scoped(&ctx, &t.plane, &t.dir, ops::UpdateScope::Machine);
    assert!(
        rig.skills().join("deploy/SKILL.md").exists(),
        "{:?}",
        out.warnings
    );
}

#[test]
fn a_bare_update_outside_a_project_converges_the_machine() {
    let rig = Rig::new("scope-here-machine");
    rig.seed_session();
    rig.seed_feed();
    let t = two_scopes("scope-here-machine");
    // Standing in the plain work dir — no `topos.toml` covers it, so the machine is where you are.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let out = sweep(&ctx, &t.plane, &t.dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    assert_eq!(out.data.scope.as_deref(), Some("machine"));
    assert!(rig.skills().join("deploy/SKILL.md").exists());
    // A project the invocation never stood in is not reconciled from outside it.
    assert!(!t.proj.0.join(".claude/skills/api").exists());
    assert!(crate::sidecar::existing_project_store(&rig.fs, &t.proj.0).is_none());
}

#[test]
fn update_g_inside_a_project_converges_only_the_machine() {
    let rig = Rig::new("scope-global");
    rig.seed_session();
    rig.seed_feed();
    let t = two_scopes("scope-global");
    let ctx = rig.ctx_at(Some(&t.proj.0));
    let out = sweep_scoped(&ctx, &t.plane, &t.dir, ops::UpdateScope::Machine);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    assert_eq!(out.data.scope.as_deref(), Some("machine"));
    assert!(rig.skills().join("deploy/SKILL.md").exists());
    // The checkout the invocation was standing in is untouched: no bytes, and no store minted.
    assert!(
        !t.proj.0.join(".claude/skills/api").exists(),
        "{:?}",
        out.data.skills
    );
    assert!(crate::sidecar::existing_project_store(&rig.fs, &t.proj.0).is_none());
    assert!(
        !out.data.skills.iter().any(|s| s.skill == "api"),
        "{:?}",
        out.data.skills
    );
}

/// The property the hook sweep exists for: `update --quiet` fires on a session start in SOME
/// folder, and everything the machine holds must still converge — silent delivery is the promise,
/// so the folder the agent happened to open may never narrow what auto-update reaches.
#[test]
fn the_hook_sweep_converges_both_scopes_from_inside_a_project() {
    let rig = Rig::new("scope-both");
    rig.seed_session();
    rig.seed_feed();
    let t = two_scopes("scope-both");
    let ctx = rig.ctx_at(Some(&t.proj.0));
    let out = sweep_both(&ctx, &t.plane, &t.dir);
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);

    assert_eq!(out.data.scope.as_deref(), Some("both"));
    assert!(
        t.proj.0.join(".claude/skills/api/SKILL.md").exists(),
        "the project scope converged"
    );
    assert!(
        rig.skills().join("deploy/SKILL.md").exists(),
        "the machine scope converged in the same run"
    );
}

/// A project file the grammar REFUSES still covers the folder — so a bare `update` standing in
/// it DRIVES that scope, and the broken file refuses the run whole. Falling back to the machine
/// would answer a typo by converging a tree nobody asked about — and would land bytes the person
/// is at that moment being told their manifest is broken.
#[test]
fn a_frozen_project_manifest_refuses_and_never_falls_back_to_the_machine() {
    let rig = Rig::new("scope-frozen");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let deploy = one_file(b"# deploy\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &deploy);
    plane.serves(vec![delivered("s_deploy", "deploy", &deploy)]);
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_deploy", "deploy", &deploy)],
        Vec::new(),
    );
    let proj = project(
        "scope-frozen-proj",
        "[skills]\n\"not a reference\" = \"latest\"\n",
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap_err();

    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains(&proj.0.display().to_string()),
        "the refusal names the file this run stood under: {msg}"
    );
    assert_eq!(
        crate::render::err_tty(&err).lines().last(),
        Some("nothing changed")
    );
    assert!(
        !rig.skills().exists(),
        "a broken project file never hands the run to the machine"
    );
}

/// `update -g` over a global file spelling a field the grammar does not know refuses the file —
/// nonzero, `nothing changed`, and NO success-claiming receipt — instead of a warning under an
/// "updated machine-wide" summary that would claim a sweep which never read the file.
#[test]
fn update_g_over_an_unreadable_file_refuses_naming_the_field() {
    let rig = Rig::new("stale-refuses");
    rig.seed_session();
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dir = \"~/.claude/skills\" }}\n"
    ));
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Machine,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("unknown field `dir` — a workspace bundle takes"),
        "the grammar's own teaching reaches the surfaces verbatim: {msg}"
    );
    assert!(msg.contains("topos.toml"), "the file is named: {msg}");
    let tty = crate::render::err_tty(&err);
    assert_eq!(tty.lines().last(), Some("nothing changed"), "{tty}");
    assert!(!tty.contains("updated machine-wide"), "{tty}");
}

/// The dest DIALECT faults refuse as MANIFEST refusals in both directions — never as
/// CORRUPT_STATE, whose fixed TTY line blames topos's own state for a hand-edit and buries the
/// real teaching in the log. The message names the file, the offending entry, and the rule, and
/// the run they would drive refuses whole.
#[test]
fn a_dest_dialect_fault_refuses_as_a_manifest_refusal_not_corrupt_state() {
    // Machine direction: a bare-relative entry in the machine-wide file.
    let rig = Rig::new("dialect-machine");
    rig.seed_session();
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dest = [\"skills\"] }}\n"
    ));
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Machine,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("dest entry `skills` is relative — the machine-wide file names machine"),
        "the rule reaches the surfaces verbatim, not just log.jsonl: {msg}"
    );
    assert!(msg.contains("topos.toml"), "the file is named: {msg}");
    assert_eq!(
        crate::render::err_tty(&err).lines().last(),
        Some("nothing changed")
    );

    // Project direction: an absolute (checkout-escaping) entry in a project file.
    let rig = Rig::new("dialect-project");
    rig.seed_session();
    let proj = project(
        "dialect-project-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\napi = {{ dest = [\"/abs\"] }}\n"),
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
    let msg = crate::render::safe_message(&err);
    assert!(
        msg.contains("dest entry `/abs` leaves the checkout"),
        "{msg}"
    );
    assert!(
        msg.contains(&proj.0.display().to_string()),
        "the project file is named: {msg}"
    );
}

/// The QUIET sweep must not claim success over a manifest it could not read either: the refusal
/// is a HARD failure (never the auth/transport soft-skip that exits 0), so the hook surfaces a
/// nonzero exit instead of a silent success stamp.
#[test]
fn the_quiet_sweep_refuses_a_broken_manifest_hard() {
    let rig = Rig::new("quiet-refuses");
    rig.seed_session();
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = {{ dir = \"~/.claude/skills\" }}\n"
    ));
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())));
    let dir = FakeDirectory::new(Vec::new(), Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let err = ops::manifest_update(
        &ctx,
        &connect(&plane, &dir),
        None,
        &ops::ManifestUpdateOpts {
            scope: ops::UpdateScope::Both,
            ..ops::ManifestUpdateOpts::default()
        },
    )
    .unwrap_err();
    assert_eq!(err.code(), "MANIFEST_INVALID");
    assert!(
        !ops::quiet_soft_failure(&err),
        "a broken manifest is a local fault — the hook path must exit nonzero, not warn-and-0"
    );
}

/// A named target narrows WITHIN the driven scope — so a name that is perfectly real one scope
/// over is a miss, and the refusal has to say which scope it searched rather than claim the name
/// is nowhere (which would send someone to re-add what they already have).
#[test]
fn a_target_from_the_other_scope_refuses_naming_the_scope_it_searched() {
    let rig = Rig::new("scope-target");
    rig.seed_session();
    let t = two_scopes("scope-target");

    // Standing in the project: `deploy` is the FEED's, and the feed is not this folder's scope.
    let ctx = rig.ctx_at(Some(&t.proj.0));
    let refused = ops::manifest_update(
        &ctx,
        &connect(&t.plane, &t.dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["deploy".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    );
    let Err(err) = refused else {
        panic!("a target from the other scope must refuse");
    };
    assert_eq!(err.code(), "INVALID_ARGUMENT", "{err}");
    let msg = err.to_string();
    assert!(
        msg.contains(&format!(
            "'deploy' is not demanded by {}/topos.toml",
            t.proj.0.display()
        )),
        "{msg}"
    );
    assert!(msg.contains("`topos update -g deploy`"), "{msg}");
    assert!(!rig.skills().exists(), "the refusal moved no bytes: {msg}");

    // The mirror image, standing outside the project: `api` is the project file's demand.
    let ctx = rig.ctx_at(Some(&rig.work.0));
    let refused = ops::manifest_update(
        &ctx,
        &connect(&t.plane, &t.dir),
        None,
        &ops::ManifestUpdateOpts {
            targets: vec!["api".to_owned()],
            ..ops::ManifestUpdateOpts::default()
        },
    );
    let Err(err) = refused else {
        panic!("a target from the other scope must refuse");
    };
    let msg = err.to_string();
    assert!(
        msg.contains("'api' is not in your machine-wide set"),
        "{msg}"
    );
    assert!(msg.contains("`topos add -g api`"), "{msg}");
}

// =================================================================================================
// WHICH VERSION A COPY REPLACED — said at both scopes, once.
// =================================================================================================

/// The 12 chars every receipt spells a version id with.
fn short12(v: &Version) -> String {
    crate::render::short(&topos_core::digest::to_hex(&v.id)).to_owned()
}

/// The disclosure lines of one code, in order.
fn coded<'a>(out: &'a ops::PullOutcome, code: &str) -> Vec<&'a str> {
    out.disclosures
        .iter()
        .filter(|m| m.code.as_deref() == Some(code))
        .map(|m| m.text.as_str())
        .collect()
}

/// **A MACHINE-WIDE UPDATE NAMES THE VERSION IT REPLACED.** A project update says it from
/// `topos.lock` (the old document against the new one); the machine has no lock, so `-g` printed
/// `fast-forwarded` and named neither end of the move — leaving out the one id a person needs to
/// type to put the old bytes back. The lock did not move here; the COPY did, and the copy is what
/// the line is about: same wording, same channel, same `note:` place in the receipt. A FIRST
/// receive replaced nothing and says nothing.
#[test]
fn a_machine_scope_update_discloses_the_version_it_replaced() {
    let rig = Rig::new("g-replaced");
    rig.seed_session();
    let v1 = one_file(b"# v1\n");
    let v2 = one_file(b"# v2\n");
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())))
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    rig.write_global(&format!(
        "[skills]\n\"{HOST}/{WS_NAME}/deploy\" = \"latest\"\n"
    ));
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // The first receive: bytes land, and nothing was replaced.
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let first = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert_eq!(first.data.skills[0].action, PullAction::Installed);
    assert!(
        coded(&first, "COPY_MOVED").is_empty(),
        "{:?}",
        first.disclosures
    );

    // The team publishes; the machine's copy fast-forwards — and says off what.
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v2)], Vec::new());
    let out = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert_eq!(out.data.skills[0].action, PullAction::FastForwarded);
    assert_eq!(
        coded(&out, "COPY_MOVED"),
        vec![format!("deploy: {} → {}", short12(&v1), short12(&v2))],
        "exactly one line, naming both ends: {:?}",
        out.disclosures
    );
    let receipt = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert_eq!(
        receipt
            .lines()
            .filter(|l| l.starts_with("note: deploy: "))
            .count(),
        1,
        "{receipt}"
    );

    // A sweep with nothing to move says nothing again.
    let quiet = sweep_scoped(&ctx, &plane, &dir, ops::UpdateScope::Machine);
    assert!(
        coded(&quiet, "COPY_MOVED").is_empty(),
        "old == new is not a move: {:?}",
        quiet.disclosures
    );
}

/// …and the project scope keeps saying it ONCE, through the lock it moves. The copy-level line is
/// the machine's answer to having no lock — a project run that emitted both would print the same
/// fact twice under two codes.
#[test]
fn a_project_scope_update_discloses_the_replaced_version_only_once() {
    let rig = Rig::new("proj-replaced");
    rig.seed_session();
    let v1 = one_file(b"# v1\n");
    let v2 = one_file(b"# v2\n");
    let plane = FakePlane::new(Arc::new(Mutex::new(Vec::new())))
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let proj = project(
        "proj-replaced-checkout",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let ctx = rig.ctx_at(Some(&proj.0));

    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    sweep(&ctx, &plane, &dir);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v2)], Vec::new());
    let out = sweep(&ctx, &plane, &dir);

    assert_eq!(
        coded(&out, "LOCK_MOVED"),
        vec![format!("deploy: {} → {}", short12(&v1), short12(&v2))],
        "{:?}",
        out.disclosures
    );
    assert!(
        coded(&out, "COPY_MOVED").is_empty(),
        "the lock already said it: {:?}",
        out.disclosures
    );
    let receipt = crate::render::pull_tty(
        &out.data,
        &out.decisions,
        &out.warnings,
        &out.advisories,
        &out.disclosures,
        out.failed_bundles.len(),
        out.unplaced_bundles.len(),
    );
    assert_eq!(
        receipt
            .lines()
            .filter(|l| l.starts_with("note: deploy: "))
            .count(),
        1,
        "{receipt}"
    );
}
