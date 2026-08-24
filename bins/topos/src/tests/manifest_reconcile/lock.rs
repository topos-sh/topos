//! The project LOCK's behavior end to end: install fills and never bumps, update moves,
//! `--frozen` refuses gaps, a fresh checkout converges to the lock (not to current) — for a
//! connected MCP server's revision exactly as for a skill's version — and a channel freezes to
//! its locked member list.

use std::sync::{Arc, Mutex};

use super::rig::{
    CallLog, FakeDirectory, FakePlane, HOST, Rig, Scratch, WS, WS_NAME, catalog_entry,
    catalog_server_at, channel, connect, delivered, delivered_at, one_file, project, sweep,
};
use crate::ops;
use topos_types::results::PullAction;

fn hex(v: &super::rig::Version) -> String {
    topos_core::digest::to_hex(&v.id)
}

fn install(
    ctx: &crate::ctx::Ctx<'_>,
    plane: &FakePlane,
    dir: &FakeDirectory,
    lock: ops::LockMode,
) -> Result<ops::PullOutcome, crate::error::ClientError> {
    ops::manifest_update(
        ctx,
        &connect(plane, dir),
        None,
        &ops::ManifestUpdateOpts {
            lock,
            ..ops::ManifestUpdateOpts::default()
        },
    )
}

fn lock_text(proj: &std::path::Path) -> String {
    std::fs::read_to_string(proj.join("topos.lock")).unwrap_or_default()
}

#[test]
fn install_fills_the_lock_once_and_never_bumps_it() {
    let rig = Rig::new("lock-fill");
    rig.seed_session();
    let proj = project(
        "lock-fill-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let v2 = one_file(b"# v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let ctx = rig.ctx_at(Some(&proj.0));

    // First install: the row has no entry — resolved ONCE at the served current, and written.
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    let text = lock_text(&proj.0);
    assert!(text.contains(&hex(&v1)), "the lock records v1:\n{text}");
    assert!(
        text.contains(&format!("workspace = \"{HOST}/{WS_NAME}\"")),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v1\n"
    );

    // The team publishes v2. Install converges to the LOCK: bytes stay v1, the lock stays v1.
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v2)], Vec::new());
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    assert!(lock_text(&proj.0).contains(&hex(&v1)), "never bumped");
    assert!(!lock_text(&proj.0).contains(&hex(&v2)));
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v1\n",
        "install never moves a locked project"
    );

    // `topos update` is what moves it: lock and bytes both go to v2.
    sweep(&ctx, &plane, &dir);
    assert!(lock_text(&proj.0).contains(&hex(&v2)), "update rewrites");
    assert!(!lock_text(&proj.0).contains(&hex(&v1)));
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v2\n"
    );
}

#[test]
fn a_fresh_checkout_converges_to_the_lock_not_to_current() {
    let rig = Rig::new("lock-fresh");
    rig.seed_session();
    let v1 = one_file(b"# v1\n");
    let v2 = one_file(b"# v2\n");
    let proj = project(
        "lock-fresh-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    // The committed lock names v1 while the server serves v2 — a clone must get v1.
    std::fs::write(
        proj.0.join("topos.lock"),
        format!(
            "schema = 1\nworkspace = \"{HOST}/{WS_NAME}\"\n\n[skills.deploy]\nversion = \"{}\"\n",
            hex(&v1)
        ),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v2)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));

    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v1\n",
        "the lock decides, not the served current"
    );
    assert!(lock_text(&proj.0).contains(&hex(&v1)));
}

