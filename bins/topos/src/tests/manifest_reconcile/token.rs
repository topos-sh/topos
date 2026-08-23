//! The machine-token CI path: a `tpt_…` credential session serves a locked project's fetches
//! and the applied report, and NEVER dials the person feed — the token is read-only
//! server-side, and the engine must not ask a question it knows the answer refuses.

use std::sync::{Arc, Mutex};

use super::rig::{
    CallLog, FakeDirectory, FakePlane, HOST, Rig, WS_NAME, catalog_entry, connect, one_file,
    project,
};
use crate::ops;
use crate::sessions::{self, SESSION_ACTIVE, Session};

fn hex(v: &super::rig::Version) -> String {
    topos_core::digest::to_hex(&v.id)
}

fn token_session(rig: &Rig) {
    sessions::upsert_session(
        &rig.fs,
        &rig.layout(),
        Session {
            host: HOST.into(),
            base_url: format!("https://{HOST}/api"),
            // The ADDRESS rides the workspace-id slot — all a CI checkout knows is its
            // committed file's address; the server's token lane resolves either spelling.
            workspace_id: WS_NAME.into(),
            workspace_name: WS_NAME.into(),
            display_name: "machine token".into(),
            session_id: "sn_machine_token".into(),
            credential: "tpt_test".into(),
            status: SESSION_ACTIVE.into(),
            logged_in_at: 0,
        },
    )
    .unwrap();
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

#[test]
fn a_token_session_converges_a_project_and_never_dials_the_feed() {
    let rig = Rig::new("token-ci");
    token_session(&rig);
    let proj = project(
        "token-ci-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v1);
    // The feed lane would refuse a token — the engine must not even ask: were it dialed, this
    // fake would surface as an unreachable-workspace warning below.
    plane.serve_unreachable();
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));

    let out = install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();

    assert!(
        out.unreachable.is_empty(),
        "no feed was dialed, so nothing is unreachable: {:?}",
        out.warnings
    );
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v1\n",
        "the project converged over the token's read lanes"
    );
    let lock_text = std::fs::read_to_string(proj.0.join("topos.lock")).unwrap();
    assert!(lock_text.contains(&hex(&v1)), "{lock_text}");
}

#[test]
fn frozen_plus_token_is_the_ci_path_and_still_refuses_a_lock_gap() {
    let rig = Rig::new("token-frozen");
    token_session(&rig);
    let proj = project(
        "token-frozen-proj",
        &format!("workspace = \"{HOST}/{WS_NAME}\"\n\n[skills]\ndeploy = \"latest\"\n"),
    );
    let log: CallLog = Arc::new(Mutex::new(Vec::new()));
    let v1 = one_file(b"# v1\n");
    let plane = FakePlane::new(log).with_version("s_deploy", &v1);
    plane.serve_unreachable();
    let dir = FakeDirectory::new(vec![catalog_entry("s_deploy", "deploy", &v1)], Vec::new());
    let ctx = rig.ctx_at(Some(&proj.0));

    // No lock yet: frozen writes nothing, so the gap refuses.
    let err = install(&ctx, &plane, &dir, ops::LockMode::Frozen).unwrap_err();
    assert!(err.detail().contains("frozen"), "{}", err.detail());

    // Fill the lock (plain install), then the frozen run converges cleanly — the CI loop.
    install(&ctx, &plane, &dir, ops::LockMode::Install).unwrap();
    std::fs::remove_dir_all(proj.0.join(".claude")).unwrap();
    let out = install(&ctx, &plane, &dir, ops::LockMode::Frozen).unwrap();
    assert!(out.unreachable.is_empty());
    assert_eq!(
        std::fs::read_to_string(proj.0.join(".claude/skills/deploy/SKILL.md")).unwrap(),
        "# v1\n",
        "a fresh checkout under --frozen converges to the lock"
    );
}
