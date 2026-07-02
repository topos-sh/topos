//! Per-route integration tests — `tower::ServiceExt::oneshot` against `router(state)`, no socket.
//!
//! Each test seeds a real [`Authority`] through the feature-gated test-fixtures shims (a registered device,
//! a rostered principal, a minted read token, and — where needed — a signed genesis), then drives the wire
//! exactly as a client would: a `Topos-Device-Signature` header over the SERVER-rehashed candidate ids, a
//! JSON body, the conditional-GET headers. They assert the status, the canonical receipt/envelope shape, and
//! the commit-sensitive 304.
//!
//! The suite mirrors `src/routes/`: one child module per route family, plus `misc` for the cross-route
//! tests (state construction, the maintenance pass, the wire-error envelope). This module is the shared
//! support half — the two seeded fixtures ([`Ctx`] for the write/read routes, [`EnrollCtx`] for
//! enrollment + governance) and the request/signing helpers they drive.

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
use ed25519_dalek::{Signer as _, SigningKey};
use sqlx::PgPool;
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

async fn setup(pool: PgPool, tag: &str) -> Ctx {
    let dir = unique_dir(tag);
    let authority = Authority::from_pool(pool, &dir.join("git"), &dir.join("large"))
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

// ══ the enrollment + governance fixture family ════════════════════════════════════════════════════════
//
// The shared support for the `bootstrap` / `enroll` / `governance` test modules: the [`EnrollCtx`] scaffold
// (a cloud workspace, a confirmed owner + device, a FakeMailer) and the signing/wire helpers their per-route
// proofs drive. The comprehensive acceptance suite + the cross-component `follow` e2e live in `tests/`.

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

async fn enroll_setup(pool: PgPool, tag: &str) -> EnrollCtx {
    let dir = unique_dir(tag);
    let authority = Authority::from_pool(pool, &dir.join("git"), &dir.join("large"))
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