#[test]
fn frozen_refuses_an_uncovered_row_and_writes_nothing() {
    let rig = Rig::new("lock-frozen");
    rig.seed_session();
    let proj = project(
        "lock-frozen-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v1);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));

    let err = install(&ctx, &plane, &dir, ops::LockMode::Frozen)
        .expect_err("no lock entry covers the row");
    let detail = err.detail();
    assert!(detail.contains("--frozen"), "{detail}");
    assert!(detail.contains("deploy"), "{detail}");
    assert!(detail.contains("topos update"), "{detail}");
    assert!(!proj.0.join("topos.lock").exists(), "nothing written");
    assert!(!proj.0.join(".claude/skills/deploy").exists());
}

#[test]
fn a_name_overridden_row_keeps_its_lock_entry_through_update() {
    // Harvest writes the lock under the row's KEY; the closeout prune must key the same way —
    // pruning by display name deleted the entry the very same run had just written.
    let rig = Rig::new("lock-nameover");
    rig.seed_session();
    let proj = project(
        "lock-nameover-proj",
        &format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = {{ version = \"latest\", name = \"deploy-checklist\" }}\n"
        ),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v1);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));

    install(&ctx, &plane, &dir, ops::LockMode::Install).expect("fill");
    assert!(
        lock_text(&proj.0).contains("[skills.deploy]"),
        "{}",
        lock_text(&proj.0)
    );
    install(&ctx, &plane, &dir, ops::LockMode::Update).expect("update");
    assert!(
        lock_text(&proj.0).contains("[skills.deploy]"),
        "the update pruned the entry it should keep: {}",
        lock_text(&proj.0)
    );
}

