//! `POST /v1/publish` — the direct pointer-move write: the canonical OK receipt, the edge op_id
//! guard, byte-identical idempotent replay, and the 200-CONFLICT stale-expected outcome.

use topos_types::SignedCurrentRecord;

use super::*;

// ── publish ─────────────────────────────────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn publish_happy_path_moves_current_and_returns_a_canonical_ok_receipt(pool: PgPool) {
    let ctx = setup(pool, "publish-ok").await;
    let (g_vid, _g_digest) = seed_genesis(&ctx, "00000000-0000-4000-8000-000000000000").await;

    let op = "00000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"genesis v0\nplus more\n")];
    let parents = [g_vid];
    let (vid, digest) = compute_ids(&parents, &files);
    let sig = sign_sig(&ctx.key, DeviceOp::PublishDirect, op, gn(1, 1), vid, digest);
    let body = candidate_body(op, gn(1, 1), &parents, &files);

    let (status, _h, bytes) = run(&ctx, post("/v1/publish", &sig, body)).await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "publish should be ok: {env:?}");
    assert_eq!(env.command, "publish");
    assert!(env.error.is_none());
    let receipt = env.receipt.expect("a receipt");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.workspace_id, WS);
    assert_eq!(receipt.skill_id.as_deref(), Some(SKILL));
    assert_eq!(
        receipt.version_id.as_deref(),
        Some(hex::encode(vid).as_str())
    );
    assert_eq!(receipt.current_generation, Some(gn(1, 2)));
    // On OK, `data` carries the signed current record (a future client advances its floor from it).
    let record: SignedCurrentRecord =
        serde_json::from_value(env.data).expect("OK data is the SignedCurrentRecord");
    assert_eq!(record.scope.workspace_id, WS);
    assert_eq!(record.scope.skill_id, SKILL);
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_write_with_a_non_uuid_op_id_is_rejected_before_ingest(pool: PgPool) {
    // A path-safe op_id that is NOT a canonical UUID: `OpId::parse` accepts it, but the authority binds op_id
    // as 16 bytes only AFTER it ingests + leases the candidate, so without an edge guard a malformed
    // unauthenticated request would pin the uploaded objects on the later parse failure. The edge must reject
    // it with a 400 BEFORE reaching the authority (so the candidate is never ingested).
    let ctx = setup(pool, "publish-bad-opid").await;
    let (g_vid, _) = seed_genesis(&ctx, "20000000-0000-4000-8000-000000000000").await;

    let op = "not-a-canonical-uuid"; // lowercase + hyphens → path-safe, but not a UUID
    let files = vec![file("SKILL.md", b"must never be ingested\n")];
    let parents = [g_vid];
    // A well-formed (86-char base64url) but unchecked signature: the op_id is rejected before the authority
    // ever verifies a signature, so a real one is unnecessary.
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0u8; 64]);
    let body = candidate_body(op, gn(1, 1), &parents, &files);

    let (status, _h, bytes) = run(&ctx, post("/v1/publish", &sig, body)).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "a non-UUID op_id must be a 400 at the edge, never a 200 that ingested the candidate"
    );
    let env = envelope(&bytes);
    assert!(!env.ok);
    assert!(
        env.error.is_some(),
        "a malformed op_id is a wire error: {env:?}"
    );
    assert!(
        env.receipt.is_none(),
        "no receipt — the authority was never reached"
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_idempotent_retry_replays_a_byte_identical_response(pool: PgPool) {
    let ctx = setup(pool, "publish-retry").await;
    let (g_vid, _) = seed_genesis(&ctx, "10000000-0000-4000-8000-000000000000").await;

    let op = "10000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"retry me\n")];
    let parents = [g_vid];
    let (vid, digest) = compute_ids(&parents, &files);
    let sig = sign_sig(&ctx.key, DeviceOp::PublishDirect, op, gn(1, 1), vid, digest);

    let (s1, _, b1) = run(
        &ctx,
        post(
            "/v1/publish",
            &sig,
            candidate_body(op, gn(1, 1), &parents, &files),
        ),
    )
    .await;
    let (s2, _, b2) = run(
        &ctx,
        post(
            "/v1/publish",
            &sig,
            candidate_body(op, gn(1, 1), &parents, &files),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s2, StatusCode::OK);
    // Same op_id + identity → the replay returns the byte-identical receipt/envelope.
    assert_eq!(
        b1, b2,
        "a lost-ack retry must replay the byte-identical response"
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_stale_publish_is_a_200_conflict_with_a_rebase_next_action(pool: PgPool) {
    let ctx = setup(pool, "publish-conflict").await;
    let (g_vid, _) = seed_genesis(&ctx, "20000000-0000-4000-8000-000000000000").await;

    // First child wins → (1,2).
    let op_a = "20000000-0000-4000-8000-000000000001";
    let files_a = vec![file("SKILL.md", b"winner\n")];
    let (vid_a, dig_a) = compute_ids(&[g_vid], &files_a);
    let sig_a = sign_sig(
        &ctx.key,
        DeviceOp::PublishDirect,
        op_a,
        gn(1, 1),
        vid_a,
        dig_a,
    );
    let (sa, _, ba) = run(
        &ctx,
        post(
            "/v1/publish",
            &sig_a,
            candidate_body(op_a, gn(1, 1), &[g_vid], &files_a),
        ),
    )
    .await;
    assert_eq!(sa, StatusCode::OK);
    assert_eq!(
        envelope(&ba).receipt.unwrap().current_generation,
        Some(gn(1, 2))
    );

    // Second child, still based on (1,1) → CONFLICT (current is now (1,2)).
    let op_b = "20000000-0000-4000-8000-000000000002";
    let files_b = vec![file("SKILL.md", b"loser\n")];
    let (vid_b, dig_b) = compute_ids(&[g_vid], &files_b);
    let sig_b = sign_sig(
        &ctx.key,
        DeviceOp::PublishDirect,
        op_b,
        gn(1, 1),
        vid_b,
        dig_b,
    );
    let (sb, _, bb) = run(
        &ctx,
        post(
            "/v1/publish",
            &sig_b,
            candidate_body(op_b, gn(1, 1), &[g_vid], &files_b),
        ),
    )
    .await;

    // Design rule: a CONFLICT is a 200 carrying the receipt + WireError, not a non-2xx.
    assert_eq!(sb, StatusCode::OK);
    let env = envelope(&bb);
    assert!(!env.ok);
    let receipt = env.receipt.unwrap();
    assert_eq!(receipt.outcome, TerminalOutcome::Conflict);
    assert_eq!(receipt.current_generation, Some(gn(1, 2)));
    let error = env.error.expect("a CONFLICT carries a WireError");
    assert_eq!(error.outcome, TerminalOutcome::Conflict);
    assert!(
        error
            .next_actions
            .iter()
            .any(|a| a.code == topos_types::ActionCode::RebaseAndRetry),
        "a CONFLICT must offer REBASE_AND_RETRY"
    );
}
