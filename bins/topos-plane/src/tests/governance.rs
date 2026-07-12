//! The governance mutations (`PUT/DELETE .../roster/{email}`, `DELETE .../devices`): owner-OK, member-DENIED.
//! (Invitation is no longer a governance op — it moved to the member-lane `POST .../invitations` route; see
//! `tests/invitations.rs`.)

use super::*;

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_owner_roster_set_is_ok_and_a_member_is_denied(pool: PgPool) {
    let ctx = enroll_setup(pool, "gov-roster-set").await;
    let ws = WorkspaceId::parse(WS).unwrap();
    // A member device (a non-owner) to prove the role gate, plus a seat to raise.
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
            MEMBER_CRED,
        )
        .await
        .unwrap();

    // The owner raises the member to reviewer — a 200 OK envelope.
    let body = serde_json::json!({ "workspace_id": WS, "op_id": "d0000000-0000-4000-8000-000000000001", "role": "reviewer" });
    let (status, _, bytes) = send(
        ctx.app(),
        req_json_auth(
            "PUT",
            &format!("/v1/workspaces/{WS}/roster/{MEMBER_PRINCIPAL}"),
            body,
            OWNER_CRED,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(envelope(&bytes).ok, "an owner roster-set is ok");

    // The member attempting the same is a 200 + DENIED (the actor is authenticated — nothing to hide).
    let body = serde_json::json!({ "workspace_id": WS, "op_id": "d0000000-0000-4000-8000-000000000002", "role": "owner" });
    let (status, _, bytes) = send(
        ctx.app(),
        req_json_auth(
            "PUT",
            &format!("/v1/workspaces/{WS}/roster/{OWNER_PRINCIPAL}"),
            body,
            MEMBER_CRED,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(!env.ok, "a member's roster-set must be denied: {env:?}");
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
            TARGET_CRED,
        )
        .await
        .unwrap();

    let op = "ffffffff-0000-4000-8000-000000000001";
    // The acting owner rides the Bearer credential (OWNER_CRED); the body names only the TARGET device id.
    let body = serde_json::json!({
        "workspace_id": WS,
        "op_id": op,
        "target_device_key_id": TARGET_DK,
    });

    let (status, _, bytes) = send(
        ctx.app(),
        req_json_auth(
            "DELETE",
            &format!("/v1/workspaces/{WS}/devices"),
            body,
            OWNER_CRED,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "an owner revoke should be ok: {env:?}");
    assert_eq!(env.command, "revoke");
}
