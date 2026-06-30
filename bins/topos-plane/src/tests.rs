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

use plane_store::{
    Authority, DeploymentMode, EnrollmentConfig, FileMode, OpId, Principal, Role, SkillId,
    UploadedFile, WorkspaceId,
};
use topos_core::digest::{self, ManifestEntry};
use topos_core::sign::{
    self, Commit, DeviceOp, DeviceOpFields, EnrollFields, GovernanceOpFields, GovernanceOpKind,
    device_op_preimage, enroll_preimage, governance_op_preimage,
};
use topos_types::bootstrap::{BootstrapData, ConsentMode};
use topos_types::requests::{
    DeviceAuthorizeResponse, DeviceTokenResponse, DeviceTokenStatus, PasscodeConfirmResponse,
    PasscodeConfirmStatus, RedeemResponse,
};
use topos_types::{Generation, JsonEnvelope, SignatureAlg, SignedCurrentRecord, TerminalOutcome};

use crate::enroll::mailer::FakeMailer;
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
    send(ctx.app(), req).await
}

/// Drive any router via `oneshot`; return (status, response headers clone, body bytes). Shared by the
/// write/read tests (over [`Ctx`]) and the enrollment/governance tests (over [`EnrollCtx`]).
async fn send(
    app: axum::Router,
    req: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let resp = app.oneshot(req).await.unwrap();
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

// ── enrollment wiring: with_enroll_config / with_mailer / the accessors ───────────────────────────────

#[tokio::test]
async fn enroll_config_and_injected_mailer_are_readable() {
    use crate::enroll::mailer::{FakeMailer, MailContext, Passcode};

    let ctx = setup("state-enroll").await;
    let fake = Arc::new(FakeMailer::default());
    // with_enroll_config sets the static config (no SMTP ⇒ a NoopMailer); with_mailer overrides it for the
    // test so we can assert the handler sends through exactly the injected mailer.
    let state = ctx
        .state
        .clone()
        .with_enroll_config(crate::state::EnrollConfig {
            base_url: "https://plane.test".to_owned(),
            deployment_mode: plane_store::DeploymentMode::Cloud,
            enrollment_method: "passcode".to_owned(),
            smtp: None,
        })
        .with_mailer(fake.clone());

    assert_eq!(state.enroll().base_url, "https://plane.test");
    assert_eq!(state.enroll().enrollment_method, "passcode");
    assert_eq!(
        state.enroll().deployment_mode,
        plane_store::DeploymentMode::Cloud
    );

    // The accessor returns exactly the injected mailer — a send lands in the FakeMailer's record.
    let mail_ctx = MailContext {
        workspace_display_name: "Acme".to_owned(),
        base_url: "https://plane.test".to_owned(),
    };
    state
        .mailer()
        .send_passcode(
            "alice@acme.com",
            &Passcode::new("424242".to_owned()),
            &mail_ctx,
        )
        .unwrap();
    let sent = fake.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].to, "alice@acme.com");
    assert_eq!(sent[0].code, "424242");
}

