//! Cross-route tests: the `PlaneState::open` construction path, the maintenance pass, and the
//! uniform wire-error envelope.

use super::*;

/// Create a uniquely-named empty database on the `$DATABASE_URL` server and return a connection URL to it
/// — for the one test that exercises the production `PlaneState::open(database_url)` path (which connects +
/// migrates itself). The route tests instead take an already-migrated pool from `#[sqlx::test(migrator = "plane_store::MIGRATOR")]`.
async fn unique_database_url(tag: &str) -> String {
    use sqlx::{Connection, Executor};
    static N: AtomicU32 = AtomicU32::new(0);
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must point at a Postgres");
    let name = format!(
        "topos_plane_{tag}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    );
    let mut admin = sqlx::PgConnection::connect(&base)
        .await
        .expect("connect to the base Postgres database");
    admin
        .execute(format!(r#"CREATE DATABASE "{name}""#).as_str())
        .await
        .expect("create the per-test database");
    admin.close().await.ok();
    let (prefix, _db) = base
        .rsplit_once('/')
        .expect("DATABASE_URL ends in /<database>");
    format!("{prefix}/{name}")
}

/// The runtime parity guard for the **single construction path**: `PlaneState::open` (the leak-free
/// constructor the bin + a downstream plane use) runs against a real `database_url` (a freshly-provisioned
/// Postgres database, its git/large stores in a tempdir) and yields a SERVING state. It is the only test
/// that EXECUTES the production constructor (the bin isn't run in CI; the `open` doc-test is `no_run`), so
/// it pins that the internal resolution matches the bin's (mode `"cloud"` ⇒ `Cloud`; no SMTP ⇒
/// `device_code`) and that the composed `router(state)` answers — an unknown read token is the
/// indistinguishable 404, never a panic/500. It provisions its own database (so it can pass a URL, not a
/// pool), so it is a plain `#[tokio::test]`, not `#[sqlx::test(migrator = "plane_store::MIGRATOR")]`.
#[tokio::test]
async fn open_builds_a_serving_state() {
    let dir = unique_dir("open");
    let state = PlaneState::open(crate::PlaneConfig {
        database_url: unique_database_url("open").await,
        git_root: dir.join("git"),
        large_root: dir.join("large"),
        plane_key_path: dir.join("plane.key"),
        enroll_secret_path: dir.join("enroll.key"),
        base_url: "https://plane.test".to_owned(),
        mode: "cloud".to_owned(),
        enrollment_method: None,
        smtp: None,
    })
    .await
    .expect("open builds a serving state");

    // The constructor's internal resolution matches the bin's: the mode `String` parsed to `Cloud`, and the
    // enrollment method defaulted to `device_code` (no SMTP relay).
    assert_eq!(state.enroll().base_url, "https://plane.test");
    assert_eq!(state.enroll().deployment_mode, DeploymentMode::Cloud);
    assert_eq!(state.enroll().enrollment_method, "device_code");

    // The composed router serves: an unknown token is the indistinguishable 404, proving the authority +
    // routes are wired by the constructor.
    let (status, _h, _b) = send(router(state), get("/v1/current/rt_unknown_token", &[])).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dir);
}

// ── maintenance: one scheduled tick body drives the authority's reclamation ops ─────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_maintenance_pass_reclaims_a_rejected_proposals_unique_bytes(pool: PgPool) {
    // The tick BODY (`run_maintenance_pass`) is tested directly against a real authority — the scheduler's
    // interval is tokio's to test, not ours. Make real garbage over the wire: open a proposal with unique
    // bytes, then reject it — its `proposal_object` root stops matching and the unique objects become
    // unrooted; a pass must enumerate the workspace and reclaim them, logging no fault.
    let ctx = setup(pool, "maintenance").await;
    let (g_vid, _) = seed_genesis(&ctx, "90000000-0000-4000-8000-000000000000").await;

    let op_p = "90000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"a change nobody wanted\n")];
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

    let op_r = "90000000-0000-4000-8000-000000000002";
    let sig_r = sign_sig(
        &ctx.key,
        DeviceOp::ReviewReject,
        op_r,
        gn(1, 1),
        prop_vid,
        prop_digest,
    );
    let body = serde_json::to_vec(&serde_json::json!({
        "workspace_id": WS, "skill_id": SKILL, "op_id": op_r, "device_key_id": DKID,
        "expected": { "epoch": 1, "seq": 1 },
        "proposal": hex::encode(prop_vid), "decision": "reject",
    }))
    .unwrap();
    let (sr, _, _) = run(&ctx, post("/v1/reviews", &sig_r, body)).await;
    assert_eq!(sr, StatusCode::OK);

    // One pass — the same body the spawned scheduler runs each tick (and once at startup).
    let pass = crate::maintenance::run_maintenance_pass(&ctx.state).await;
    assert_eq!(pass.faults, 0, "a healthy store logs no faults: {pass:?}");
    assert!(
        pass.objects_reclaimed >= 1,
        "the rejected proposal's unique bytes are unrooted and must be reclaimed: {pass:?}"
    );

    // A second pass converges to nothing-to-do (the reclaim is not repeated; genesis stays rooted).
    let second = crate::maintenance::run_maintenance_pass(&ctx.state).await;
    assert_eq!(second, crate::maintenance::MaintenancePass::default());
}

// ── transport: a malformed body is an envelope-shaped 400 ─────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_malformed_body_is_a_400_envelope_not_axums_plain_text(pool: PgPool) {
    let ctx = setup(pool, "bad-body").await;
    // A valid signature header but a non-JSON body.
    let sig = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([0u8; 64]);
    let (status, _, bytes) = run(&ctx, post("/v1/publish", &sig, b"not json".to_vec())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let env = envelope(&bytes);
    assert!(!env.ok);
    assert!(
        env.error.is_some(),
        "a 400 body is the uniform error envelope"
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_missing_device_signature_header_is_a_400(pool: PgPool) {
    let ctx = setup(pool, "no-sig").await;
    let (g_vid, _) = seed_genesis(&ctx, "70000000-0000-4000-8000-000000000000").await;
    let op = "70000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"unsigned\n")];
    // POST with NO Topos-Device-Signature header.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/publish")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(candidate_body(op, gn(1, 1), &[g_vid], &files)))
        .unwrap();
    let (status, _, _) = run(&ctx, req).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