#[test]
fn an_mcp_row_never_resolves_to_a_file_skill_of_the_same_name() {
    // The section is the kind: an [mcp] row whose name only a FILE skill answers is NOT
    // AVAILABLE — installing the file skill in its place would be the wrong bundle kind.
    let rig = Rig::new("lock-kindgate");
    rig.seed_session();
    let proj = project(
        "lock-kindgate-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v1);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));

    let out = install(&ctx, &plane, &dir, ops::LockMode::Install).expect("sweep runs");
    assert!(
        out.warnings.iter().any(|m| {
            m.code.as_deref() == Some("NOT_AVAILABLE") && m.text.contains("MCP server")
        }),
        "{:?}",
        out.warnings
    );
    assert!(
        !proj.0.join(".claude/skills/deploy").exists(),
        "the file skill must not install in an [mcp] row's place"
    );
}

#[test]
fn a_lock_from_another_workspace_refuses_install_and_update_re_resolves() {
    let rig = Rig::new("lock-foreign");
    rig.seed_session();
    let proj = project(
        "lock-foreign-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v1);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    std::fs::write(
        proj.0.join("topos.lock"),
        format!(
            "schema = 1\nworkspace = \"other.example/elsewhere\"\n\n[skills.deploy]\nversion = \"{}\"\n",
            hex(&v1)
        ),
    )
    .unwrap();

    // Install and frozen both refuse: entries are keyed by bare name, so another workspace's
    // lock would hand this one its resolutions.
    for mode in [ops::LockMode::Install, ops::LockMode::Frozen] {
        let err = install(&ctx, &plane, &dir, mode).expect_err("foreign lock");
        let detail = err.detail();
        assert!(detail.contains("other.example/elsewhere"), "{detail}");
        assert!(detail.contains(&format!("{HOST}/{WS_NAME}")), "{detail}");
        assert!(detail.contains("topos update"), "{detail}");
    }
    assert!(
        !proj.0.join(".claude/skills/deploy").exists(),
        "nothing placed"
    );

    // An update treats the workspace change as a full re-resolution: stale entries are
    // discarded, the rewrite is for THIS workspace, and the change is said.
    let out = install(&ctx, &plane, &dir, ops::LockMode::Update).expect("update re-resolves");
    assert!(
        out.warnings
            .iter()
            .any(|m| m.code.as_deref() == Some("LOCK_WORKSPACE_CHANGED")),
        "{:?}",
        out.warnings
    );
    let text = lock_text(&proj.0);
    assert!(
        text.contains(&format!("workspace = \"{HOST}/{WS_NAME}\"")),
        "{text}"
    );
    assert!(text.contains(&hex(&v1)), "{text}");
}

#[test]
fn frozen_reads_a_wrong_shaped_entry_as_a_disagreement() {
    let rig = Rig::new("lock-shape");
    rig.seed_session();
    let proj = project(
        "lock-shape-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v1);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));
    // A stale REPO-shaped block under a workspace row: coverage by name alone would pass it
    // and frozen would install whatever the catalog serves — unpinned.
    std::fs::write(
        proj.0.join("topos.lock"),
        format!(
            "schema = 1\nworkspace = \"{HOST}/{WS_NAME}\"\n\n[skills.deploy]\nsource = \"github:acme/tools\"\ncommit = \"9d0e8c17aa34bb56cc78\"\n"
        ),
    )
    .unwrap();

    let err = install(&ctx, &plane, &dir, ops::LockMode::Frozen).expect_err("wrong shape");
    let detail = err.detail();
    assert!(detail.contains("deploy"), "{detail}");
    assert!(detail.contains("repo source"), "{detail}");
    assert!(
        !proj.0.join(".claude/skills/deploy").exists(),
        "nothing placed"
    );
}

#[test]
fn a_channel_freezes_to_its_locked_member_list() {
    let rig = Rig::new("lock-chan");
    rig.seed_session();
    let proj = project(
        "lock-chan-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[channels]\nbackend = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let alpha = one_file(b"# alpha\n");
    let beta = one_file(b"# beta\n");
    let plane = FakePlane::new(log)
        .with_version("s_alpha", &alpha)
        .with_version("s_beta", &beta);
    let ctx = rig.ctx_at(Some(&proj.0));

    // First install: the channel resolves to its served members and the lock freezes the list.
    let dir = FakeDirectory::new(
        vec![catalog_entry("s_alpha", "alpha", &alpha)],
        vec![channel("backend", &[("s_alpha", "alpha")])],
    );
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    let text = lock_text(&proj.0);
    assert!(
        text.contains("[channels.backend]") && text.contains("members = [\"alpha\"]"),
        "{text}"
    );
    assert!(proj.0.join(".claude/skills/alpha/SKILL.md").exists());

    // The server adds beta to the channel. Install takes exactly the locked list — no beta.
    let grown = FakeDirectory::new(
        vec![
            catalog_entry("s_alpha", "alpha", &alpha),
            catalog_entry("s_beta", "beta", &beta),
        ],
        vec![channel(
            "backend",
            &[("s_alpha", "alpha"), ("s_beta", "beta")],
        )],
    );
    install(&ctx, &plane, &grown, ops::LockMode::Install).unwrap();
    assert!(
        !proj.0.join(".claude/skills/beta").exists(),
        "a frozen channel does not grow on install"
    );
    assert!(lock_text(&proj.0).contains("members = [\"alpha\"]"));

    // `topos update` takes the growth and rewrites the list.
    sweep(&ctx, &plane, &grown);
    assert!(proj.0.join(".claude/skills/beta/SKILL.md").exists());
    let text = lock_text(&proj.0);
    assert!(
        text.contains("members = [\"alpha\", \"beta\"]"),
        "the update took the new member:\n{text}"
    );
    assert!(text.contains("via = \"backend\""), "{text}");
}

#[test]
fn the_machine_scope_ignores_the_lock_and_stays_live() {
    let rig = Rig::new("lock-machine");
    rig.seed_session();
    rig.seed_feed();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let v2 = one_file(b"# v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    plane.serves(vec![delivered("s_deploy", "deploy", &v1)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&rig.work.0));

    // Delivered at v1 under install semantics (the hook posture)…
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    let placed = rig.skills().join("deploy/SKILL.md");
    assert_eq!(std::fs::read_to_string(&placed).unwrap(), "# v1\n");

    // …and the personal scope follows to v2 on the NEXT install: no lock stands here.
    plane.serves(vec![delivered_at("s_deploy", "deploy", &v2, 2)]);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v2)], Vec::new());
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    assert_eq!(
        std::fs::read_to_string(&placed).unwrap(),
        "# v2\n",
        "what follows the person stays live"
    );
}

#[test]
fn frozen_refuses_a_pin_the_lock_disagrees_with_and_writes_nothing() {
    let rig = Rig::new("lock-frozen-agree");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let v2 = one_file(b"# v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());

    // Install at v1 fills the lock.
    let proj = project(
        "lock-frozen-agree-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    let lock_before = lock_text(&proj.0);
    assert!(lock_before.contains(&hex(&v1)));

    // The toml then PINS v2 while the lock still records v1: frozen refuses on the
    // disagreement and neither file nor bytes move.
    std::fs::write(
        proj.0.join("topos.toml"),
        format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"{}\"\n",
            hex(&v2)
        ),
    )
    .unwrap();
    let err = install(&ctx, &plane, &dir, ops::LockMode::Frozen).unwrap_err();
    let detail = err.detail();
    assert!(detail.contains("disagrees"), "{detail}");
    assert!(detail.contains("deploy"), "{detail}");
    assert_eq!(lock_text(&proj.0), lock_before, "frozen wrote nothing");
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v1\n",
        "frozen moved no bytes"
    );
}

#[test]
fn a_toml_pin_reconciles_the_lock_on_plain_install() {
    let rig = Rig::new("lock-pin-fill");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let v2 = one_file(b"# v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let proj = project(
        "lock-pin-fill-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let ctx = rig.ctx_at(Some(&proj.0));
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    assert!(lock_text(&proj.0).contains(&hex(&v1)));

    // A hand-written PIN to v2 is intent: plain install reconciles the LOCK to it, places
    // the pinned bytes, and says so.
    std::fs::write(
        proj.0.join("topos.toml"),
        format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"{}\"\n",
            hex(&v2)
        ),
    )
    .unwrap();
    let out = install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    let text = lock_text(&proj.0);
    assert!(
        text.contains(&hex(&v2)),
        "lock reconciled to the pin:\n{text}"
    );
    assert!(!text.contains(&hex(&v1)));
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v2\n"
    );
    assert!(
        out.disclosures
            .iter()
            .any(|m| m.code.as_deref() == Some("LOCK_PINNED")),
        "{:?}",
        out.disclosures
    );
}

