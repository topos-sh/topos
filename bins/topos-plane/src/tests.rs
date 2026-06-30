//! Per-route integration tests — `tower::ServiceExt::oneshot` against `router(state)`, no socket.
//!
//! Each test seeds a real [`Authority`] through the feature-gated test-fixtures shims (a registered device,
//! a rostered principal, a minted read token, and — where needed — a signed genesis), then drives the wire
//! exactly as a client would: a `Topos-Device-Signature` header over the SERVER-rehashed candidate ids, a
//! JSON body, the conditional-GET headers. They assert the status, the canonical receipt/envelope shape, and
//! the commit-sensitive 304.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use ed25519_dalek::{Signer as _, SigningKey};
use tower::ServiceExt as _;

use plane_store::{Authority, FileMode, OpId, Principal, SkillId, UploadedFile, WorkspaceId};
use topos_core::digest::{self, ManifestEntry};
use topos_core::sign::{self, Commit, DeviceOp, DeviceOpFields, device_op_preimage};
use topos_types::{Generation, JsonEnvelope, SignedCurrentRecord, TerminalOutcome};

use crate::{PlaneState, router};

// ── constants ──────────────────────────────────────────────────────────────────────────────────────
const WS: &str = "w_acme";
const SKILL: &str = "s_deploy";
const DKID: &str = "dk_a";
const PRINCIPAL: &str = "p_dev";
const READ_TOKEN: &str = "rt_test_secret_value";
const AUTHOR: &str = "d_test";
const MESSAGE: &str = "topos publish";
const CREATED_AT: &str = "2026-06-29T00:00:00Z";
const NOW: i64 = 1_000_000;
const KEY_SEED: u8 = 7;

// ── fixture ────────────────────────────────────────────────────────────────────────────────────────

