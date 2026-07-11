//! The governance mutations (`POST /v1/invites`, `DELETE .../devices`): owner-OK, member-DENIED.

use super::*;

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_owner_device_invite_returns_invite_data(pool: PgPool) {
    let ctx = enroll_setup(pool, "enroll-invite-ok").await;
    let env = create_invite(
        &ctx,
        "dddddddd-0000-4000-8000-000000000001",
        &[ALICE_EMAIL],
        SKILL,
    )
    .await;
    assert!(env.ok, "an owner invite should be ok: {env:?}");
    assert_eq!(env.command, "invite");
    assert!(
        env.data["invite_link"]
            .as_str()
            .is_some_and(|l| l.contains("/i/"))
    );
    // The seeded roster + offered skills are echoed.
    assert!(
        env.data["roster_added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == ALICE_EMAIL)
    );
    assert!(
        env.data["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == SKILL)
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_member_device_invite_is_denied(pool: PgPool) {
    let ctx = enroll_setup(pool, "enroll-invite-denied").await;
    // A non-owner member device (governance requires the owner role for invite).
    let ws = WorkspaceId::parse(WS).unwrap();
    let member_principal = Principal::parse(MEMBER_PRINCIPAL).unwrap();
    ctx.authority()
        .seed_workspace_member(&ws, &member_principal, "member", "confirmed")
        .await
        .unwrap();
    ctx.authority()
        .seed_device(
            &ws,
            MEMBER_DK,
            &dev_pubkey(MEMBER_SEED),
            &member_principal,
            false,
        )
        .await
        .unwrap();

    let op = "eeeeeeee-0000-4000-8000-000000000001";
    let emails = [ALICE_EMAIL];
    let body = serde_json::json!({
        "workspace_id": WS,
        "op_id": op,
        "device_key_id": MEMBER_DK,
        "emails": emails,
        "role": "member",
        "skills": [{ "skill_id": SKILL, "name": "Deploy" }],
    });

    let (status, _, bytes) = send(ctx.app(), post_nosig("/v1/invites", body)).await;
    // A role-denial is a 200 + DENIED envelope (the actor is an authenticated member — nothing to hide).
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(!env.ok, "a member's invite must be denied: {env:?}");
    assert_eq!(
        env.error.expect("DENIED carries a WireError").outcome,
        TerminalOutcome::Denied
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_owner_revoke_of_a_device_is_ok(pool: PgPool) {
    let ctx = enroll_setup(pool, "enroll-revoke").await;
    // A target device for the owner to revoke.
    let ws = WorkspaceId::parse(WS).unwrap();
    let target_principal = Principal::parse(TARGET_PRINCIPAL).unwrap();
    ctx.authority()
        .seed_device(
            &ws,
            TARGET_DK,
            &dev_pubkey(TARGET_SEED),
            &target_principal,
            false,
        )
        .await
        .unwrap();

    let op = "ffffffff-0000-4000-8000-000000000001";
    let body = serde_json::json!({
        "workspace_id": WS,
        "op_id": op,
        "device_key_id": OWNER_DK,
        "target_device_key_id": TARGET_DK,
    });

    let (status, _, bytes) = send(
        ctx.app(),
        req_json("DELETE", &format!("/v1/workspaces/{WS}/devices"), body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "an owner revoke should be ok: {env:?}");
    assert_eq!(env.command, "revoke");
}
