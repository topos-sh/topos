//! `GET /i/{token}` — the unauthenticated TOFU bootstrap.

use topos_types::SignatureAlg;
use topos_types::bootstrap::{BootstrapData, ConsentMode};

use super::*;

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn invite_bootstrap_returns_the_pinned_plane_key_no_role_and_auto_land_false(pool: PgPool) {
    let ctx = enroll_setup(pool, "enroll-bootstrap").await;
    let env = create_invite(
        &ctx,
        "aaaaaaaa-0000-4000-8000-000000000001",
        &[ALICE_EMAIL],
        SKILL,
    )
    .await;
    let token = token_from_link(env.data["invite_link"].as_str().unwrap());

    let (status, _, bytes) = send(ctx.app(), get(&format!("/i/{token}"), &[])).await;
    assert_eq!(status, StatusCode::OK);
    let data: BootstrapData = serde_json::from_slice(&bytes).expect("the body is a BootstrapData");
    // An INVITE bootstrap still echoes the link token as the non-secret token_id (a shareable link's
    // own tail) — the claim door, by contrast, must not (see the claim test below).
    assert_eq!(data.invite.token_id, token);
    // The plane signing key is pinned (the trust root the device TOFU-pins).
    assert_eq!(data.plane.signing_key.alg, SignatureAlg::Ed25519);
    assert!(!data.plane.signing_key.key_id.is_empty());
    assert!(!data.plane.signing_key.value.is_empty());
    // No role; a first-received skill is never silently landed; the offered skill is disclosed.
    assert!(!data.invite.first_receive_auto_land);
    assert_eq!(data.invite.consent, ConsentMode::DirectHumanFirstReceive);
    assert_eq!(data.workspace.workspace_id, WS);
    assert_eq!(
        data.plane.deployment_mode,
        topos_types::bootstrap::DeploymentMode::Cloud
    );
    assert!(data.offered_skills.iter().any(|s| s.skill_id == SKILL));
    // The bootstrap carries no role anywhere.
    let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(raw.get("role").is_none() && raw["invite"].get("role").is_none());

    // A bad/unknown token ⇒ the indistinguishable 404.
    let (s404, _, _) = send(ctx.app(), get("/i/not-a-real-token", &[])).await;
    assert_eq!(s404, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_claim_link_bootstraps_with_the_admin_claim_method_until_redeemed(pool: PgPool) {
    let ctx = enroll_setup(pool, "claim-bootstrap").await;
    // Mint through the leak-free wrapper (the same call the bin's `mint-claim` subcommand makes).
    let link = ctx
        .state
        .mint_admin_claim("w_newco", Some("Newco"), Some("owner@newco.com"), 3600)
        .await
        .expect("mint the claim link");
    let token = token_from_link(&link);

    // The claim serves through the SAME /i/ route: the workspace-to-be's identity, NO skills, and the
    // admin_claim enrollment method the client branches on.
    let (status, _, bytes) = send(ctx.app(), get(&format!("/i/{token}"), &[])).await;
    assert_eq!(status, StatusCode::OK);
    let data: BootstrapData = serde_json::from_slice(&bytes).expect("a BootstrapData");
    assert_eq!(data.workspace.workspace_id, "w_newco");
    assert_eq!(data.workspace.display_name, "Newco");
    assert_eq!(data.plane.enrollment_method, "admin_claim");
    assert!(data.offered_skills.is_empty());
    assert!(
        !data.plane.signing_key.value.is_empty(),
        "the TOFU root rides the claim bootstrap"
    );
    // The claim token is the LIVE one-time bearer owner capability: unlike an invite, the body must not
    // echo it anywhere (`token_id` is the empty placeholder) — a body-logging proxy learns nothing.
    assert_eq!(data.invite.token_id, "");
    let body = String::from_utf8(bytes.to_vec()).expect("utf-8 body");
    assert!(
        !body.contains(&token),
        "a claim /i/ body must never contain the claim token: {body}"
    );

    // Redeem it over the wire (the request display_name is disclosure-only — the row's name wins)…
    let device = dev_key(31);
    let device_pk = device.verifying_key().to_bytes();
    let (status, _, bytes) = send(
        ctx.app(),
        post_nosig(
            "/v1/admin-claim",
            serde_json::json!({
                "claim_token": token,
                "device_public_key": b64key(&device_pk),
                "display_name": "Adversarial Name",
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "the claim redeem stands the workspace up: {env:?}");
    assert_eq!(env.data["workspace_id"], "w_newco");
    assert_eq!(
        env.data["principal"], "owner@newco.com",
        "the seated owner is the MINT-bound email"
    );

    // …after which the /i/ link is the uniform 404 (consumed).
    let (s404, _, _) = send(ctx.app(), get(&format!("/i/{token}"), &[])).await;
    assert_eq!(s404, StatusCode::NOT_FOUND);
}
