//! The per-skill advisory DISPLAY NAME — the author's skill-folder name the plane records on the
//! catalog row (UNSIGNED, last-writer-wins), serves on the session-read
//! skill index, and NEVER lets touch the byte-exact digest or a version id. A revert / a name-less publish
//! keeps the existing name (COALESCE, not a clobber to NULL).

use super::*;
use crate::enroll::DeploymentMode;

const CLOUD: DeploymentMode = DeploymentMode::Cloud;

/// Drive a genesis publish carrying `display_name` (the device `"dk"` must already be registered + rostered);
/// returns the OK receipt.
async fn publish_named(
    fx: &Fixture,
    key: &[u8; 32],
    w: &WorkspaceId,
    s: &SkillId,
    op_id_str: &str,
    files: Vec<UploadedFile>,
    display_name: Option<&str>,
) -> crate::SetCurrentReceipt {
    let (staged, device) = prepare(
        fx,
        key,
        "dk",
        w,
        s,
        DeviceOp::PublishDirect,
        op_id_str,
        genesis(files),
        gn(0, 0),
    )
    .await;
    crate::set_current::publish(
        &fx.authority,
        w,
        s,
        &staged,
        &device,
        display_name,
        None,
        CREATED_AT,
        NOW,
    )
    .await
    .unwrap()
}

/// The session-index display name for a skill (a confirmed member must be seated).
async fn index_name(a: &Authority, w: &WorkspaceId, skill_id: &str) -> Option<String> {
    let index = a
        .list_skills_session(w, "member@acme.com", CLOUD)
        .await
        .unwrap();
    index
        .into_iter()
        .find(|r| r.skill_id == skill_id)
        .and_then(|r| r.display_name)
}

/// A genesis publish records its `display_name` on `current`; the session index serves it, and a
/// name-less publish leaves it NULL (served as `None`).
#[sqlx::test]
async fn a_genesis_publish_records_the_name_and_the_index_serves_it(pool: PgPool) {
    let fx = Fixture::new(pool, "dn-index").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (named, anon) = (skill("s_named"), skill("s_anon"));
    let key = dev_key(80);
    register(&fx, &w, &named, "dk", &key, "p_author").await;
    register(&fx, &w, &anon, "dk", &key, "p_author").await;
    a.db()
        .seed_workspace_member(&w, &prin("member@acme.com"), "member", "confirmed")
        .await
        .unwrap();

    publish_named(
        &fx,
        &key,
        &w,
        &named,
        "80000000-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"ship it")],
        Some("Deploy Skill"),
    )
    .await;
    publish_named(
        &fx,
        &key,
        &w,
        &anon,
        "80000000-0000-4000-8000-000000000002",
        vec![file("SKILL.md", b"quietly")],
        None,
    )
    .await;

    assert_eq!(
        index_name(a, &w, "s_named").await.as_deref(),
        Some("Deploy Skill")
    );
    // A publish that carried no name leaves the pointer's display name NULL (served as None).
    assert_eq!(index_name(a, &w, "s_anon").await, None);
}

/// The display name is ADVISORY: it is not in the commit frame or the bundle digest, so the SAME bytes
/// yield the SAME `version_id` + `bundle_digest` whether a name is carried or not. (Two workspaces so the
/// identical genesis commit does not collide on the per-workspace `skill_commit` PK.)
#[sqlx::test]
async fn the_name_never_enters_the_version_id_or_the_digest(pool: PgPool) {
    let fx = Fixture::new(pool, "dn-digest").await;
    let (w1, w2) = (ws("w_one"), ws("w_two"));
    let s = skill("s_deploy");
    let key = dev_key(81);
    register(&fx, &w1, &s, "dk", &key, "p_author").await;
    register(&fx, &w2, &s, "dk", &key, "p_author").await;

    let bytes = b"byte-identical bundle content";
    let named = publish_named(
        &fx,
        &key,
        &w1,
        &s,
        "81000000-0000-4000-8000-000000000001",
        vec![file("SKILL.md", bytes)],
        Some("A Loud Name"),
    )
    .await;
    let anon = publish_named(
        &fx,
        &key,
        &w2,
        &s,
        "81000000-0000-4000-8000-000000000002",
        vec![file("SKILL.md", bytes)],
        None,
    )
    .await;

    assert!(named.is_ok() && anon.is_ok());
    // Identical bytes ⇒ identical id + digest, regardless of the (unsigned) display name.
    assert_eq!(named.version_id, anon.version_id);
    assert_eq!(named.bundle_digest, anon.bundle_digest);
}

/// Last-writer-wins AMONG writers that express a name: a name-less pointer move (a revert would be the same
/// shape) keeps the current name; the next publish that carries one updates it.
#[sqlx::test]
async fn a_name_less_move_keeps_the_name_and_the_next_named_move_updates_it(pool: PgPool) {
    let fx = Fixture::new(pool, "dn-lww").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let s = skill("s_deploy");
    let key = dev_key(83);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    a.db()
        .seed_workspace_member(&w, &prin("member@acme.com"), "member", "confirmed")
        .await
        .unwrap();

    // Genesis names the skill.
    publish_named(
        &fx,
        &key,
        &w,
        &s,
        "83000000-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v1")],
        Some("First Name"),
    )
    .await;
    assert_eq!(
        index_name(a, &w, "s_deploy").await.as_deref(),
        Some("First Name")
    );

    // A child publish that carries NO name must NOT blank the existing one (COALESCE, not a clobber).
    let parent = current_commit(&fx, &w, &s).await;
    let (staged, device) = prepare(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "83000000-0000-4000-8000-000000000002",
        child(parent, vec![file("SKILL.md", b"v2")]),
        gn(1, 1),
    )
    .await;
    crate::set_current::publish(
        &fx.authority,
        &w,
        &s,
        &staged,
        &device,
        None,
        None,
        CREATED_AT,
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        index_name(a, &w, "s_deploy").await.as_deref(),
        Some("First Name")
    );

    // A child publish that DOES carry a name wins (last-writer-wins).
    let parent = current_commit(&fx, &w, &s).await;
    let (staged, device) = prepare(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "83000000-0000-4000-8000-000000000003",
        child(parent, vec![file("SKILL.md", b"v3")]),
        gn(1, 2),
    )
    .await;
    crate::set_current::publish(
        &fx.authority,
        &w,
        &s,
        &staged,
        &device,
        Some("Second Name"),
        None,
        CREATED_AT,
        NOW,
    )
    .await
    .unwrap();
    assert_eq!(
        index_name(a, &w, "s_deploy").await.as_deref(),
        Some("Second Name")
    );
}
