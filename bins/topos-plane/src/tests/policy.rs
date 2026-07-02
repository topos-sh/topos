//! `PUT .../policy/review-required` — the admin-token self-host operator route.

use super::*;

// ── the self-host policy route: admin-token auth + the observable review-required flip ────────────────

fn put_policy(auth: Option<&str>, review_required: bool) -> Request<Body> {
    let body =
        serde_json::to_vec(&serde_json::json!({ "review_required": review_required })).unwrap();
    put_policy_raw(auth, body)
}

fn put_policy_raw(auth: Option<&str>, body: Vec<u8>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("PUT")
        .uri(format!("/v1/workspaces/{WS}/policy/review-required"))
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = auth {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    builder.body(Body::from(body)).unwrap()
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_policy_route_is_invisible_without_an_admin_token(pool: PgPool) {
    // No admin token configured ⇒ 404 (indistinguishable from a missing route), even with a bearer token
    // on the request — a composition that never sets the token can't expose an unauthenticated toggle.
    let ctx = setup(pool, "policy-off").await;
    let (status, _h, bytes) = run(&ctx, put_policy(Some("whatever"), true)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let env = envelope(&bytes);
    assert!(!env.ok);
    assert!(env.receipt.is_none(), "the authority was never reached");
    // A malformed body must NOT make the disabled route observable (auth is decided before any parse):
    // still the same indistinguishable 404, never a 400 body-parse oracle.
    let (status, _h, _b) = run(&ctx, put_policy_raw(Some("whatever"), b"not json".to_vec())).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_policy_route_toggles_review_required_observably(pool: PgPool) {
    let mut ctx = setup(pool, "policy-on").await;
    ctx.state = ctx.state.clone().with_admin_token("op_secret");
    let (g_vid, _) = seed_genesis(&ctx, "30000000-0000-4000-8000-000000000000").await;

    // Configured + missing/wrong token ⇒ an honest 401; nothing flips. A malformed body never masks the
    // auth answer (401 before any parse); only an AUTHENTICATED malformed body is a 400.
    let (status, _h, _b) = run(&ctx, put_policy(None, true)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _h, _b) = run(&ctx, put_policy(Some("wrong"), true)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _h, _b) = run(&ctx, put_policy_raw(Some("wrong"), b"not json".to_vec())).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _h, _b) = run(
        &ctx,
        put_policy_raw(Some("op_secret"), b"not json".to_vec()),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The right token ⇒ 204, and the flip is OBSERVABLE: a direct publish now fails typed.
    let (status, _h, _b) = run(&ctx, put_policy(Some("op_secret"), true)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let op = "30000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"gated update\n")];
    let parents = [g_vid];
    let (vid, digest) = compute_ids(&parents, &files);
    let sig = sign_sig(&ctx.key, DeviceOp::PublishDirect, op, gn(1, 1), vid, digest);
    let (status, _h, bytes) = run(
        &ctx,
        post(
            "/v1/publish",
            &sig,
            candidate_body(op, gn(1, 1), &parents, &files),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a protocol outcome is always a 200");
    let env = envelope(&bytes);
    assert!(!env.ok);
    let receipt = env.receipt.expect("a gated receipt");
    assert_eq!(receipt.outcome, TerminalOutcome::ApprovalRequired);

    // Toggle OFF ⇒ 204 (idempotent set), and a fresh direct publish is OK again.
    let (status, _h, _b) = run(&ctx, put_policy(Some("op_secret"), false)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let op2 = "30000000-0000-4000-8000-000000000002";
    let sig2 = sign_sig(
        &ctx.key,
        DeviceOp::PublishDirect,
        op2,
        gn(1, 1),
        vid,
        digest,
    );
    let (status, _h, bytes) = run(
        &ctx,
        post(
            "/v1/publish",
            &sig2,
            candidate_body(op2, gn(1, 1), &parents, &files),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(
        env.ok,
        "with the gate off the direct publish lands: {env:?}"
    );
    assert_eq!(
        env.receipt.expect("an OK receipt").current_generation,
        Some(gn(1, 2))
    );
}
