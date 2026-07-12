//! `GET /i/{token}` — the unauthenticated CLAIM bootstrap (the only kind the door serves now; the tokened
//! invite door was interred).

use topos_types::bootstrap::BootstrapData;

use super::*;

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
    // The claim token is the LIVE one-time bearer owner capability: unlike an invite, the body must not
    // echo it anywhere (`token_id` is the empty placeholder) — a body-logging proxy learns nothing.
    assert_eq!(data.invite.token_id, "");
    let body = String::from_utf8(bytes.to_vec()).expect("utf-8 body");
    assert!(
        !body.contains(&token),
        "a claim /i/ body must never contain the claim token: {body}"
    );

    // Redeem it over the wire (the request display_name is disclosure-only — the row's name wins)…
    let device_pk = dev_pubkey(31);
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

// ── Content negotiation: one resource, two representations ───────────────────────────────────────────

/// The hosted split, end to end at the route: the minted CLAIM link rides the PUBLIC link base; an
/// `Accept: application/json` (the topos client) gets the machine contract, while curl's bare `*/*` and a
/// browser's html Accept get the markdown agent-instruction document — which NEVER echoes the token/link
/// (the one-time bearer custody rule), only the install line + the redeem step + the consent floor. Both
/// 200s are `no-store` + `Vary: accept`; a dead token stays the uniform JSON 404 either way.
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_claim_bootstrap_content_negotiates_json_and_agent_markdown(pool: PgPool) {
    const LINK_BASE: &str = "https://links.test";
    let ctx = enroll_setup_link_base(pool, "nego-claim", LINK_BASE).await;
    // The minted claim link STRING rides the public link base (the bin-side composer path).
    let link = ctx
        .state
        .mint_admin_claim("w_lb", Some("LB"), Some("owner@lb.test"), 3600)
        .await
        .expect("mint claim");
    assert!(
        link.starts_with(&format!("{LINK_BASE}/i/")),
        "minted on the link base: {link}"
    );
    let token = token_from_link(&link);

    // The topos client's explicit Accept ⇒ the JSON contract; the payload declares the API base (the
    // client re-roots onto it), not the link base.
    let (status, headers, bytes) = send(
        ctx.app(),
        get(&format!("/i/{token}"), &[("accept", "application/json")]),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data: BootstrapData = serde_json::from_slice(&bytes).expect("a BootstrapData");
    assert_eq!(data.plane.base_url, ENROLL_BASE_URL);
    assert_eq!(data.plane.enrollment_method, "admin_claim");
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    assert_eq!(headers.get("vary").unwrap(), "accept");

    // curl / an agent's web fetch (bare */*) ⇒ the instruction document (text/plain, so a browser
    // displays it inline) — and it NEVER contains the token or a link.
    let (status, headers, bytes) =
        send(ctx.app(), get(&format!("/i/{token}"), &[("accept", "*/*")])).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/plain"),
        "the instruction document for */*"
    );
    assert_eq!(headers.get("cache-control").unwrap(), "no-store");
    assert_eq!(headers.get("vary").unwrap(), "accept");
    assert_eq!(headers.get("x-robots-tag").unwrap(), "noindex");
    let doc = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(doc.contains("ONE-TIME workspace claim"));
    assert!(doc.contains("the link you just fetched"));
    assert!(doc.contains("releases/latest/download/install.sh"));
    assert!(doc.contains("LB"));
    assert!(!doc.contains("--resume"));
    assert!(
        !doc.contains(&token),
        "a claim markdown body must never contain the claim token: {doc}"
    );

    // A browser Accept takes the same document door (there is no HTML face).
    let (_, headers, _) = send(
        ctx.app(),
        get(
            &format!("/i/{token}"),
            &[("accept", "text/html,application/xhtml+xml,*/*;q=0.8")],
        ),
    )
    .await;
    assert!(
        headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );

    // NO Accept at all stays the machine-contract JSON (bare HTTP libraries; older clients).
    let (_, headers, bytes) = send(ctx.app(), get(&format!("/i/{token}"), &[])).await;
    assert!(
        headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let _: BootstrapData = serde_json::from_slice(&bytes).expect("still the JSON contract");

    // Errors never content-negotiate: a dead token is the uniform JSON envelope on every Accept.
    let (s404, headers, _) =
        send(ctx.app(), get("/i/not-a-real-token", &[("accept", "*/*")])).await;
    assert_eq!(s404, StatusCode::NOT_FOUND);
    assert!(
        headers
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
}

/// The claim door's markdown holds the same custody line as its JSON: the one-time bearer owner token
/// never appears in a response body — the document warns about the owner semantics and points the agent
/// at the URL it just fetched instead of echoing it.
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_claim_markdown_never_echoes_the_token(pool: PgPool) {
    let ctx = enroll_setup(pool, "nego-claim").await;
    let link = ctx
        .state
        .mint_admin_claim("w_newco", Some("Newco"), Some("owner@newco.com"), 3600)
        .await
        .expect("mint the claim link");
    let token = token_from_link(&link);

    let (status, _, bytes) =
        send(ctx.app(), get(&format!("/i/{token}"), &[("accept", "*/*")])).await;
    assert_eq!(status, StatusCode::OK);
    let doc = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(
        !doc.contains(&token),
        "a claim /i/ markdown body must never contain the claim token: {doc}"
    );
    assert!(doc.contains("ONE-TIME workspace claim"));
    assert!(doc.contains("becomes its OWNER"));
    assert!(doc.contains("the link you just fetched"));
    assert!(doc.contains("Newco"));
}
