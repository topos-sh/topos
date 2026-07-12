//! `POST /v1/proposals` + the proposals-listing read `GET .../proposals`.

use super::*;

// ── propose ───────────────────────────────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn propose_opens_a_proposal_needs_review_without_moving_current(pool: PgPool) {
    let ctx = setup(pool, "propose-ok").await;
    let (g_vid, _) = seed_genesis(&ctx, "30000000-0000-4000-8000-000000000000").await;

    let op = "30000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"a proposed change\n")];
    let (vid, _digest) = compute_ids(&[g_vid], &files);

    let (status, _, bytes) = run(
        &ctx,
        post(
            "/v1/proposals",
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
async fn list_proposals_route_returns_open_proposals_and_404s_a_bad_credential(pool: PgPool) {
    // The proposals-listing read over `router(state)`: open a proposal via the HTTP propose route, then GET
    // the list with the device's workspace credential → 200 + the proposal's @hash + base; a bad/absent
    // credential → the indistinguishable 404 (never 401/403). A member reads ANY skill (the skill comes from
    // the path), so a different skill with no proposals is a 200 + empty list — not a 404.
    let ctx = setup(pool, "list-proposals").await;
    let (g_vid, _) = seed_genesis(&ctx, "70000000-0000-4000-8000-000000000000").await;

    // Open a proposal (a child of genesis) via `POST /v1/proposals`.
    let op = "70000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"a proposed change\n")];
    let (vid, _digest) = compute_ids(&[g_vid], &files);
    let (status, _, _) = run(
        &ctx,
        post(
            "/v1/proposals",
            candidate_body(op, gn(1, 1), &[g_vid], &files),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET the proposals list with the device's workspace credential → 200 + the one open proposal.
    let uri = format!("/v1/workspaces/{WS}/skills/{SKILL}/proposals");
    let (s_ok, headers, bytes) = run(
        &ctx,
        get(&uri, &[("authorization", &format!("Bearer {CREDENTIAL}"))]),
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

    // A bogus bearer credential → the indistinguishable 404.
    let (s_bad, _, _) = run(
        &ctx,
        get(&uri, &[("authorization", "Bearer not-a-real-credential")]),
    )
    .await;
    assert_eq!(s_bad, StatusCode::NOT_FOUND);

    // A DIFFERENT skill in the path with the SAME valid member credential → 200 + an empty list: catalog/
    // proposal visibility is workspace membership (the credential is not skill-scoped), and s_other simply
    // has no open proposals. (Under the retired read-token model this was a skill-scoped 404.)
    let other = format!("/v1/workspaces/{WS}/skills/s_other/proposals");
    let (s_other, _, other_bytes) = run(
        &ctx,
        get(
            &other,
            &[("authorization", &format!("Bearer {CREDENTIAL}"))],
        ),
    )
    .await;
    assert_eq!(s_other, StatusCode::OK);
    let other_list: topos_types::requests::WireProposalList =
        serde_json::from_slice(&other_bytes).expect("the body is a WireProposalList");
    assert!(
        other_list.proposals.is_empty(),
        "a skill with no open proposals lists empty for a member: {other_list:?}"
    );
}
