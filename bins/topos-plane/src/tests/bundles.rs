//! The skill-scoped object reads (`.../bundles/{object_id}` + the sibling versions route):
//! 404-not-403, never by bare hash.

use super::*;

// ── reads: 404-not-403 ──────────────────────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_bundle_read_with_no_or_bad_credential_is_404_never_403(pool: PgPool) {
    let ctx = setup(pool, "read-404").await;
    let obj = "0".repeat(64);
    let uri = format!("/v1/workspaces/{WS}/skills/{SKILL}/bundles/{obj}");

    // No Authorization header.
    let (s_none, _, _) = run(&ctx, get(&uri, &[])).await;
    assert_eq!(s_none, StatusCode::NOT_FOUND);

    // A bogus bearer token.
    let (s_bad, _, _) = run(
        &ctx,
        get(&uri, &[("authorization", "Bearer not-a-real-token")]),
    )
    .await;
    assert_eq!(s_bad, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_scope_path_mismatch_read_is_404_never_403(pool: PgPool) {
    let ctx = setup(pool, "read-mismatch").await;
    // A valid token scoped to (WS, SKILL); ask for a DIFFERENT skill in the path → the indistinguishable 404.
    let obj = "0".repeat(64);
    let uri = format!("/v1/workspaces/{WS}/skills/s_other/bundles/{obj}");
    let (status, _, _) = run(
        &ctx,
        get(&uri, &[("authorization", &format!("Bearer {READ_TOKEN}"))]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The version-metadata route too (a bad/absent token).
    let vid = "1".repeat(64);
    let v_uri = format!("/v1/workspaces/{WS}/skills/{SKILL}/versions/{vid}");
    let (s_ver, _, _) = run(&ctx, get(&v_uri, &[])).await;
    assert_eq!(s_ver, StatusCode::NOT_FOUND);
}
