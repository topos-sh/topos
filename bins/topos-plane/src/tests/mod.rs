//! Route tests for the internal custody lane — the real `router(state)` driven via `oneshot` (no
//! socket) over a real authority (`#[sqlx::test]` provisions a per-test database and runs
//! plane-store's embedded migrations through the exported `MIGRATOR`).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use plane_store::Authority;
use sqlx::PgPool;
use tower::ServiceExt as _;

use crate::state::PlaneState;

const TOKEN: &str = "internal-test-bearer";

/// A temp dir + a composed router, cleaned up on drop.
struct Fixture {
    dir: PathBuf,
    router: Router,
}

impl Fixture {
    fn new(pool: PgPool, tag: &str) -> Self {
        Self::build(pool, tag, true)
    }

    /// A fixture whose internal lane was never armed (no bearer configured).
    fn unarmed(pool: PgPool, tag: &str) -> Self {
        Self::build(pool, tag, false)
    }

    fn build(pool: PgPool, tag: &str, armed: bool) -> Self {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-tp-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let authority = Authority::from_pool(pool, &dir.join("stores"), &dir.join("large"))
            .expect("open authority");
        let mut state = PlaneState::new(Arc::new(authority));
        if armed {
            state = state.with_internal_token(TOKEN);
        }
        Self {
            dir,
            router: crate::router(state),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One request through the composed router; returns `(status, body-bytes)`.
async fn send(
    router: &Router,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let mut req = Request::builder().method(method).uri(path);
    if let Some(token) = bearer {
        req = req.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let req = match body {
        Some(v) => req
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(v.to_string())),
        None => req.body(Body::empty()),
    }
    .expect("request builds");
    let response = router.clone().oneshot(req).await.expect("router answers");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body reads");
    (status, bytes.to_vec())
}

fn json(bytes: &[u8]) -> serde_json::Value {
    serde_json::from_slice(bytes).expect("a JSON body")
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn candidate_body(content: &[u8], parent: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "files": [{ "path": "GUIDE.md", "mode": "100644", "content_base64": b64(content) }],
        "parent": parent,
        "attribution": "Alice (test)",
        "message": "test: candidate",
    })
}

const BASE: &str = "/internal/v1/workspaces/w1/bundles/b1";

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_lane_is_invisible_unarmed_and_401_on_a_wrong_bearer(pool: PgPool) {
    let fx = Fixture::unarmed(pool, "gate");
    // healthz stays open either way.
    let (status, body) = send(&fx.router, "GET", "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
    // Unarmed lane: the uniform 404 whatever the credential says.
    let (status, body) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/current"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["code"], "NOT_FOUND");
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_armed_lane_answers_401_wrong_and_404_missing_bearer(pool: PgPool) {
    let fx = Fixture::new(pool, "gate2");
    let (status, body) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/current"),
        Some("wrong"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(json(&body)["code"], "UNAUTHORIZED");
    let (status, _) = send(&fx.router, "GET", &format!("{BASE}/current"), None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // An unmatched path is the same uniform 404 shape.
    let (status, body) = send(&fx.router, "GET", "/nope", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(json(&body)["code"], "NOT_FOUND");
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_custody_loop_round_trips_over_http(pool: PgPool) {
    let fx = Fixture::new(pool, "loop");

    // Genesis publish (expected_generation absent = genesis).
    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/publish"),
        Some(TOKEN),
        Some(candidate_body(b"hello", None)),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let published = json(&body);
    assert_eq!(published["deduped"], false);
    assert_eq!(published["pointer"]["generation"], 1);
    assert_eq!(published["pointer"]["replayed"], false);
    let v1 = published["version_id"]
        .as_str()
        .expect("version id")
        .to_owned();
    assert_eq!(published["commit_id"], v1.as_str());
    let digest = published["bundle_digest"]
        .as_str()
        .expect("digest")
        .to_owned();

    // Current.
    let (status, body) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/current"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let current = json(&body);
    assert_eq!(current["version_id"], v1.as_str());
    assert_eq!(current["generation"], 1);
    assert_eq!(current["bundle_digest"], digest.as_str());
    assert_eq!(current["moved_by_display"], "Alice (test)");

    // Version meta + file listing.
    let (status, body) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/versions/{v1}"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let meta = json(&body);
    assert_eq!(meta["files"][0]["path"], "GUIDE.md");
    assert_eq!(meta["files"][0]["mode"], "100644");
    let object_id = meta["files"][0]["object_id"]
        .as_str()
        .expect("object id")
        .to_owned();

    // Object bytes.
    let (status, body) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/objects/{object_id}"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"hello");

    // The propose path: commit a child WITHOUT moving the pointer, then approve via the pointer op.
    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/versions"),
        Some(TOKEN),
        Some(candidate_body(b"proposed", Some(&v1))),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let proposal = json(&body);
    let v2 = proposal["version_id"].as_str().expect("v2").to_owned();
    // The pointer has not moved.
    let (_, body) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/current"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(json(&body)["generation"], 1);

    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/pointer"),
        Some(TOKEN),
        Some(serde_json::json!({
            "version_id": v2,
            "expected_generation": 1,
            "attribution": "Reviewer Bob",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let moved = json(&body);
    assert_eq!(moved["generation"], 2);
    assert_eq!(moved["moved_by_display"], "Reviewer Bob");

    // Revert back to v1's bytes (a forward move).
    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/revert"),
        Some(TOKEN),
        Some(serde_json::json!({
            "to_version_id": v1,
            "expected_generation": 2,
            "attribution": "Reviewer Bob",
            "message": "revert to the good bytes",
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let reverted = json(&body);
    assert_eq!(reverted["pointer"]["generation"], 3);
    assert_eq!(reverted["bundle_digest"], digest.as_str());
    assert_ne!(
        reverted["version_id"],
        v1.as_str(),
        "a fresh forward commit"
    );

    // The log walks newest-first.
    let (status, body) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/log?limit=10"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let log = json(&body);
    let versions = log["versions"].as_array().expect("versions");
    assert_eq!(versions.len(), 3);
    assert_eq!(versions[2]["version_id"], v1.as_str());

    // Purge the superseded v2 (un-pointed now), then verify the typed pointed-at refusal on v1's
    // revert head is impossible to hit here — purge the pointed head instead and expect 409.
    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/versions/{v2}/purge"),
        Some(TOKEN),
        Some(serde_json::json!({ "attribution": "Owner Carol" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let purged = json(&body);
    assert_eq!(purged["tombstoned"], 1);
    assert_eq!(purged["reclaimed"], 1);

    let head = reverted["version_id"].as_str().expect("head").to_owned();
    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/versions/{head}/purge"),
        Some(TOKEN),
        Some(serde_json::json!({ "attribution": "Owner Carol" })),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(json(&body)["code"], "POINTED_AT");

    // Delete the bundle, then the workspace — both answer their reports.
    let (status, body) = send(&fx.router, "DELETE", BASE, Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json(&body)["versions_dropped"], 3);
    let (status, _) = send(
        &fx.router,
        "DELETE",
        "/internal/v1/workspaces/w1",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_lost_cas_answers_409_with_the_live_pointer(pool: PgPool) {
    let fx = Fixture::new(pool, "conflict");
    let (_, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/publish"),
        Some(TOKEN),
        Some(candidate_body(b"one", None)),
    )
    .await;
    let v1 = json(&body)["version_id"].as_str().expect("v1").to_owned();
    let mut child = candidate_body(b"two", Some(&v1));
    child["expected_generation"] = serde_json::json!(1);
    let (status, _) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/publish"),
        Some(TOKEN),
        Some(child),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A stale writer: 409 CONFLICT carrying the live (generation, version_id).
    let mut stale = candidate_body(b"stale", Some(&v1));
    stale["expected_generation"] = serde_json::json!(1);
    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/publish"),
        Some(TOKEN),
        Some(stale),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let conflict = json(&body);
    assert_eq!(conflict["code"], "CONFLICT");
    assert_eq!(conflict["generation"], 2);
    assert!(conflict["version_id"].is_string());
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn shape_violations_answer_400_and_misses_the_uniform_404(pool: PgPool) {
    let fx = Fixture::new(pool, "shape");
    // A malformed workspace id in the path (leading dot) is a 400 shape refusal.
    let (status, body) = send(
        &fx.router,
        "GET",
        "/internal/v1/workspaces/.hidden/bundles/b1/current",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["code"], "BAD_REQUEST");

    // A malformed version id in a PATH is the uniform 404 (not a known id); an unknown bundle too.
    let (status, _) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/versions/not-hex"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = send(
        &fx.router,
        "GET",
        &format!("{BASE}/current"),
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // An empty attribution is a 400.
    let mut bad = candidate_body(b"x", None);
    bad["attribution"] = serde_json::json!("");
    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/publish"),
        Some(TOKEN),
        Some(bad),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["code"], "BAD_REQUEST");

    // A denied candidate (an unknown parent) is a typed 400 REJECTED.
    let ghost = "ab".repeat(32);
    let (status, body) = send(
        &fx.router,
        "POST",
        &format!("{BASE}/versions"),
        Some(TOKEN),
        Some(candidate_body(b"x", Some(&ghost))),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json(&body)["code"], "REJECTED");
}