/// A seeded plane (temp dirs cleaned on drop): a registered+rostered device, a minted read token.
struct Ctx {
    dir: PathBuf,
    state: PlaneState,
    key: SigningKey,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Ctx {
    fn authority(&self) -> &Authority {
        self.state.authority()
    }

    /// A fresh router over the shared state (oneshot consumes the router; the `Arc`-backed state is shared).
    fn app(&self) -> axum::Router {
        router(self.state.clone())
    }
}

fn unique_dir(tag: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("topos-plane-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn dev_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

async fn setup(tag: &str) -> Ctx {
    let dir = unique_dir(tag);
    let authority = Authority::open_sqlite(&dir.join("db"), &dir.join("git"), &dir.join("large"))
        .await
        .expect("open authority")
        .with_plane_key(&dir.join("plane.key"))
        .expect("plane key");
    let ws = WorkspaceId::parse(WS).unwrap();
    let skill = SkillId::parse(SKILL).unwrap();
    let principal = Principal::parse(PRINCIPAL).unwrap();
    let key = dev_key(KEY_SEED);
    authority
        .seed_device(
            &ws,
            DKID,
            &key.verifying_key().to_bytes(),
            &principal,
            false,
        )
        .await
        .unwrap();
    authority
        .seed_roster(&ws, &skill, &principal)
        .await
        .unwrap();
    authority
        .mint_read_token(&ws, &skill, &principal, READ_TOKEN)
        .await
        .unwrap();
    // Disable the rate limiter so a handful of test requests never trips it.
    let state = PlaneState::new(Arc::new(authority)).with_rate_limit(crate::Limits {
        burst: 1.0,
        refill_per_sec: 1.0,
        enabled: false,
    });
    Ctx { dir, state, key }
}

/// Seed a signed genesis at (1,1); returns (genesis version_id, genesis bundle_digest).
async fn seed_genesis(ctx: &Ctx, op_id: &str) -> ([u8; 32], [u8; 32]) {
    let receipt = ctx
        .authority()
        .seed_published_genesis(
            &WorkspaceId::parse(WS).unwrap(),
            &SkillId::parse(SKILL).unwrap(),
            DKID,
            &[KEY_SEED; 32],
            &OpId::parse(op_id).unwrap(),
            vec![file("SKILL.md", b"genesis v0\n")],
            AUTHOR,
            MESSAGE,
            CREATED_AT,
            NOW,
        )
        .await
        .expect("seed genesis");
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current, Some(gn(1, 1)));
    (
        receipt.version_id.unwrap().0,
        receipt.bundle_digest.unwrap(),
    )
}

// ── small builders ─────────────────────────────────────────────────────────────────────────────────

fn gn(epoch: u64, seq: u64) -> Generation {
    Generation { epoch, seq }
}

fn file(path: &str, bytes: &[u8]) -> UploadedFile {
    UploadedFile {
        path: path.to_owned(),
        mode: FileMode::Regular,
        bytes: bytes.to_vec(),
    }
}

/// Recompute the server-trusted ids a candidate publish will derive (the device op signs over these).
fn compute_ids(parents: &[[u8; 32]], files: &[UploadedFile]) -> ([u8; 32], [u8; 32]) {
    let manifest: Vec<ManifestEntry> = files
        .iter()
        .map(|f| ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let digest = digest::bundle_digest(&manifest).unwrap();
    let version_id = sign::commit_id(&Commit {
        parents,
        tree: digest,
        author: AUTHOR,
        message: MESSAGE,
    })
    .unwrap();
    (version_id, digest)
}

/// Parse a canonical UUID op-id into its 16 bytes (no `uuid` dep — hex over the hyphen-stripped string).
fn op_id_bytes(op_id: &str) -> [u8; 16] {
    let hex: String = op_id.chars().filter(|c| *c != '-').collect();
    let mut out = [0u8; 16];
    hex::decode_to_slice(&hex, &mut out).unwrap();
    out
}

/// Sign a device op over the server-trusted identity → base64url-unpadded (the `Topos-Device-Signature`).
#[allow(clippy::too_many_arguments)]
fn sign_sig(
    key: &SigningKey,
    op: DeviceOp,
    op_id: &str,
    expected: Generation,
    version_id: [u8; 32],
    digest: [u8; 32],
) -> String {
    let fields = DeviceOpFields {
        workspace_id: WS,
        skill_id: SKILL,
        op,
        op_id: op_id_bytes(op_id),
        device_key_id: DKID,
        expected_epoch: expected.epoch,
        expected_seq: expected.seq,
        commit_id: version_id,
        bundle_digest: digest,
    };
    let sig = key.sign(&device_op_preimage(&fields).unwrap()).to_bytes();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig)
}

/// Build a candidate-bearing request body (publish/propose share the shape).
fn candidate_body(
    op_id: &str,
    expected: Generation,
    parents: &[[u8; 32]],
    files: &[UploadedFile],
) -> Vec<u8> {
    let wire_files: Vec<serde_json::Value> = files
        .iter()
        .map(|f| {
            serde_json::json!({
                "path": f.path,
                "mode": f.mode.as_str(),
                "content_base64": base64::engine::general_purpose::STANDARD.encode(&f.bytes),
            })
        })
        .collect();
    let body = serde_json::json!({
        "workspace_id": WS,
        "skill_id": SKILL,
        "op_id": op_id,
        "device_key_id": DKID,
        "expected": { "epoch": expected.epoch, "seq": expected.seq },
        "candidate": {
            "files": wire_files,
            "parents": parents.iter().map(hex::encode).collect::<Vec<_>>(),
            "author": AUTHOR,
            "message": MESSAGE,
        },
    });
    serde_json::to_vec(&body).unwrap()
}

fn post(uri: &str, sig: &str, body: Vec<u8>) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("topos-device-signature", sig)
        .body(Body::from(body))
        .unwrap()
}

fn get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    builder.body(Body::empty()).unwrap()
}

