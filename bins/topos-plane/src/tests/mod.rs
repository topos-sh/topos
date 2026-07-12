//! Per-route integration tests — `tower::ServiceExt::oneshot` against `router(state)`, no socket.
//!
//! Each test seeds a real [`Authority`] through the feature-gated test-fixtures shims (a registered device
//! WITH its workspace credential, a CONFIRMED workspace member, and — where needed — a published genesis),
//! then drives the wire exactly as a client would: the workspace credential in the `Authorization: Bearer`
//! header (one bearer secret per enrolled device authenticates every read AND write — no signature, no body
//! credential; the plane resolves it to the device's registry row and gates on confirmed membership), and
//! the conditional-GET headers. They assert the status, the canonical receipt/envelope shape, and the
//! commit-sensitive 304.
//!
//! The suite mirrors `src/routes/`: one child module per route family, plus `misc` for the cross-route
//! tests (state construction, the maintenance pass, the wire-error envelope). This module is the shared
//! support half — the two seeded fixtures ([`Ctx`] for the write/read routes, [`EnrollCtx`] for
//! enrollment + governance) and the request/wire helpers they drive.

mod bootstrap;
mod bundles;
mod current;
mod enroll;
mod governance;
mod misc;
mod policy;
mod proposals;
mod publish;
mod reverts;
mod reviews;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use base64::Engine as _;
use sqlx::PgPool;
use tower::ServiceExt as _;

use plane_store::{
    Authority, DeploymentMode, EnrollmentConfig, FileMode, OpId, Principal, SkillId, UploadedFile,
    WorkspaceId,
};
use topos_core::digest::{self, ManifestEntry};
use topos_core::identity::{self, Commit};
use topos_types::requests::{
    DeviceAuthorizeResponse, DeviceTokenResponse, DeviceTokenStatus, PasscodeConfirmResponse,
    PasscodeConfirmStatus,
};
use topos_types::{Generation, JsonEnvelope, TerminalOutcome};

use crate::enroll::mailer::FakeMailer;
use crate::{PlaneState, router};

// ── constants ──────────────────────────────────────────────────────────────────────────────────────
const WS: &str = "w_acme";
const SKILL: &str = "s_deploy";
const DKID: &str = "dk_a";
const PRINCIPAL: &str = "p_dev";
/// The seeded device's ONE workspace credential — the bearer secret it presents on every read AND write
/// (`Authorization: Bearer <credential>`). Distinct per device across a DB (the registry's stored sha256 is
/// globally unique); each `#[sqlx::test]` gets a fresh DB, so a single constant per device is enough.
const CREDENTIAL: &str = "cred_dev";
const AUTHOR: &str = "d_test";
const MESSAGE: &str = "topos publish";
const CREATED_AT: &str = "2026-06-29T00:00:00Z";
const NOW: i64 = 1_000_000;
const KEY_SEED: u8 = 7;

// ── fixture ────────────────────────────────────────────────────────────────────────────────────────