#[test]
fn a_lock_held_project_reports_held_once_and_never_fast_forwards_forever() {
    let rig = Rig::new("lock-held");
    rig.seed_session();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let v2 = one_file(b"# v2\n");
    let plane = FakePlane::new(log)
        .with_version("s_deploy", &v1)
        .with_version("s_deploy", &v2);
    let proj = project(
        "lock-held-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let ctx = rig.ctx_at(Some(&proj.0));

    // Lock fills at v1; the team then publishes v2.
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v2)], Vec::new());

    // Every later install: the row is HELD (named by the lock) — never a repeated
    // fast-forward, and the bytes stay the lock's.
    for _ in 0..3 {
        let out = install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
        let row = out
            .data
            .skills
            .iter()
            .find(|s| s.skill.contains("deploy"))
            .expect("a row");
        assert_ne!(
            row.action,
            PullAction::FastForwarded,
            "a stable pin is not work"
        );
        assert_eq!(row.action, PullAction::Held, "held, honestly");
        assert_eq!(
            std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
            "# v1\n"
        );
        assert!(lock_text(&proj.0).contains(&hex(&v1)), "lock never bumped");
    }

    // `update` is what moves it — it actually moves despite the generations, and the receipt
    // carries the promised old → new row.
    let out = sweep(&ctx, &plane, &dir);
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v2\n",
        "update re-resolves and lands"
    );
    assert!(lock_text(&proj.0).contains(&hex(&v2)));
    assert!(
        out.disclosures.iter().any(|m| {
            m.code.as_deref() == Some("LOCK_MOVED")
                && m.text.starts_with("deploy: ")
                && m.text.contains(" → ")
        }),
        "{:?}",
        out.disclosures
    );
}

