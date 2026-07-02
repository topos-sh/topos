//! `POST /v1/reverts` — the forward revert commit.

use super::*;

// ── revert ────────────────────────────────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn revert_is_a_forward_commit_that_advances_seq(pool: PgPool) {
    let ctx = setup(pool, "revert-ok").await;
    let (g_vid, g_digest) = seed_genesis(&ctx, "40000000-0000-4000-8000-000000000000").await;

    // Publish a child → (1,2); current commit = child.
    let op_c = "40000000-0000-4000-8000-000000000001";
    let files_c = vec![file("SKILL.md", b"a regrettable change\n")];
    let (child_vid, dig_c) = compute_ids(&[g_vid], &files_c);
    let sig_c = sign_sig(
        &ctx.key,
        DeviceOp::PublishDirect,
        op_c,
        gn(1, 1),
        child_vid,
        dig_c,
    );
    let (sc, _, _) = run(
        &ctx,
        post(
            "/v1/publish",
            &sig_c,
            candidate_body(op_c, gn(1, 1), &[g_vid], &files_c),
        ),
    )
    .await;
    assert_eq!(sc, StatusCode::OK);

    // Revert --to genesis: a forward commit {tree: genesis tree, parents: [child]} → (1,3).
    let op_r = "40000000-0000-4000-8000-000000000002";
    let forward_vid = sign::commit_id(&Commit {
        parents: &[child_vid],
        tree: g_digest,
        author: AUTHOR,
        message: MESSAGE,
    })
    .unwrap();
    let sig_r = sign_sig(
        &ctx.key,
        DeviceOp::Revert,
        op_r,
        gn(1, 2),
        forward_vid,
        g_digest,
    );
    let body = serde_json::to_vec(&serde_json::json!({
        "workspace_id": WS, "skill_id": SKILL, "op_id": op_r, "device_key_id": DKID,
        "expected": { "epoch": 1, "seq": 2 },
        "good": hex::encode(g_vid), "author": AUTHOR, "message": MESSAGE,
    }))
    .unwrap();

    let (status, _, bytes) = run(&ctx, post("/v1/reverts", &sig_r, body)).await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "revert should be ok: {env:?}");
    assert_eq!(env.command, "revert");
    let receipt = env.receipt.unwrap();
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current_generation, Some(gn(1, 3)));
    assert_eq!(
        receipt.version_id.as_deref(),
        Some(hex::encode(forward_vid).as_str())
    );
}
