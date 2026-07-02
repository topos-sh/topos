//! The enrollment surface: the state wiring (`with_enroll_config` / `with_mailer`) and the full
//! device flow to a redeem (happy path + the wrong-device-key denial).

use topos_types::requests::RedeemResponse;

use super::*;

// ── enrollment wiring: with_enroll_config / with_mailer / the accessors ───────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn enroll_config_and_injected_mailer_are_readable(pool: PgPool) {
    use crate::enroll::mailer::{FakeMailer, MailContext, Passcode};

    let ctx = setup(pool, "state-enroll").await;
    let fake = Arc::new(FakeMailer::default());
    // with_enroll_config sets the static config (no SMTP ⇒ a NoopMailer); with_mailer overrides it for the
    // test so we can assert the handler sends through exactly the injected mailer.
    let state = ctx
        .state
        .clone()
        .with_enroll_config(crate::state::EnrollConfig {
            base_url: "https://plane.test".to_owned(),
            deployment_mode: plane_store::DeploymentMode::Cloud,
            enrollment_method: "passcode".to_owned(),
            smtp: None,
        })
        .with_mailer(fake.clone());

    assert_eq!(state.enroll().base_url, "https://plane.test");
    assert_eq!(state.enroll().enrollment_method, "passcode");
    assert_eq!(
        state.enroll().deployment_mode,
        plane_store::DeploymentMode::Cloud
    );

    // The accessor returns exactly the injected mailer — a send lands in the FakeMailer's record.
    let mail_ctx = MailContext {
        workspace_display_name: "Acme".to_owned(),
        base_url: "https://plane.test".to_owned(),
    };
    state
        .mailer()
        .send_passcode(
            "alice@acme.com",
            &Passcode::new("424242".to_owned()),
            &mail_ctx,
        )
        .unwrap();
    let sent = fake.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "alice@acme.com");
    assert_eq!(sent[0].code, "424242");
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn full_device_flow_enrolls_and_redeems_read_creds(pool: PgPool) {
    let ctx = enroll_setup(pool, "enroll-redeem").await;
    let (grant, user_code, device_pk, device_key) = enroll_to_grant(
        &ctx,
        "bbbbbbbb-0000-4000-8000-000000000001",
        ALICE_SEED,
        ALICE_EMAIL,
        SKILL,
    )
    .await;

    let device_key_id = device_key_id_for(&device_pk);
    let grant_hash = digest::sha256(grant.as_bytes());
    let sig = sign_enroll(
        &device_key,
        grant_hash,
        &user_code,
        &device_key_id,
        device_pk,
        &[SKILL],
    );
    let body = serde_json::json!({
        "workspace_id": WS,
        "grant": grant,
        "device_public_key": b64key(&device_pk),
    });

    let (status, _, bytes) = send(
        ctx.app(),
        signed_req("POST", &format!("/v1/workspaces/{WS}/devices"), &sig, body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "redeem should be ok: {env:?}");
    assert_eq!(env.command, "redeem");
    let resp: RedeemResponse =
        serde_json::from_value(env.data).expect("OK data is a RedeemResponse");
    assert_eq!(resp.workspace_id, WS);
    assert_eq!(resp.device_key_id, device_key_id);
    assert!(
        resp.read_creds.iter().any(|c| c.skill_id == SKILL),
        "a read cred for the offered skill is minted: {resp:?}"
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_redeem_with_a_wrong_device_key_is_denied(pool: PgPool) {
    let ctx = enroll_setup(pool, "enroll-wrongkey").await;
    let (grant, user_code, _device_pk, _device_key) = enroll_to_grant(
        &ctx,
        "cccccccc-0000-4000-8000-000000000001",
        ALICE_SEED,
        ALICE_EMAIL,
        SKILL,
    )
    .await;

    // Present a DIFFERENT device key than the grant binds → the grant's device-key match fails.
    let wrong = dev_key(99);
    let wrong_pk = wrong.verifying_key().to_bytes();
    let wrong_dk = device_key_id_for(&wrong_pk);
    let grant_hash = digest::sha256(grant.as_bytes());
    let sig = sign_enroll(
        &wrong,
        grant_hash,
        &user_code,
        &wrong_dk,
        wrong_pk,
        &[SKILL],
    );
    let body = serde_json::json!({
        "workspace_id": WS,
        "grant": grant,
        "device_public_key": b64key(&wrong_pk),
    });

    let (status, _, bytes) = send(
        ctx.app(),
        signed_req("POST", &format!("/v1/workspaces/{WS}/devices"), &sig, body),
    )
    .await;
    // A device-key mismatch is a 200 + DENIED envelope, never a 403.
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(!env.ok, "a wrong device key must be denied: {env:?}");
    assert_eq!(
        env.error.expect("DENIED carries a WireError").outcome,
        TerminalOutcome::Denied
    );
}