// =================================================================================================
// The `[mcp]` half of the lock: a connected server's entry converges to the revision the lock
// names, exactly as a skill's converges to its locked version. The workspace serves one revision
// by id; a workspace that serves none is the degraded path below.
// =================================================================================================

/// A revision id of the shape the catalog mints (`mcpr_` + 32 hex).
fn rev(tag: &str) -> String {
    format!("mcpr_{}", tag.repeat(32 / tag.len()))
}

/// What the PROJECT's store recorded for a connected server — the whole durable state of the kind.
fn recorded(
    rig: &Rig,
    proj: &std::path::Path,
    skill_id: &str,
) -> topos_types::persisted::McpServerRecord {
    let layout = crate::sidecar::existing_project_store(&rig.fs, proj).expect("a project store");
    let sid = crate::id::SkillId::parse(skill_id).unwrap();
    crate::doc::read_doc(&rig.fs, &layout.published(&sid).server)
        .expect("the record reads")
        .expect("a record stands")
}

/// A checkout whose committed lock names an OLD revision, and a workspace that has moved on.
fn locked_mcp_project(tag: &str, old: &str) -> Scratch {
    let proj = project(
        tag,
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = \"latest\"\n"),
    );
    std::fs::write(
        proj.0.join("topos.lock"),
        format!(
            "schema = 1\nworkspace = \"{HOST}/{WS_NAME}\"\n\n[mcp.linear]\nrevision = \"{old}\"\n"
        ),
    )
    .unwrap();
    proj
}

#[test]
fn an_mcp_entry_converges_to_the_locked_revision_and_only_update_moves_it() {
    let rig = Rig::new("lock-mcp");
    rig.seed_session();
    let (old, new) = (rev("a"), rev("b"));
    let proj = locked_mcp_project("lock-mcp-proj", &old);
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    // The workspace serves the NEW revision as current, and still serves the old one by id.
    let dir = FakeDirectory::new(Vec::new(), Vec::new())
        .with_server(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v2",
            &new,
        ))
        .with_revision(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v1",
            &old,
        ));
    let ctx = rig.ctx_at(Some(&proj.0));

    // A FRESH CHECKOUT takes the lock's revision — the document a teammate received then, not
    // the one the catalog serves today.
    let out = install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let record = recorded(&rig, &proj.0, "s_linear");
    assert_eq!(record.revision_id, old);
    assert_eq!(
        record.document["remotes"][0]["url"], "https://mcp.example/v1",
        "the locked revision's own document: {record:?}"
    );
    assert!(lock_text(&proj.0).contains(&old), "the lock never moved");
    // Honoring a lock is not news — a skill's lock says nothing either.
    assert!(
        !out.disclosures
            .iter()
            .any(|m| m.code.as_deref() == Some("MCP_FILLED")),
        "{:?}",
        out.disclosures
    );

    // `--frozen` SUCCEEDS on that same fresh checkout: the revision the lock names is available.
    let frozen = Rig::new("lock-mcp-frozen");
    frozen.seed_session();
    let cold = locked_mcp_project("lock-mcp-frozen-proj", &old);
    let cold_ctx = frozen.ctx_at(Some(&cold.0));
    install(&cold_ctx, &plane, &dir, ops::LockMode::Frozen).expect("the locked revision is served");
    assert_eq!(recorded(&frozen, &cold.0, "s_linear").revision_id, old);

    // A repeat install holds there (the record already stands at the locked revision).
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    assert_eq!(recorded(&rig, &proj.0, "s_linear").revision_id, old);

    // `topos update` is what moves it — the lock and the entry both take the served revision.
    sweep(&ctx, &plane, &dir);
    let record = recorded(&rig, &proj.0, "s_linear");
    assert_eq!(record.revision_id, new);
    assert_eq!(
        record.document["remotes"][0]["url"],
        "https://mcp.example/v2"
    );
    let text = lock_text(&proj.0);
    assert!(text.contains(&new) && !text.contains(&old), "{text}");
}

