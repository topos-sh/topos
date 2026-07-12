//! The membership-scoped object reads (`.../bundles/{object_id}` + the sibling versions route):
//! 404-not-403, never by bare hash.

use super::*;

// ── reads: 404-not-403 ──────────────────────────────────────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_bundle_read_with_no_or_bad_credential_is_404_never_403(pool: PgPool) {
    let ctx = setup(pool, "read-404").await;
    let obj = "0".repeat(64);
    let uri = format!("/v1/workspaces/{WS}/skills/{SKILL}/bundles/{obj}");

    // No Authorization header (the uniform missing-credential miss).
    let (s_none, _, _) = run(&ctx, get(&uri, &[])).await;
    assert_eq!(s_none, StatusCode::NOT_FOUND);

    // A bogus bearer credential (unknown → the same indistinguishable 404).
    let (s_bad, _, _) = run(
        &ctx,
        get(&uri, &[("authorization", "Bearer not-a-real-credential")]),
    )
    .await;
    assert_eq!(s_bad, StatusCode::NOT_FOUND);
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_unreachable_object_read_is_404_never_403(pool: PgPool) {
    let ctx = setup(pool, "read-mismatch").await;
    // A valid member credential (the member reads any skill; the skill comes from the path), but an object
    // that is not reachable under the requested skill → the indistinguishable 404, never a 403.
    let obj = "0".repeat(64);
    let uri = format!("/v1/workspaces/{WS}/skills/s_other/bundles/{obj}");
    let (status, _, _) = run(
        &ctx,
        get(&uri, &[("authorization", &format!("Bearer {CREDENTIAL}"))]),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The version-metadata route too (a bad/absent credential is the same uniform miss).
    let vid = "1".repeat(64);
    let v_uri = format!("/v1/workspaces/{WS}/skills/{SKILL}/versions/{vid}");
    let (s_ver, _, _) = run(&ctx, get(&v_uri, &[])).await;
    assert_eq!(s_ver, StatusCode::NOT_FOUND);
}
