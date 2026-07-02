//! `POST /v1/reviews` — approve promotes an open proposal.

use super::*;

// ── review ────────────────────────────────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn review_approve_promotes_an_open_proposal(pool: PgPool) {
    let ctx = setup(pool, "review-ok").await;
    let (g_vid, _) = seed_genesis(&ctx, "50000000-0000-4000-8000-000000000000").await;

    // Open a proposal at base (1,1).
    let op_p = "50000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"the reviewed change\n")];
    let (prop_vid, prop_digest) = compute_ids(&[g_vid], &files);
    let sig_p = sign_sig(
        &ctx.key,
        DeviceOp::PublishPropose,
        op_p,
        gn(1, 1),
        prop_vid,
        prop_digest,
    );
    let (sp, _, _) = run(
        &ctx,
        post(
            "/v1/proposals",
            &sig_p,
            candidate_body(op_p, gn(1, 1), &[g_vid], &files),
        ),
    )
    .await;
    assert_eq!(sp, StatusCode::OK);

    // Approve it (review_required is off by default, so the proposer may approve) → (1,2).
    let op_a = "50000000-0000-4000-8000-000000000002";
    let sig_a = sign_sig(
        &ctx.key,
        DeviceOp::ReviewApprove,
        op_a,
        gn(1, 1),
        prop_vid,
        prop_digest,
    );
    let body = serde_json::to_vec(&serde_json::json!({
        "workspace_id": WS, "skill_id": SKILL, "op_id": op_a, "device_key_id": DKID,
        "expected": { "epoch": 1, "seq": 1 },
        "proposal": hex::encode(prop_vid), "decision": "approve",
    }))
    .unwrap();

    let (status, _, bytes) = run(&ctx, post("/v1/reviews", &sig_a, body)).await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "approve should be ok: {env:?}");
    assert_eq!(env.command, "review");
    let receipt = env.receipt.unwrap();
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current_generation, Some(gn(1, 2)));
    assert_eq!(
        receipt.version_id.as_deref(),
        Some(hex::encode(prop_vid).as_str())
    );
}