#[test]
fn a_workspace_that_serves_no_revision_by_id_keeps_the_degraded_moves() {
    let rig = Rig::new("lock-mcp-old");
    rig.seed_session();
    let (old, new) = (rev("a"), rev("b"));
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    // A server too old to serve revisions by id: the catalog answers, the by-revision lane does
    // not — which is byte-for-byte what a revision that is simply gone looks like from here.
    let dir = FakeDirectory::new(Vec::new(), Vec::new())
        .with_server(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v2",
            &new,
        ))
        .serving_no_revisions();

    // `--frozen` REFUSES, names the fix, and writes nothing.
    let cold = locked_mcp_project("lock-mcp-old-frozen", &old);
    let cold_ctx = rig.ctx_at(Some(&cold.0));
    let err = install(&cold_ctx, &plane, &dir, ops::LockMode::Frozen).unwrap_err();
    assert!(err.detail().contains("--frozen"), "{}", err.detail());
    assert!(
        crate::sidecar::existing_project_store(&rig.fs, &cold.0)
            .and_then(|l| {
                let sid = crate::id::SkillId::parse("s_linear").unwrap();
                crate::doc::read_doc::<topos_types::persisted::McpServerRecord>(
                    &rig.fs,
                    &l.published(&sid).server,
                )
                .ok()
                .flatten()
            })
            .is_none(),
        "frozen recorded nothing"
    );

    // A plain install takes the served revision instead, says so, and the lock records what the
    // machine actually holds.
    let proj = locked_mcp_project("lock-mcp-old-proj", &old);
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    let filled = out
        .disclosures
        .iter()
        .find(|m| m.code.as_deref() == Some("MCP_FILLED"))
        .unwrap_or_else(|| panic!("{:?}", out.disclosures));
    assert!(
        filled.text.contains("does not serve that revision"),
        "{}",
        filled.text
    );
    assert_eq!(recorded(&rig, &proj.0, "s_linear").revision_id, new);
    let text = lock_text(&proj.0);
    assert!(text.contains(&new) && !text.contains(&old), "{text}");
}

#[test]
fn a_channels_mcp_member_converges_to_the_locked_revision_too() {
    // A channel carries members of both kinds, and the lock records each member's own entry —
    // so the server a channel brings honors its lock exactly as an explicit `[mcp]` row does.
    let rig = Rig::new("lock-mcp-channel");
    rig.seed_session();
    let (old, new) = (rev("a"), rev("b"));
    let proj = project(
        "lock-mcp-channel-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[channels]\ntools = \"latest\"\n"),
    );
    std::fs::write(
        proj.0.join("topos.lock"),
        format!(
            "schema = 1\nworkspace = \"{HOST}/{WS_NAME}\"\n\n[channels.tools]\nmembers = \
             [\"linear\"]\n\n[mcp.linear]\nrevision = \"{old}\"\n"
        ),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let mut dir = FakeDirectory::new(Vec::new(), Vec::new())
        .with_server(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v2",
            &new,
        ))
        .with_revision(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v1",
            &old,
        ));
    dir.channels = vec![channel("tools", &[("s_linear", "linear")])];
    let ctx = rig.ctx_at(Some(&proj.0));

    let out = install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    assert!(out.warnings.is_empty(), "{:?}", out.warnings);
    let record = recorded(&rig, &proj.0, "s_linear");
    assert_eq!(record.revision_id, old);
    assert_eq!(
        record.document["remotes"][0]["url"], "https://mcp.example/v1",
        "{record:?}"
    );
    assert!(lock_text(&proj.0).contains(&old));

    // And `update` moves the member's entry, the channel's member list beside it.
    sweep(&ctx, &plane, &dir);
    assert_eq!(recorded(&rig, &proj.0, "s_linear").revision_id, new);
    assert!(lock_text(&proj.0).contains(&new));
}