/// A seeded plane (temp dirs cleaned on drop): a registered device WITH its workspace credential, its
/// principal a CONFIRMED workspace member (the read AND write gate).
struct Ctx {
    dir: PathBuf,
    state: PlaneState,
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

/// A deterministic device public key from a seed byte. Nothing signs or verifies with it anymore — the
/// registry row it registers is looked up by `device_key_id`; the key is just the device's presented identity.
fn dev_pubkey(seed: u8) -> [u8; 32] {
    [seed; 32]
}

async fn setup(pool: PgPool, tag: &str) -> Ctx {
    let dir = unique_dir(tag);
    let authority =
        Authority::from_pool(pool, &dir.join("git"), &dir.join("large")).expect("open authority");
    let ws = WorkspaceId::parse(WS).unwrap();
    let principal = Principal::parse(PRINCIPAL).unwrap();
    // The device carries its workspace credential; membership (a CONFIRMED workspace_member seat) is the read
    // AND write gate — the per-skill roster gates nothing in the credential model, so it is not seeded.
    authority
        .seed_device(
            &ws,
            DKID,
            &dev_pubkey(KEY_SEED),
            &principal,
            false,
            CREDENTIAL,
        )
        .await
        .unwrap();
    authority
        .seed_workspace_member(&ws, &principal, "member", "confirmed")
        .await
        .unwrap();
    // Disable the rate limiter so a handful of test requests never trips it.
    let state = PlaneState::new(Arc::new(authority)).with_rate_limit(crate::Limits {
        burst: 1.0,
        refill_per_sec: 1.0,
        enabled: false,
    });
    Ctx { dir, state }
}

/// Seed a published genesis at (1,1); returns (genesis version_id, genesis bundle_digest).
async fn seed_genesis(ctx: &Ctx, op_id: &str) -> ([u8; 32], [u8; 32]) {
    let receipt = ctx
        .authority()
        .seed_published_genesis(
            &WorkspaceId::parse(WS).unwrap(),
            &SkillId::parse(SKILL).unwrap(),
            CREDENTIAL,
            &OpId::parse(op_id).unwrap(),
            vec![file("SKILL.md", b"genesis v0\n")],
            AUTHOR,
            MESSAGE,
            None,
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

/// Recompute the server-trusted ids a candidate publish will derive (so a test can assert the receipt's
/// server-rehashed `version_id` / `bundle_digest`).
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
    let version_id = identity::commit_id(&Commit {
        parents,
        tree: digest,
        author: AUTHOR,
        message: MESSAGE,
    })
    .unwrap();
    (version_id, digest)
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
    // No credential material rides the body — the workspace credential is the `Authorization: Bearer` header.
    let body = serde_json::json!({
        "workspace_id": WS,
        "skill_id": SKILL,
        "op_id": op_id,
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

/// A candidate-write POST presenting the seeded device's workspace credential as `Authorization: Bearer`
/// (the one bearer secret authenticating every write — no signature, no body credential).
fn post(uri: &str, body: Vec<u8>) -> Request<Body> {
    post_as(uri, body, Some(CREDENTIAL))
}

/// A candidate-write POST presenting a CHOSEN bearer credential (or NONE) — for the unknown-credential
/// (→ 200 DENIED) and missing-header (→ 404 uniform miss) write cases.
fn post_as(uri: &str, body: Vec<u8>, credential: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(cred) = credential {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {cred}"));
    }
    builder.body(Body::from(body)).unwrap()
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

// ══ the enrollment + governance fixture family ════════════════════════════════════════════════════════
//
// The shared support for the `bootstrap` / `enroll` / `governance` test modules: the [`EnrollCtx`] scaffold
// (a cloud workspace, a confirmed owner + device, a FakeMailer) and the wire helpers their per-route
// proofs drive. The comprehensive acceptance suite + the cross-component `follow` e2e live in `tests/`.

const OWNER_DK: &str = "dk_owner";
const OWNER_PRINCIPAL: &str = "owner@acme.com";
const OWNER_SEED: u8 = 11;
/// The owner device's workspace credential — the Bearer secret the governance mutations present as the actor.
const OWNER_CRED: &str = "cred_owner";
const MEMBER_DK: &str = "dk_member";
const MEMBER_PRINCIPAL: &str = "member@acme.com";
const MEMBER_SEED: u8 = 12;
const MEMBER_CRED: &str = "cred_member";
const TARGET_DK: &str = "dk_target";
const TARGET_PRINCIPAL: &str = "target@acme.com";
const TARGET_SEED: u8 = 13;
const TARGET_CRED: &str = "cred_target";
const ALICE_EMAIL: &str = "alice@acme.com";
const ALICE_SEED: u8 = 14;
const ENROLL_BASE_URL: &str = "https://plane.test";

/// A seeded enrollment plane: a cloud `workspace`, a confirmed owner + its registered device, the enrollment
/// secret loaded, and a `FakeMailer` injected so the passcode is readable without SMTP.
struct EnrollCtx {
    dir: PathBuf,
    state: PlaneState,
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

async fn enroll_setup(pool: PgPool, tag: &str) -> EnrollCtx {
    enroll_setup_mode(pool, tag, DeploymentMode::Cloud).await
}

/// An [`EnrollCtx`] whose plane runs at the given deployment mode (the standup tests need a self-host
/// plane, whose standup start must be the uniform 404).
async fn enroll_setup_mode(pool: PgPool, tag: &str, mode: DeploymentMode) -> EnrollCtx {
    enroll_setup_full(pool, tag, mode, None).await
}

/// An [`EnrollCtx`] whose minted `/i/` links ride a SEPARATE public link base (the hosted split: links on
/// the web origin, the API on the plane base) — the content-negotiation + link-minting tests use it.
async fn enroll_setup_link_base(pool: PgPool, tag: &str, link_base: &str) -> EnrollCtx {
    enroll_setup_full(pool, tag, DeploymentMode::Cloud, Some(link_base)).await
}

async fn enroll_setup_full(
    pool: PgPool,
    tag: &str,
    mode: DeploymentMode,
    link_base: Option<&str>,
) -> EnrollCtx {
    let dir = unique_dir(tag);
    let authority = Authority::from_pool(pool, &dir.join("git"), &dir.join("large"))
        .expect("open authority")
        .with_enrollment_config(EnrollmentConfig {
            secret_path: dir.join("enroll.secret"),
            base_url: ENROLL_BASE_URL.to_owned(),
            verify_base_url: None,
            link_base_url: link_base.map(str::to_owned),
            deployment_mode: mode,
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
    authority
        .seed_device(
            &ws,
            OWNER_DK,
            &dev_pubkey(OWNER_SEED),
            &owner,
            false,
            OWNER_CRED,
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
            verify_base_url: ENROLL_BASE_URL.to_owned(),
            link_base_url: link_base.unwrap_or(ENROLL_BASE_URL).to_owned(),
            strict_deployment_mode: Some(mode),
            deployment_mode: mode,
            enrollment_method: "passcode".to_owned(),
            smtp: None,
        })
        .with_mailer(fake.clone());
    EnrollCtx { dir, state, fake }
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

/// A POST with a JSON body and NO `Authorization` header — the unauthenticated enrollment steps (device
/// authorize/token, passcode, redeem, admin-claim) that carry their own grant/token in the body. Governance
/// mutations use [`req_json_auth`] instead (the actor rides the Bearer workspace credential).
fn post_nosig(uri: &str, body: serde_json::Value) -> Request<Body> {
    req_json("POST", uri, body)
}

/// A request with a JSON body for any method (POST/PUT/DELETE enrollment/redeem). No `Authorization` header:
/// the enrollment steps are unauthenticated and carry their own grant/token in the body.
fn req_json(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// A governance request (POST/PUT/DELETE) presenting the acting device's workspace credential as
/// `Authorization: Bearer` — the actor of every governance mutation now rides the header, never a body field.
fn req_json_auth(
    method: &str,
    uri: &str,
    body: serde_json::Value,
    credential: &str,
) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {credential}"))
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

/// Drive `POST /v1/invites` as the owner (the owner device credential rides the `Authorization: Bearer`
/// header); return the success envelope (asserts a 200).
async fn create_invite(ctx: &EnrollCtx, op_id: &str, emails: &[&str], skill: &str) -> JsonEnvelope {
    let body = serde_json::json!({
        "workspace_id": WS,
        "op_id": op_id,
        "emails": emails,
        "role": "member",
        "skills": [{ "skill_id": skill, "name": "Deploy" }],
    });
    let (status, _, bytes) = send(
        ctx.app(),
        req_json_auth("POST", "/v1/invites", body, OWNER_CRED),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    envelope(&bytes)
}

/// Run the full cloud device-auth flow (authorize → poll → passcode → confirm → poll) to a `Granted` grant.
/// Returns `(grant, user_code, device_public_key)`.
async fn enroll_to_grant(
    ctx: &EnrollCtx,
    invite_op: &str,
    device_seed: u8,
    email: &str,
    skill: &str,
) -> (String, String, [u8; 32]) {
    let invite = create_invite(ctx, invite_op, &[email], skill).await;
    let token = token_from_link(invite.data["invite_link"].as_str().unwrap());

    let device_pk = dev_pubkey(device_seed);

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

    (grant, auth.user_code, device_pk)
}
