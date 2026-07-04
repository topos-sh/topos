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
            verify_base_url: "https://plane.test".to_owned(),
            link_base_url: "https://plane.test".to_owned(),
            strict_deployment_mode: Some(plane_store::DeploymentMode::Cloud),
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
        verify_base_url: "https://plane.test".to_owned(),
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

// ── the STANDUP door over the wire (authorize → verify → approve → poll → redeem) ─────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn standup_authorize_to_redeem_over_the_wire(pool: PgPool) {
    use topos_types::requests::{
        DeviceAuthorizeResponse, DeviceTokenResponse, DeviceTokenStatus, SessionIntent,
        VerificationContextResponse,
    };

    let ctx = enroll_setup(pool, "standup-wire").await;
    let device = dev_key(21);
    let device_pk = device.verifying_key().to_bytes();

    // AUTHORIZE with intent=standup and NO invite: the response carries the high-entropy code, the
    // complete verification URI on the verify base, and the plane block to TOFU-pin.
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/authorize",
            serde_json::json!({
                "intent": "standup",
                "device_public_key": b64key(&device_pk),
                "machine_name": "founder-laptop",
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let auth: DeviceAuthorizeResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        auth.user_code.len(),
        19,
        "standup code = 16 chars + 3 dashes"
    );
    assert_eq!(auth.verification_uri, format!("{ENROLL_BASE_URL}/verify"));
    assert_eq!(
        auth.verification_uri_complete.as_deref(),
        Some(format!("{ENROLL_BASE_URL}/verify/{}", auth.user_code).as_str())
    );
    let plane = auth.plane.expect("a standup start carries the plane block");
    assert_eq!(
        plane.deployment_mode,
        topos_types::bootstrap::DeploymentMode::Cloud
    );
    let expected_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(ctx.authority().plane_public_key().unwrap());
    assert_eq!(
        plane.signing_key.value, expected_key,
        "the TOFU root is the plane key"
    );

    // The verification disclosure: intent standup, no workspace yet ("" — the page renders standup copy).
    let (s, _, b) = send(
        ctx.app(),
        get(&format!("/v1/enroll/verify/{}", auth.user_code), &[]),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let verify: VerificationContextResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(verify.intent, Some(SessionIntent::Standup));
    assert_eq!(verify.workspace_display_name, "");
    assert!(verify.offered_skills.is_empty());

    // The WEB LEG (a composing plane's authenticated route) approves through the leak-free wrapper.
    let approved = ctx
        .state
        .approve_standup(&auth.user_code, "founder@newco.com", Some("Newco"))
        .await
        .unwrap();
    let crate::ApproveStandupSummary::Approved {
        workspace_id,
        display_name,
    } = approved
    else {
        panic!("expected Approved, got {approved:?}");
    };
    assert_eq!(display_name, "Newco");

    // POLL: granted, with the workspace context a standup client lacks.
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/token",
            serde_json::json!({ "device_code": auth.device_code }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let poll: DeviceTokenResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(poll.status, DeviceTokenStatus::Granted);
    let grant = poll.grant.expect("grant");
    let ws_block = poll
        .workspace
        .expect("a granted poll carries the workspace context");
    assert_eq!(ws_block.workspace_id, workspace_id);
    assert_eq!(ws_block.display_name, "Newco");

    // REDEEM over the wire — the possession frame binds the FRESH workspace + the empty offered set.
    let device_key_id = device_key_id_for(&device_pk);
    let grant_hash = digest::sha256(grant.as_bytes());
    let fields = sign::EnrollFields {
        workspace_id: &ws_block.workspace_id,
        grant_hash,
        device_auth_id: &auth.user_code,
        device_key_id: &device_key_id,
        device_public_key: device_pk,
        offered_skill_ids: &[],
    };
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(device.sign(&enroll_preimage(&fields).unwrap()).to_bytes());
    let body = serde_json::json!({
        "workspace_id": ws_block.workspace_id,
        "grant": grant,
        "device_public_key": b64key(&device_pk),
    });
    let (s, _, b) = send(
        ctx.app(),
        signed_req(
            "POST",
            &format!("/v1/workspaces/{}/devices", ws_block.workspace_id),
            &sig,
            body,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let env = envelope(&b);
    assert!(env.ok, "the standup owner's redeem admits: {env:?}");
    let resp: RedeemResponse = serde_json::from_value(env.data).unwrap();
    assert_eq!(resp.workspace_id, ws_block.workspace_id);
    assert_eq!(
        resp.principal.as_deref(),
        Some("founder@newco.com"),
        "the redeem discloses the seated owner (hijack visibility)"
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn standup_authorize_contradictory_bodies_are_400(pool: PgPool) {
    let ctx = enroll_setup(pool, "standup-400").await;
    let device_pk = dev_key(22).verifying_key().to_bytes();
    // intent=enroll with NO invite token.
    let (s, _, _) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/authorize",
            serde_json::json!({
                "intent": "enroll",
                "device_public_key": b64key(&device_pk),
                "machine_name": "laptop",
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // intent=standup WITH an invite token.
    let (s, _, _) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/authorize",
            serde_json::json!({
                "intent": "standup",
                "invite_token": "tok",
                "device_public_key": b64key(&device_pk),
                "machine_name": "laptop",
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn standup_authorize_on_a_self_host_plane_is_the_uniform_404(pool: PgPool) {
    let ctx = enroll_setup_mode(pool, "standup-selfhost", DeploymentMode::SelfHost).await;
    let device_pk = dev_key(23).verifying_key().to_bytes();
    let (s, _, _) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/authorize",
            serde_json::json!({
                "intent": "standup",
                "device_public_key": b64key(&device_pk),
                "machine_name": "laptop",
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