#[test]
fn a_locked_install_books_its_delivery_so_the_fleet_and_the_offline_surfaces_keep_it() {
    // A manifest-only server rides no channel, so NOTHING else in the sweep carries it: the
    // applied report's row set and the offline delivery cache are both built from what the
    // reconcile books here. A locked install that skipped that booking would drop the server out
    // of the fleet's state on every run — while the config entry it just wrote stands.
    let rig = Rig::new("lock-mcp-report");
    rig.seed_session();
    let (old, new) = (rev("a"), rev("b"));
    let proj = locked_mcp_project("lock-mcp-report-proj", &old);
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new())
        .with_server(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v2",
            &new,
        ))
        .with_revision(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v1",
            &old,
        ));
    let ctx = rig.ctx_at(Some(&proj.0));
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();

    // The applied report carries the bundle, AT THE REVISION THIS MACHINE HOLDS — the locked one,
    // never the catalog's current.
    let reported = plane.reported.lock().unwrap().clone();
    let row = reported
        .iter()
        .find(|(id, _, _)| id == "s_linear")
        .unwrap_or_else(|| panic!("the locked server is reported: {reported:?}"));
    assert_eq!(row.1, old, "the report names what is held");

    // …and the offline cache says the same, marked as the manifest row's delivery.
    let cache = crate::sync_status::read(&rig.fs, &rig.layout()).unwrap();
    let entry = cache
        .workspaces
        .get(WS)
        .unwrap_or_else(|| panic!("a cache row for the workspace: {cache:?}"));
    let cached = entry
        .delivered
        .get("s_linear")
        .unwrap_or_else(|| panic!("a cache row for the server: {entry:?}"));
    assert!(cached.via_manifest, "the manifest row is what delivers it");
    assert_eq!(cached.served_version, old, "held at the locked revision");
}

#[test]
fn a_locked_install_that_reaches_no_agent_says_so_instead_of_reading_installed() {
    // The lock decides WHICH document; `dest` decides WHERE it goes. A dest naming no MCP config
    // file places nothing, and the row must say that on the honored path exactly as it does on
    // the ordinary one — a receipt reading `up to date` over a machine holding nothing is the one
    // thing it may never say.
    let rig = Rig::new("lock-mcp-noreach");
    rig.seed_session();
    let (old, new) = (rev("a"), rev("b"));
    let proj = project(
        "lock-mcp-noreach-proj",
        &format!(
            "workspace = \"{HOST}/{WS_NAME}\"\n\n[mcp]\nlinear = {{ dest = \
             [\"not-a-config.json\"] }}\n"
        ),
    );
    std::fs::write(
        proj.0.join("topos.lock"),
        format!(
            "schema = 1\nworkspace = \"{HOST}/{WS_NAME}\"\n\n[mcp.linear]\nrevision = \"{old}\"\n"
        ),
    )
    .unwrap();
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let plane = FakePlane::new(log);
    let dir = FakeDirectory::new(Vec::new(), Vec::new())
        .with_server(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v2",
            &new,
        ))
        .with_revision(catalog_server_at(
            "s_linear",
            "linear",
            "https://mcp.example/v1",
            &old,
        ));
    let ctx = rig.ctx_at(Some(&proj.0));
    let out = install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();

    assert!(
        crate::message::legacy_lines(&out.warnings)
            .iter()
            .any(|w| w.contains("reaches no agent")),
        "{:?}",
        out.warnings
    );
    let row = out
        .data
        .skills
        .iter()
        .find(|s| s.skill == "linear")
        .unwrap_or_else(|| panic!("{:?}", out.data.skills));
    assert!(
        row.note
            .as_deref()
            .is_some_and(|n| n.starts_with("reaches no agent")),
        "the row states it reached nobody: {row:?}"
    );
}