/// The runtime parity guard for the **single construction path**: `PlaneState::open_sqlite` (the leak-free
/// constructor the bin + a downstream plane use) runs against a real tempdir and yields a SERVING state. It
/// is the only test that EXECUTES the constructor (the bin isn't run in CI; the `open_sqlite` doc-test is
/// `no_run`), so it pins that the internal resolution matches the bin's (mode `"cloud"` ⇒ `Cloud`; no SMTP ⇒
/// `device_code`) and that the composed `router(state)` answers — an unknown read token is the
/// indistinguishable 404, never a panic/500.
#[tokio::test]
async fn open_sqlite_builds_a_serving_state() {
    let dir = unique_dir("open-sqlite");
    let state = PlaneState::open_sqlite(crate::PlaneConfig {
        db_path: dir.join("db"),
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
    .expect("open_sqlite builds a serving state");

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

// ── proposals listing: 200 + the open proposals, 404 on a bad token / scope mismatch ──────────────────

#[tokio::test]
async fn list_proposals_route_returns_open_proposals_and_404s_a_bad_token() {
    // The proposals-listing read over `router(state)`: open a proposal via the HTTP propose route, then GET
    // the list with the read token → 200 + the proposal's @hash + base; a bad/absent token or a scope/path
    // mismatch → the indistinguishable 404 (never 401/403).
    let ctx = setup("list-proposals").await;
    let (g_vid, _) = seed_genesis(&ctx, "70000000-0000-4000-8000-000000000000").await;

    // Open a proposal (a child of genesis) via `POST /v1/proposals`.
    let op = "70000000-0000-4000-8000-000000000001";
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
    let (status, _, _) = run(
        &ctx,
        post(
            "/v1/proposals",
            &sig,
            candidate_body(op, gn(1, 1), &[g_vid], &files),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // GET the proposals list with the read token → 200 + the one open proposal.
    let uri = format!("/v1/workspaces/{WS}/skills/{SKILL}/proposals");
    let (s_ok, headers, bytes) = run(
        &ctx,
        get(&uri, &[("authorization", &format!("Bearer {READ_TOKEN}"))]),
    )
    .await;
    assert_eq!(s_ok, StatusCode::OK);
    // The list is MUTABLE, so it carries a short must-revalidate window — never the version read's `immutable`.
    let cc = headers
        .get(header::CACHE_CONTROL)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        cc.contains("must-revalidate") && !cc.contains("immutable"),
        "the proposals list must not be immutable-cached: {cc}"
    );
    let list: topos_types::requests::WireProposalList =
        serde_json::from_slice(&bytes).expect("the body is a WireProposalList");
    assert_eq!(list.proposals.len(), 1);
    assert_eq!(list.proposals[0].version_id, hex::encode(vid));
    assert_eq!(list.proposals[0].base_generation, gn(1, 1));
    assert!(!list.proposals[0].created_at.is_empty());

    // A bogus bearer token → the indistinguishable 404.
    let (s_bad, _, _) = run(
        &ctx,
        get(&uri, &[("authorization", "Bearer not-a-real-token")]),
    )
    .await;
    assert_eq!(s_bad, StatusCode::NOT_FOUND);

    // A scope/path mismatch (a DIFFERENT skill in the path, valid token) → 404, BEFORE any roster read.
    let mismatch = format!("/v1/workspaces/{WS}/skills/s_other/proposals");
    let (s_mis, _, _) = run(
        &ctx,
        get(
            &mismatch,
            &[("authorization", &format!("Bearer {READ_TOKEN}"))],
        ),
    )
    .await;
    assert_eq!(s_mis, StatusCode::NOT_FOUND);
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

// ══ enrollment + governance ═══════════════════════════════════════════════════════════════════════════
//
// Per-route wiring proofs over `router(state)`: the unauthenticated bootstrap, the full device-auth → passcode
// → redeem chain (signing the enroll possession frame with a test key), and the governance invite/revoke
// authority (owner-OK, member-DENIED). The comprehensive acceptance suite + the cross-component `follow` e2e
// land in the final test step.

const OWNER_DK: &str = "dk_owner";
const OWNER_PRINCIPAL: &str = "owner@acme.com";
const OWNER_SEED: u8 = 11;
const MEMBER_DK: &str = "dk_member";
const MEMBER_PRINCIPAL: &str = "member@acme.com";
const MEMBER_SEED: u8 = 12;
const TARGET_DK: &str = "dk_target";
const TARGET_PRINCIPAL: &str = "target@acme.com";
const TARGET_SEED: u8 = 13;
const ALICE_EMAIL: &str = "alice@acme.com";
const ALICE_SEED: u8 = 14;
const ENROLL_BASE_URL: &str = "https://plane.test";

/// A seeded enrollment plane: a cloud `workspace`, a confirmed owner + its registered device, the enrollment
/// secret loaded, and a `FakeMailer` injected so the passcode is readable without SMTP.
struct EnrollCtx {
    dir: PathBuf,
    state: PlaneState,
    owner_key: SigningKey,
    fake: Arc<FakeMailer>,
}

impl Drop for EnrollCtx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl EnrollCtx {
    fn app(&self) -> axum::Router {
        router(self.state.clone())
    }

    fn authority(&self) -> &Authority {
        self.state.authority()
    }
}

async fn enroll_setup(tag: &str) -> EnrollCtx {
    let dir = unique_dir(tag);
    let authority = Authority::open_sqlite(&dir.join("db"), &dir.join("git"), &dir.join("large"))
        .await
        .expect("open authority")
        .with_plane_key(&dir.join("plane.key"))
        .expect("plane key")
        .with_enrollment_config(EnrollmentConfig {
            secret_path: dir.join("enroll.secret"),
            base_url: ENROLL_BASE_URL.to_owned(),
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "passcode".to_owned(),
        })
        .expect("enrollment config");
    let ws = WorkspaceId::parse(WS).unwrap();
    authority
        .seed_workspace(&ws, "Acme", "unverified", "cloud")
        .await
        .unwrap();
    let owner = Principal::parse(OWNER_PRINCIPAL).unwrap();
    authority
        .seed_workspace_member(&ws, &owner, "owner", "confirmed")
        .await
        .unwrap();
    let owner_key = dev_key(OWNER_SEED);
    authority
        .seed_device(
            &ws,
            OWNER_DK,
            &owner_key.verifying_key().to_bytes(),
            &owner,
            false,
        )
        .await
        .unwrap();

    let fake = Arc::new(FakeMailer::default());
    // with_enroll_config first (it builds a NoopMailer from the no-SMTP config), then with_mailer overrides it
    // with the FakeMailer so the passcode handler's send is readable.
    let state = PlaneState::new(Arc::new(authority))
        .with_rate_limit(crate::Limits {
            burst: 1.0,
            refill_per_sec: 1.0,
            enabled: false,
        })
        .with_enroll_config(crate::state::EnrollConfig {
            base_url: ENROLL_BASE_URL.to_owned(),
            deployment_mode: DeploymentMode::Cloud,
            enrollment_method: "passcode".to_owned(),
            smtp: None,
        })
        .with_mailer(fake.clone());
    EnrollCtx {
        dir,
        state,
        owner_key,
        fake,
    }
}

/// The server-derived device key id from a raw public key — `dk_<first 32 hex of sha256(pubkey)>` (the same
/// derivation the authority uses on redeem). The enroll frame binds it; a client never asserts it.
fn device_key_id_for(pubkey: &[u8; 32]) -> String {
    let hex = digest::to_hex(&digest::sha256(pubkey));
    format!("dk_{}", &hex[..32])
}

/// base64url-unpadded a raw 32-byte key (the device public key on the wire).
fn b64key(pubkey: &[u8; 32]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(pubkey)
}

/// Sign a governance op over the canonical frame → base64url (the `Topos-Device-Signature` header value).
fn sign_governance(
    signer: &SigningKey,
    signer_dk: &str,
    op_id: &str,
    op: GovernanceOpKind,
) -> String {
    let fields = GovernanceOpFields {
        workspace_id: WS,
        op_id: op_id_bytes(op_id),
        device_key_id: signer_dk,
        op,
    };
    let sig = signer
        .sign(&governance_op_preimage(&fields).unwrap())
        .to_bytes();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig)
}

/// Sign an enrollment possession frame over the SERVER-trusted values → base64url. `device_auth_id` is the
/// session's `user_code` (what the authority binds), `offered` the invite's offered skill ids.
fn sign_enroll(
    signer: &SigningKey,
    grant_hash: [u8; 32],
    device_auth_id: &str,
    device_key_id: &str,
    pubkey: [u8; 32],
    offered: &[&str],
) -> String {
    let fields = EnrollFields {
        workspace_id: WS,
        grant_hash,
        device_auth_id,
        device_key_id,
        device_public_key: pubkey,
        offered_skill_ids: offered,
    };
    let sig = signer.sign(&enroll_preimage(&fields).unwrap()).to_bytes();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig)
}

/// A POST with a JSON body and NO device-signature header (the enrollment reads/steps that are not signed).
fn post_nosig(uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// A request with a JSON body + a device-signature header, for any method (POST/PUT/DELETE governance/redeem).
fn signed_req(method: &str, uri: &str, sig: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header("topos-device-signature", sig)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// The `<token>` of an `<base_url>/i/<token>` invite link.
fn token_from_link(link: &str) -> String {
    link.rsplit_once("/i/")
        .expect("an invite link carries /i/")
        .1
        .to_owned()
}

/// Block (briefly) until the fire-and-forget passcode send lands in the `FakeMailer`, returning the code.
fn wait_for_passcode(fake: &FakeMailer) -> String {
    for _ in 0..200 {
        if let Some(m) = fake.sent().into_iter().next() {
            return m.code;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("no passcode mailed within the timeout");
}

/// Drive `POST /v1/invites` as the owner; return the success envelope (asserts a 200).
async fn create_invite(ctx: &EnrollCtx, op_id: &str, emails: &[&str], skill: &str) -> JsonEnvelope {
    let skills = [skill];
    let sig = sign_governance(
        &ctx.owner_key,
        OWNER_DK,
        op_id,
        GovernanceOpKind::Invite {
            role: Role::Member.signing_byte(),
            expires_at: 0,
            emails,
            skills: &skills,
        },
    );
    let body = serde_json::json!({
        "workspace_id": WS,
        "op_id": op_id,
        "device_key_id": OWNER_DK,
        "emails": emails,
        "role": "member",
        "skills": [{ "skill_id": skill, "name": "Deploy" }],
    });
    let (status, _, bytes) = send(ctx.app(), signed_req("POST", "/v1/invites", &sig, body)).await;
    assert_eq!(status, StatusCode::OK);
    envelope(&bytes)
}

/// Run the full cloud device-auth flow (authorize → poll → passcode → confirm → poll) to a `Granted` grant.
/// Returns `(grant, user_code, device_public_key, device_key)`.
async fn enroll_to_grant(
    ctx: &EnrollCtx,
    invite_op: &str,
    device_seed: u8,
    email: &str,
    skill: &str,
) -> (String, String, [u8; 32], SigningKey) {
    let invite = create_invite(ctx, invite_op, &[email], skill).await;
    let token = token_from_link(invite.data["invite_link"].as_str().unwrap());

    let device = dev_key(device_seed);
    let device_pk = device.verifying_key().to_bytes();

    // authorize.
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/authorize",
            serde_json::json!({
                "invite_token": token,
                "device_public_key": b64key(&device_pk),
                "machine_name": "alice-laptop",
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let auth: DeviceAuthorizeResponse = serde_json::from_slice(&b).unwrap();

    // poll → pending (cloud, no identity yet).
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/token",
            serde_json::json!({ "device_code": auth.device_code }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let poll: DeviceTokenResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(poll.status, DeviceTokenStatus::Pending);

    // passcode start → the FakeMailer receives the code (fire-and-forget send).
    let (s, _, _) = send(
        ctx.app(),
        post_nosig(
            "/v1/enroll/passcode",
            serde_json::json!({ "user_code": auth.user_code, "email": email }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let code = wait_for_passcode(&ctx.fake);

    // passcode confirm → the session's identity is confirmed.
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/enroll/passcode/confirm",
            serde_json::json!({ "user_code": auth.user_code, "email": email, "code": code }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let confirm: PasscodeConfirmResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(confirm.status, PasscodeConfirmStatus::Confirmed);

    // poll again → granted.
    let (s, _, b) = send(
        ctx.app(),
        post_nosig(
            "/v1/device/token",
            serde_json::json!({ "device_code": auth.device_code }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let poll: DeviceTokenResponse = serde_json::from_slice(&b).unwrap();
    assert_eq!(poll.status, DeviceTokenStatus::Granted);
    let grant = poll.grant.expect("a granted poll carries the grant");

    (grant, auth.user_code, device_pk, device)
}

#[tokio::test]
async fn invite_bootstrap_returns_the_pinned_plane_key_no_role_and_auto_land_false() {
    let ctx = enroll_setup("enroll-bootstrap").await;
    let env = create_invite(
        &ctx,
        "aaaaaaaa-0000-4000-8000-000000000001",
        &[ALICE_EMAIL],
        SKILL,
    )
    .await;
    let token = token_from_link(env.data["invite_link"].as_str().unwrap());

    let (status, _, bytes) = send(ctx.app(), get(&format!("/i/{token}"), &[])).await;
    assert_eq!(status, StatusCode::OK);
    let data: BootstrapData = serde_json::from_slice(&bytes).expect("the body is a BootstrapData");
    // The plane signing key is pinned (the trust root the device TOFU-pins).
    assert_eq!(data.plane.signing_key.alg, SignatureAlg::Ed25519);
    assert!(!data.plane.signing_key.key_id.is_empty());
    assert!(!data.plane.signing_key.value.is_empty());
    // No role; a first-received skill is never silently landed; the offered skill is disclosed.
    assert!(!data.invite.first_receive_auto_land);
    assert_eq!(data.invite.consent, ConsentMode::DirectHumanFirstReceive);
    assert_eq!(data.workspace.workspace_id, WS);
    assert_eq!(
        data.plane.deployment_mode,
        topos_types::bootstrap::DeploymentMode::Cloud
    );
    assert!(data.offered_skills.iter().any(|s| s.skill_id == SKILL));
    // The bootstrap carries no role anywhere.
    let raw: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(raw.get("role").is_none() && raw["invite"].get("role").is_none());

    // A bad/unknown token ⇒ the indistinguishable 404.
    let (s404, _, _) = send(ctx.app(), get("/i/not-a-real-token", &[])).await;
    assert_eq!(s404, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn full_device_flow_enrolls_and_redeems_read_creds() {
    let ctx = enroll_setup("enroll-redeem").await;
    let (grant, user_code, device_pk, device_key) = enroll_to_grant(
        &ctx,
        "bbbbbbbb-0000-4000-8000-000000000001",
        ALICE_SEED,
        ALICE_EMAIL,
        SKILL,
    )
    .await;

    let device_key_id = device_key_id_for(&device_pk);
    let grant_hash = digest::sha256(grant.as_bytes());
    let sig = sign_enroll(
        &device_key,
        grant_hash,
        &user_code,
        &device_key_id,
        device_pk,
        &[SKILL],
    );
    let body = serde_json::json!({
        "workspace_id": WS,
        "grant": grant,
        "device_public_key": b64key(&device_pk),
    });

    let (status, _, bytes) = send(
        ctx.app(),
        signed_req("POST", &format!("/v1/workspaces/{WS}/devices"), &sig, body),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "redeem should be ok: {env:?}");
    assert_eq!(env.command, "redeem");
    let resp: RedeemResponse =
        serde_json::from_value(env.data).expect("OK data is a RedeemResponse");
    assert_eq!(resp.workspace_id, WS);
    assert_eq!(resp.device_key_id, device_key_id);
    assert!(
        resp.read_creds.iter().any(|c| c.skill_id == SKILL),
        "a read cred for the offered skill is minted: {resp:?}"
    );
}

#[tokio::test]
async fn a_redeem_with_a_wrong_device_key_is_denied() {
    let ctx = enroll_setup("enroll-wrongkey").await;
    let (grant, user_code, _device_pk, _device_key) = enroll_to_grant(
        &ctx,
        "cccccccc-0000-4000-8000-000000000001",
        ALICE_SEED,
        ALICE_EMAIL,
        SKILL,
    )
    .await;

    // Present a DIFFERENT device key than the grant binds → the grant's device-key match fails.
    let wrong = dev_key(99);
    let wrong_pk = wrong.verifying_key().to_bytes();
    let wrong_dk = device_key_id_for(&wrong_pk);
    let grant_hash = digest::sha256(grant.as_bytes());
    let sig = sign_enroll(
        &wrong,
        grant_hash,
        &user_code,
        &wrong_dk,
        wrong_pk,
        &[SKILL],
    );
    let body = serde_json::json!({
        "workspace_id": WS,
        "grant": grant,
        "device_public_key": b64key(&wrong_pk),
    });

    let (status, _, bytes) = send(
        ctx.app(),
        signed_req("POST", &format!("/v1/workspaces/{WS}/devices"), &sig, body),
    )
    .await;
    // A device-key mismatch is a 200 + DENIED envelope, never a 403.
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(!env.ok, "a wrong device key must be denied: {env:?}");
    assert_eq!(
        env.error.expect("DENIED carries a WireError").outcome,
        TerminalOutcome::Denied
    );
}

#[tokio::test]
async fn an_owner_signed_invite_returns_invite_data() {
    let ctx = enroll_setup("enroll-invite-ok").await;
    let env = create_invite(
        &ctx,
        "dddddddd-0000-4000-8000-000000000001",
        &[ALICE_EMAIL],
        SKILL,
    )
    .await;
    assert!(env.ok, "an owner-signed invite should be ok: {env:?}");
    assert_eq!(env.command, "invite");
    assert!(
        env.data["invite_link"]
            .as_str()
            .is_some_and(|l| l.contains("/i/"))
    );
    // The seeded roster + offered skills are echoed.
    assert!(
        env.data["roster_added"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p == ALICE_EMAIL)
    );
    assert!(
        env.data["skills"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s == SKILL)
    );
}

#[tokio::test]
async fn a_member_signed_invite_is_denied() {
    let ctx = enroll_setup("enroll-invite-denied").await;
    // A non-owner member device (governance requires the owner role for invite).
    let ws = WorkspaceId::parse(WS).unwrap();
    let member = dev_key(MEMBER_SEED);
    let member_principal = Principal::parse(MEMBER_PRINCIPAL).unwrap();
    ctx.authority()
        .seed_workspace_member(&ws, &member_principal, "member", "confirmed")
        .await
        .unwrap();
    ctx.authority()
        .seed_device(
            &ws,
            MEMBER_DK,
            &member.verifying_key().to_bytes(),
            &member_principal,
            false,
        )
        .await
        .unwrap();

    let op = "eeeeeeee-0000-4000-8000-000000000001";
    let emails = [ALICE_EMAIL];
    let skills = [SKILL];
    let sig = sign_governance(
        &member,
        MEMBER_DK,
        op,
        GovernanceOpKind::Invite {
            role: Role::Member.signing_byte(),
            expires_at: 0,
            emails: &emails,
            skills: &skills,
        },
    );
    let body = serde_json::json!({
        "workspace_id": WS,
        "op_id": op,
        "device_key_id": MEMBER_DK,
        "emails": emails,
        "role": "member",
        "skills": [{ "skill_id": SKILL, "name": "Deploy" }],
    });

    let (status, _, bytes) = send(ctx.app(), signed_req("POST", "/v1/invites", &sig, body)).await;
    // A role-denial is a 200 + DENIED envelope (the actor is an authenticated member — nothing to hide).
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(!env.ok, "a member-signed invite must be denied: {env:?}");
    assert_eq!(
        env.error.expect("DENIED carries a WireError").outcome,
        TerminalOutcome::Denied
    );
}

#[tokio::test]
async fn an_owner_revoke_of_a_device_is_ok() {
    let ctx = enroll_setup("enroll-revoke").await;
    // A target device for the owner to revoke.
    let ws = WorkspaceId::parse(WS).unwrap();
    let target = dev_key(TARGET_SEED);
    let target_principal = Principal::parse(TARGET_PRINCIPAL).unwrap();
    ctx.authority()
        .seed_device(
            &ws,
            TARGET_DK,
            &target.verifying_key().to_bytes(),
            &target_principal,
            false,
        )
        .await
        .unwrap();

    let op = "ffffffff-0000-4000-8000-000000000001";
    let sig = sign_governance(
        &ctx.owner_key,
        OWNER_DK,
        op,
        GovernanceOpKind::DeviceRevoke {
            target_device_key_id: TARGET_DK,
        },
    );
    let body = serde_json::json!({
        "workspace_id": WS,
        "op_id": op,
        "device_key_id": OWNER_DK,
        "target_device_key_id": TARGET_DK,
    });

    let (status, _, bytes) = send(
        ctx.app(),
        signed_req(
            "DELETE",
            &format!("/v1/workspaces/{WS}/devices"),
            &sig,
            body,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "an owner revoke should be ok: {env:?}");
    assert_eq!(env.command, "revoke");
}
