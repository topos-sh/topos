//! `POST /v1/login` — a login-intent device flow proves an identity, and the redeem re-mints one workspace
//! credential per confirmed seat. The credential is returned ONCE and works end-to-end.

use topos_types::requests::{
    DeviceAuthorizeResponse, DeviceTokenResponse, DeviceTokenStatus, LoginData,
};

use super::*;

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn login_redeems_one_credential_per_confirmed_seat(pool: PgPool) {
    let ctx = enroll_setup(pool, "login-redeem").await;
    let ws = WorkspaceId::parse(WS).unwrap();
    // Alice is a CONFIRMED member of w_acme — login re-mints her credential there.
    ctx.authority()
        .seed_workspace_member(
            &ws,
            &Principal::parse(ALICE_EMAIL).unwrap(),
            "member",
            "confirmed",
        )
        .await
        .unwrap();

    let device_pk = dev_pubkey(ALICE_SEED);

    // AUTHORIZE with intent=login and NO workspace.
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/authorize",
            serde_json::json!({
                "intent": "login",
                "device_public_key": b64key(&device_pk),
                "machine_name": "alice-laptop",
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let auth: DeviceAuthorizeResponse = serde_json::from_slice(&b).unwrap();
    // Every start carries the plane block (a login client learns the API base from the response).
    assert!(
        auth.plane.is_some(),
        "a login start carries the plane block"
    );

    // poll → pending, then prove the identity with a passcode (login sessions accept the passcode leg).
    let (_, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/token",
            serde_json::json!({ "device_code": auth.device_code }),
        ),
    )
    .await;
    let poll: DeviceTokenResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(poll.status, DeviceTokenStatus::Pending);

    let code = mint_passcode(&ctx, &auth.user_code, ALICE_EMAIL).await;
    let (_, _, _) = send(
        ctx.app(),
        post_nosig(
            "/v1/enroll/passcode/confirm",
            serde_json::json!({ "user_code": auth.user_code, "email": ALICE_EMAIL, "code": code }),
        ),
    )
    .await;

    // poll → granted. A LOGIN grant is workspace-less (no workspace block).
    let (_, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/token",
            serde_json::json!({ "device_code": auth.device_code }),
        ),
    )
    .await;
    let poll: DeviceTokenResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(poll.status, DeviceTokenStatus::Granted);
    assert!(
        poll.workspace.is_none(),
        "a login grant is workspace-less: {poll:?}"
    );
    let grant = poll.grant.expect("a granted login poll carries the grant");

    // REDEEM at POST /v1/login → one credential per confirmed seat.
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/login",
            serde_json::json!({ "grant": grant, "device_public_key": b64key(&device_pk) }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let env = envelope(&b);
    assert!(env.ok, "the login redeem is ok: {env:?}");
    assert_eq!(env.command, "login");
    let data: LoginData = serde_json::from_value(env.data).expect("OK data is LoginData");
    assert_eq!(data.principal, ALICE_EMAIL);
    let seat = data
        .memberships
        .iter()
        .find(|m| m.workspace_id == WS)
        .expect("a re-minted seat for w_acme");
    assert_eq!(seat.blocked, None, "the seat is not blocked: {seat:?}");
    let credential = seat.credential.clone().expect("a re-minted credential");

    // The credential WORKS end to end — it reads the workspace catalog.
    let (s_cat, _, _) = send(
        ctx.app(),
        get(
            &format!("/v1/workspaces/{WS}/skills"),
            &[("authorization", &format!("Bearer {credential}"))],
        ),
    )
    .await;
    assert_eq!(
        s_cat,
        StatusCode::OK,
        "the re-minted credential reads the catalog"
    );
}