/// Run a request against a fresh router; return (status, response headers clone, body bytes).
async fn run(ctx: &Ctx, req: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = ctx.app().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap()
        .to_vec();
    (status, headers, body)
}

fn envelope(bytes: &[u8]) -> JsonEnvelope {
    serde_json::from_slice(bytes).expect("response body is a JsonEnvelope")
}

// ── publish ─────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn publish_happy_path_moves_current_and_returns_a_canonical_ok_receipt() {
    let ctx = setup("publish-ok").await;
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

#[tokio::test]
async fn a_write_with_a_non_uuid_op_id_is_rejected_before_ingest() {
    // A path-safe op_id that is NOT a canonical UUID: `OpId::parse` accepts it, but the authority binds op_id
    // as 16 bytes only AFTER it ingests + leases the candidate, so without an edge guard a malformed
    // unauthenticated request would pin the uploaded objects on the later parse failure. The edge must reject
    // it with a 400 BEFORE reaching the authority (so the candidate is never ingested).
    let ctx = setup("publish-bad-opid").await;
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

#[tokio::test]
async fn an_idempotent_retry_replays_a_byte_identical_response() {
    let ctx = setup("publish-retry").await;
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

#[tokio::test]
async fn a_stale_publish_is_a_200_conflict_with_a_rebase_next_action() {
    let ctx = setup("publish-conflict").await;
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

// ── propose ───────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn propose_opens_a_proposal_needs_review_without_moving_current() {
    let ctx = setup("propose-ok").await;
    let (g_vid, _) = seed_genesis(&ctx, "30000000-0000-4000-8000-000000000000").await;

    let op = "30000000-0000-4000-8000-000000000001";
    let files = vec![file("SKILL.md", b"a proposed change\n")];
    let (vid, digest) = compute_ids(&[g_vid], &files);
    let sig = sign_sig(
        &ctx.key,
        DeviceOp::PublishPropose,
        op,
        gn(1, 1),
        vid,
        digest,
    );

    let (status, _, bytes) = run(
        &ctx,
        post(
            "/v1/proposals",
            &sig,
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

// ── revert ────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn revert_is_a_forward_commit_that_advances_seq() {
    let ctx = setup("revert-ok").await;
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

// ── review ────────────────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn review_approve_promotes_an_open_proposal() {
    let ctx = setup("review-ok").await;
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

// ── reads: 404-not-403 ──────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn a_bundle_read_with_no_or_bad_credential_is_404_never_403() {
    let ctx = setup("read-404").await;
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

#[tokio::test]
async fn a_scope_path_mismatch_read_is_404_never_403() {
    let ctx = setup("read-mismatch").await;
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

// ── current: 200 + the commit-sensitive 304 ──────────────────────────────────────────────────────────

#[tokio::test]
async fn current_serves_the_signed_record_and_a_commit_sensitive_304() {
    let ctx = setup("current").await;
    let (g_vid, _) = seed_genesis(&ctx, "60000000-0000-4000-8000-000000000000").await;
    let uri = format!("/v1/current/{READ_TOKEN}");
    let known_version = hex::encode(g_vid);

    // 200 with the signed record + an ETag of "<epoch>.<seq>".
    let (status, headers, bytes) = run(&ctx, get(&uri, &[])).await;
    assert_eq!(status, StatusCode::OK);
    let etag = headers
        .get(header::ETAG)
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert_eq!(etag, "\"1.1\"");
    let record: SignedCurrentRecord =
        serde_json::from_slice(&bytes).expect("the body is the signed record");
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

// ── transport: a malformed body is an envelope-shaped 400 ─────────────────────────────────────────────

#[tokio::test]
async fn a_malformed_body_is_a_400_envelope_not_axums_plain_text() {
    let ctx = setup("bad-body").await;
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

#[tokio::test]
async fn a_missing_device_signature_header_is_a_400() {
    let ctx = setup("no-sig").await;
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
