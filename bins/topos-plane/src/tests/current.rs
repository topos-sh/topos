//! `GET /v1/current/{read_token}` — the current record + the commit-sensitive 304.

use topos_types::WireCurrentRecord;

use super::*;

// ── current: 200 + the commit-sensitive 304 ──────────────────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn current_serves_the_record_and_a_commit_sensitive_304(pool: PgPool) {
    let ctx = setup(pool, "current").await;
    let (g_vid, _) = seed_genesis(&ctx, "60000000-0000-4000-8000-000000000000").await;
    let uri = format!("/v1/current/{READ_TOKEN}");
    let known_version = hex::encode(g_vid);

    // 200 with the current record + an ETag of "<epoch>.<seq>".
    let (status, headers, bytes) = run(&ctx, get(&uri, &[])).await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(etag, "\"1.1\"");
    let record: WireCurrentRecord =
        serde_json::from_slice(&bytes).expect("the body is the current record");
    assert_eq!(record.scope.workspace_id, WS);

    // 304: matching ETag AND matching known version.
    let (s304, h304, b304) = run(
        &ctx,
        get(
            &uri,
            &[
                ("if-none-match", &etag),
                ("topos-known-version-id", &known_version),
            ],
        ),
    )
    .await;
    assert_eq!(s304, StatusCode::NOT_MODIFIED);
    assert!(b304.is_empty(), "a 304 has an empty body");
    assert_eq!(h304.get(header::ETAG).unwrap().to_str().unwrap(), etag);

    // 200: matching ETag but a DIFFERENT known version → commit-sensitive, the record is re-served.
    let (s_mismatch, _, _) = run(
        &ctx,
        get(
            &uri,
            &[
                ("if-none-match", &etag),
                ("topos-known-version-id", &"f".repeat(64)),
            ],
        ),
    )
    .await;
    assert_eq!(
        s_mismatch,
        StatusCode::OK,
        "a matching ETag with a different known version must NOT 304"
    );
}
