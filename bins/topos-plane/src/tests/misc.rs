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
/// `device_code`) and that the composed `router(state)` answers — a read with an unknown workspace
/// credential is the indistinguishable 404, never a panic/500. It provisions its own database (so it can
/// pass a URL, not a pool), so it is a plain `#[tokio::test]`, not `#[sqlx::test(migrator = "plane_store::MIGRATOR")]`.
#[tokio::test]
async fn open_builds_a_serving_state() {
    let dir = unique_dir("open");
    let state = PlaneState::open(crate::PlaneConfig {
        database_url: unique_database_url("open").await,
        git_root: dir.join("git"),
        large_root: dir.join("large"),
        enroll_secret_path: dir.join("enroll.key"),
        base_url: "https://plane.test".to_owned(),
        verify_base_url: None,
        link_base_url: None,
        mode: "cloud".to_owned(),
        enrollment_method: None,
    })
    .await
    .expect("open builds a serving state");

    // The constructor's internal resolution matches the bin's: the mode `String` parsed to `Cloud`, and the
    // enrollment method defaulted to `device_code` (no SMTP relay).
    assert_eq!(state.enroll().base_url, "https://plane.test");
    // The link base defaults to the base URL (the construction record mirrors the authority's copy).
    assert_eq!(state.enroll().link_base_url, "https://plane.test");
    assert_eq!(state.enroll().deployment_mode, DeploymentMode::Cloud);
    assert_eq!(state.enroll().enrollment_method, "device_code");

    // The composed router serves: a read presenting an unknown workspace credential reaches the authority
    // and resolves to the indistinguishable 404 (not a route miss), proving the authority + routes are wired
    // by the constructor.
    let (status, _h, _b) = send(
        router(state),
        get(
            "/v1/workspaces/w_unknown/skills/s_unknown/current",
            &[("authorization", "Bearer cred_unknown")],
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let _ = std::fs::remove_dir_all(&dir);
}

/// The reserved claim-only species marker is refused at CONSTRUCTION: a plane configured to advertise
/// `enrollment_method = "admin_claim"` would make clients treat live invites as one-shot claims (the wrong
/// door), so `PlaneState::open` fails typed before anything serves.
#[tokio::test]
async fn open_refuses_the_reserved_admin_claim_enrollment_method() {
    let dir = unique_dir("open-reserved");
    let err = PlaneState::open(crate::PlaneConfig {
        database_url: unique_database_url("open_reserved").await,
        git_root: dir.join("git"),
        large_root: dir.join("large"),
        enroll_secret_path: dir.join("enroll.key"),
        base_url: "https://plane.test".to_owned(),
        verify_base_url: None,
        link_base_url: None,
        mode: "self_host".to_owned(),
        enrollment_method: Some("admin_claim".to_owned()),
    })
    .await
    .expect_err("the reserved method must refuse the construction");
    assert!(err.to_string().contains("reserved"), "got {err}");
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
    let (prop_vid, _prop_digest) = compute_ids(&[g_vid], &files);
    let (sp, _, _) = run(
        &ctx,
        post(
            "/v1/proposals",
            candidate_body(op_p, gn(1, 1), &[g_vid], &files),
        ),
    )
    .await;
    assert_eq!(sp, StatusCode::OK);

    let op_r = "90000000-0000-4000-8000-000000000002";
    // No credential in the body — the workspace credential rides the `post` helper's Authorization header.
    // The reject reason is MANDATORY on the device lane now (an empty reason would be a synthesized denial).
    let body = serde_json::to_vec(&serde_json::json!({
        "workspace_id": WS, "skill_id": SKILL, "op_id": op_r,
        "expected": { "epoch": 1, "seq": 1 },
        "proposal": hex::encode(prop_vid), "decision": "reject", "reason": "not this change",
    }))
    .unwrap();
    let (sr, _, rbytes) = run(&ctx, post("/v1/reviews", body)).await;
    assert_eq!(sr, StatusCode::OK);
    assert!(envelope(&rbytes).ok, "the reject with a reason lands");

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

// ── the mint-claim subcommand's print path: one link line, the token never in tracing ────────────────

/// The bin's `mint-claim` subcommand prints EXACTLY the string [`PlaneState::mint_admin_claim`] returns
/// (one `println!` — the only stdout write on that path), so the wrapper's return IS the print path:
/// assert it is a single `<base_url>/i/<token>` line (newline-free, base64url token) and that the bearer
/// token never enters tracing (a TRACE-level subscriber captures everything emitted during the mint).
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn mint_claim_emits_one_link_line_and_never_traces_the_token(pool: PgPool) {
    let ctx = enroll_setup(pool, "mint-claim-smoke").await;

    // A thread-local TRACE-capturing subscriber for the duration of the mint (the `#[sqlx::test]` runtime
    // is current-thread, so the whole op — including the authority's SQL — runs under it).
    #[derive(Clone, Default)]
    struct Buf(Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("buf lock").extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Buf {
        type Writer = Buf;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }
    let buf = Buf::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::level_filters::LevelFilter::TRACE)
        .with_writer(buf.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    let link = ctx
        .state
        .mint_admin_claim("w_newco", Some("Newco"), Some("owner@newco.com"), 3600)
        .await
        .expect("mint the claim link");
    drop(guard);

    // Exactly one printable line, shaped `<base_url>/i/<token>`.
    assert!(
        !link.contains('\n') && !link.contains('\r'),
        "the link is a single stdout line"
    );
    let token = link
        .strip_prefix(&format!("{ENROLL_BASE_URL}/i/"))
        .expect("the link is <base_url>/i/<token>");
    assert_eq!(token.len(), 43, "a 32-byte base64url-unpadded token");
    assert!(
        token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "the token is base64url (path-safe): {token}"
    );

    // The bearer token appears NOWHERE in the captured tracing output.
    let traced = String::from_utf8_lossy(&buf.0.lock().expect("buf lock")).into_owned();
    assert!(
        !traced.contains(token),
        "the claim token must never enter tracing"
    );

    // On a cloud-mode plane the mint REFUSES without an owner email — the typed operator refusal.
    let refused = ctx
        .state
        .mint_admin_claim("w_other", None, None, 3600)
        .await;
    assert!(
        refused.is_err(),
        "a cloud-mode mint without an owner email is refused: {refused:?}"
    );
}

// ── the unconfigured `new` path fails closed on the genesis wrappers ─────────────────────────────────

/// A [`PlaneState::new`] composition that never set an enroll config has NO configured deployment mode:
/// every strict-mode genesis/standup wrapper must refuse typed (fail closed) instead of silently assuming
/// self_host against an `Authority` that may be configured cloud. (`PlaneState::open` always sets the
/// strict mode explicitly from the parsed config, so the bin and a composing plane are unaffected.)
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_unconfigured_new_state_refuses_the_genesis_wrappers(pool: PgPool) {
    let ctx = setup(pool, "unconfigured-mode").await; // PlaneState::new + the DEFAULT enroll config
    let err = ctx
        .state
        .mint_admin_claim("w_newco", Some("Newco"), Some("owner@newco.com"), 3600)
        .await
        .expect_err("mint_admin_claim must fail closed with no configured mode");
    assert!(err.to_string().contains("not configured"), "got {err}");
    let err = ctx
        .state
        .create_workspace("req-unconfigured", None, None, "owner@newco.com")
        .await
        .expect_err("create_workspace must fail closed with no configured mode");
    assert!(err.to_string().contains("not configured"), "got {err}");
    let err = ctx
        .state
        .approve_standup("ABCD-EFGH-IJKL-MNOP", "owner@newco.com", None, None)
        .await
        .expect_err("approve_standup must fail closed with no configured mode");
    assert!(err.to_string().contains("not configured"), "got {err}");
}

// ── transport: a malformed body is an envelope-shaped 400 ─────────────────────────────────────────────

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_malformed_body_is_a_400_envelope_not_axums_plain_text(pool: PgPool) {
    let ctx = setup(pool, "bad-body").await;
    // A non-JSON body — the credential rides the `Authorization` header (the `post` helper attaches it), so
    // this is a pure body-parse fault: the 400 fires in the body extractor before the handler runs.
    let (status, _, bytes) = run(&ctx, post("/v1/publish", b"not json".to_vec())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let env = envelope(&bytes);
    assert!(!env.ok);
    assert!(
        env.error.is_some(),
        "a 400 body is the uniform error envelope"
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_publish_with_an_unknown_credential_is_a_200_denied(pool: PgPool) {
    // The write credential is the `Authorization: Bearer` workspace credential, authenticated in-transaction
    // by registry-row lookup (a device is resolved by its stored credential sha256). A publish presenting a
    // credential that resolves to NO registered device is a SYNTHESIZED pre-auth DENIED — a 200 carrying the
    // DENIED receipt/error (never persisted, never a 401/403). A MISSING/blank header, by contrast, is the
    // uniform 404 (see the `bearer_token` miss); an UNKNOWN one reaches the authority and is DENIED.
    let ctx = setup(pool, "unknown-credential").await;
    let (g_vid, _) = seed_genesis(&ctx, "70000000-0000-4000-8000-000000000000").await;
    let op = "70000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"from a stranger\n")];
    let body = candidate_body(op, gn(1, 1), &[g_vid], &files);
    let (status, _, bytes) = run(
        &ctx,
        post_as("/v1/publish", body, Some("cred_unregistered")),
    )
    .await;
    // A protocol outcome is always a 200; an unknown credential is DENIED.
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(!env.ok, "an unknown credential must be denied: {env:?}");
    assert_eq!(
        env.error.expect("DENIED carries a WireError").outcome,
        TerminalOutcome::Denied
    );
}
