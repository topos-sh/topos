//! The enrollment surface: the state wiring (`with_enroll_config` / `with_mailer`) and the full
//! device flow to a redeem (happy path + the wrong-device-key binding denial).

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
async fn full_device_flow_enrolls_and_redeems_a_workspace_credential(pool: PgPool) {
    let ctx = enroll_setup(pool, "enroll-redeem").await;
    let (grant, _user_code, device_pk) = enroll_to_grant(&ctx, ALICE_SEED, ALICE_EMAIL).await;

    let device_key_id = device_key_id_for(&device_pk);
    // The grant is the bearer credential; the body presents the matching device public key. No signature.
    let body = serde_json::json!({
        "workspace_id": WS,
        "grant": grant,
        "device_public_key": b64key(&device_pk),
    });

    let (status, _, bytes) = send(
        ctx.app(),
        post_nosig(&format!("/v1/workspaces/{WS}/devices"), body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "redeem should be ok: {env:?}");
    assert_eq!(env.command, "redeem");
    // The redeem mints the device's ONE workspace credential — never a per-skill `read_creds` bundle.
    assert!(
        env.data.get("read_creds").is_none(),
        "the retired per-skill read_creds must not appear: {:?}",
        env.data
    );
    let resp: RedeemResponse =
        serde_json::from_value(env.data).expect("OK data is a RedeemResponse");
    assert_eq!(resp.workspace_id, WS);
    assert_eq!(resp.device_key_id, device_key_id);
    let credential = resp.credential.clone();
    assert!(
        !credential.is_empty(),
        "one workspace credential is minted: {resp:?}"
    );

    // A redeem REPLAY (the deterministic grant) re-derives the IDENTICAL credential (idempotent lost-ack).
    let (s2, _, b2) = send(
        ctx.app(),
        post_nosig(&format!("/v1/workspaces/{WS}/devices"), body),
    )
    .await;
    assert_eq!(s2, StatusCode::OK);
    let resp2: RedeemResponse =
        serde_json::from_value(envelope(&b2).data).expect("OK data is a RedeemResponse");
    assert_eq!(
        resp2.credential, credential,
        "a redeem replay must re-derive the identical credential"
    );

    // …and the minted credential WORKS end-to-end: alice is now a confirmed member (cloud redeem flipped her
    // invited seat to confirmed), so her credential reads the workspace catalog (empty here — no skill yet).
    let (s_cat, _, cat_bytes) = send(
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
        "the minted credential reads the catalog"
    );
    let _: topos_types::requests::WireSkillIndex =
        serde_json::from_slice(&cat_bytes).expect("the catalog body is a WireSkillIndex");
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_redeem_with_a_wrong_device_key_is_denied(pool: PgPool) {
    let ctx = enroll_setup(pool, "enroll-wrongkey").await;
    let (grant, _user_code, _device_pk) = enroll_to_grant(&ctx, ALICE_SEED, ALICE_EMAIL).await;

    // Present a DIFFERENT device public key than the grant is bound to → the redeem's binding check fails
    // (the leaked-grant-on-another-device case).
    let wrong_pk = dev_pubkey(99);
    let body = serde_json::json!({
        "workspace_id": WS,
        "grant": grant,
        "device_public_key": b64key(&wrong_pk),
    });

    let (status, _, bytes) = send(
        ctx.app(),
        post_nosig(&format!("/v1/workspaces/{WS}/devices"), body),
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
    let device_pk = dev_pubkey(21);

    // AUTHORIZE with intent=standup and NO invite: the response carries the high-entropy code, the
    // complete verification URI on the verify base, and the plane block.
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
    assert!(
        auth.user_code.len() >= 40
            && auth
                .user_code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "standup code is a high-entropy opaque URL-safe token, got {:?}",
        auth.user_code
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
    assert_eq!(
        plane.base_url, ENROLL_BASE_URL,
        "the plane block declares the API base"
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
        .approve_standup(&auth.user_code, "founder@newco.com", Some("Newco"), None)
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

    // REDEEM over the wire — the grant bears the FRESH workspace; the body presents the matching device key.
    let body = serde_json::json!({
        "workspace_id": ws_block.workspace_id,
        "grant": grant,
        "device_public_key": b64key(&device_pk),
    });
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            &format!("/v1/workspaces/{}/devices", ws_block.workspace_id),
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
async fn authorize_contradictory_intent_and_workspace_are_400(pool: PgPool) {
    let ctx = enroll_setup(pool, "authorize-400").await;
    let device_pk = dev_pubkey(22);
    // Each (intent, workspace) contradiction is a fail-closed 400.
    let contradictions = [
        // enroll needs a workspace…
        serde_json::json!({ "intent": "enroll" }),
        // …standup takes none…
        serde_json::json!({ "intent": "standup", "workspace": WS_NAME }),
        // …and login takes none.
        serde_json::json!({ "intent": "login", "workspace": WS_NAME }),
    ];
    for extra in contradictions {
        let mut body = serde_json::json!({
            "device_public_key": b64key(&device_pk),
            "machine_name": "laptop",
        });
        body.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let shown = body.to_string();
        let (s, _, _) = send(ctx.app(), post_nosig("/v1/device/authorize", body)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "expected 400 for {shown}");
    }
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn standup_authorize_on_a_self_host_plane_is_the_uniform_404(pool: PgPool) {
    let ctx = enroll_setup_mode(pool, "standup-selfhost", DeploymentMode::SelfHost).await;
    let device_pk = dev_pubkey(23);
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
