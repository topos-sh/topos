//! `POST /v1/proposals` + the proposals-listing read `GET .../proposals`.

use super::*;

// ── propose ───────────────────────────────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn propose_opens_a_proposal_needs_review_without_moving_current(pool: PgPool) {
    let ctx = setup(pool, "propose-ok").await;
    let (g_vid, _) = seed_genesis(&ctx, "30000000-0000-4000-8000-000000000000").await;

    let op = "30000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"a proposed change\n")];
    let (vid, digest) = compute_ids(&[g_vid], &files);
    let sig = sign_sig(
        &ctx.key,
        DeviceOp::PublishPropose,
        op,
        gn(1, 1),
        vid,
        digest,
    );

    let (status, _, bytes) = run(
        &ctx,
        post(
            "/v1/proposals",
            &sig,
            candidate_body(op, gn(1, 1), &[g_vid], &files),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "NEEDS_REVIEW is an ok outcome: {env:?}");
    let receipt = env.receipt.unwrap();
    assert_eq!(receipt.outcome, TerminalOutcome::NeedsReview);
    assert_eq!(
        receipt.version_id.as_deref(),
        Some(hex::encode(vid).as_str())
    );
    // A proposal moves no pointer, so the receipt reports no `current` generation.
    assert_eq!(receipt.current_generation, None);
    assert!(env.error.is_none(), "NEEDS_REVIEW is not an error");
}

// ── proposals listing: 200 + the open proposals, 404 on a bad token / scope mismatch ──────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn list_proposals_route_returns_open_proposals_and_404s_a_bad_token(pool: PgPool) {
    // The proposals-listing read over `router(state)`: open a proposal via the HTTP propose route, then GET
    // the list with the read token → 200 + the proposal's @hash + base; a bad/absent token or a scope/path
    // mismatch → the indistinguishable 404 (never 401/403).
    let ctx = setup(pool, "list-proposals").await;
    let (g_vid, _) = seed_genesis(&ctx, "70000000-0000-4000-8000-000000000000").await;

    // Open a proposal (a child of genesis) via `POST /v1/proposals`.
    let op = "70000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"a proposed change\n")];
    let (vid, digest) = compute_ids(&[g_vid], &files);
    let sig = sign_sig(
        &ctx.key,
        DeviceOp::PublishPropose,
        op,
        gn(1, 1),
        vid,
        digest,
    );
    let (status, _, _) = run(
        &ctx,
        post(
            "/v1/proposals",
            &sig,
            candidate_body(op, gn(1, 1), &[g_vid], &files),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET the proposals list with the read token → 200 + the one open proposal.
    let uri = format!("/v1/workspaces/{WS}/skills/{SKILL}/proposals");
    let (s_ok, headers, bytes) = run(
        &ctx,
        get(&uri, &[("authorization", &format!("Bearer {READ_TOKEN}"))]),
    )
    .await;
    assert_eq!(s_ok, StatusCode::OK);
    // The list is MUTABLE, so it carries a short must-revalidate window — never the version read's `immutable`.
    let cc = headers
        .get(header::CACHE_CONTROL)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        cc.contains("must-revalidate") && !cc.contains("immutable"),
        "the proposals list must not be immutable-cached: {cc}"
    );
    let list: topos_types::requests::WireProposalList =
        serde_json::from_slice(&bytes).expect("the body is a WireProposalList");
    assert_eq!(list.proposals.len(), 1);
    assert_eq!(list.proposals[0].version_id, hex::encode(vid));
    assert_eq!(list.proposals[0].base_generation, gn(1, 1));
    assert!(!list.proposals[0].created_at.is_empty());

    // A bogus bearer token → the indistinguishable 404.
    let (s_bad, _, _) = run(
        &ctx,
        get(&uri, &[("authorization", "Bearer not-a-real-token")]),
    )
    .await;
    assert_eq!(s_bad, StatusCode::NOT_FOUND);

    // A scope/path mismatch (a DIFFERENT skill in the path, valid token) → 404, BEFORE any roster read.
    let mismatch = format!("/v1/workspaces/{WS}/skills/s_other/proposals");
    let (s_mis, _, _) = run(
        &ctx,
        get(
            &mismatch,
            &[("authorization", &format!("Bearer {READ_TOKEN}"))],
        ),
    )
    .await;
    assert_eq!(s_mis, StatusCode::NOT_FOUND);
}
