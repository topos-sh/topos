//! In-crate authority tests (the `pub(crate)` seed helper is only visible here, never to an external
//! integration crate). They exercise the access rule, cross-workspace and cross-skill isolation, the
//! upload/rehash guard, dedup-obliviousness, and the transaction discipline against a real Postgres
//! database (a fresh per-test one, provisioned + migrated by `#[sqlx::test]` and injected as a `PgPool`)
//! + a real per-workspace git store.

use std::path::PathBuf;

use std::sync::atomic::{AtomicU32, Ordering};

use sqlx::PgPool;
use topos_core::digest;

use crate::db::{ClaimOutcome, InstallOutcome, Location, ObjectStatus};

use crate::{
    Authority, AuthorityError, CandidateUpload, CommitId, DeploymentMode, EnrollmentConfig,
    FileMode, ObjectId, OpId, Principal, SkillId, UploadedFile, WorkspaceId, gc, lifecycle,
};

// ── fixtures + helpers ───────────────────────────────────────────────────────────────────────────

/// A temp dir + an open authority, cleaned up on drop (RAII, so a failing test still tidies).
struct Fixture {
    dir: PathBuf,
    authority: Authority,
}

impl Fixture {
    /// Build a fixture over the injected per-test `PgPool` (each `#[sqlx::test]` provisions + migrates its
    /// own database). The git/large stores stay filesystem — one temp dir per fixture.
    async fn new(pool: PgPool, tag: &str) -> Self {
        Self::build(pool, tag, None).await
    }

    /// A fixture with an overridden size-routing threshold + reject cap — for the offload tests, which
    /// force placement (a tiny threshold routes ordinary test bytes to the large store) and exercise the
    /// reject cap with small payloads.
    async fn with_large_limits(pool: PgPool, tag: &str, threshold: u64, reject_cap: u64) -> Self {
        Self::build(pool, tag, Some((threshold, reject_cap))).await
    }

    async fn build(pool: PgPool, tag: &str, limits: Option<(u64, u64)>) -> Self {
        Self::build_with_mode(pool, tag, limits, DeploymentMode::Cloud).await
    }

    /// A fixture whose ENROLLMENT CONFIG carries the given plane deployment mode — the standup tests need a
    /// self-host plane (whose standup start must be the uniform miss).
    async fn with_mode(pool: PgPool, tag: &str, mode: DeploymentMode) -> Self {
        Self::build_with_mode(pool, tag, None, mode).await
    }

    async fn build_with_mode(
        pool: PgPool,
        tag: &str,
        limits: Option<(u64, u64)>,
        mode: DeploymentMode,
    ) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-ps-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let mut authority = Authority::from_pool(pool, &dir.join("stores"), &dir.join("large"))
            .expect("open authority")
            // Every fixture gets a plane signing key (load-or-generate, 0600) — the pointer-move tests need it;
            // it is simply unused by the read/upload/lifecycle tests.
            .with_plane_key(&dir.join("plane.key"))
            .expect("load plane key")
            // ...and an enrollment config (load-or-generate the 0600 HMAC secret) — the enrollment/governance
            // tests need it; every other test simply never touches it.
            .with_enrollment_config(EnrollmentConfig {
                secret_path: dir.join("enroll.key"),
                base_url: "https://plane.test".to_owned(),
                verify_base_url: None,
                link_base_url: None,
                deployment_mode: mode,
                enrollment_method: "device_code".to_owned(),
            })
            .expect("load enrollment secret");
        if let Some((threshold, reject_cap)) = limits {
            authority = authority.with_large_limits(threshold, reject_cap);
        }
        Self { dir, authority }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn ws(s: &str) -> WorkspaceId {
    WorkspaceId::parse(s).expect("workspace id")
}

fn skill(s: &str) -> SkillId {
    SkillId::parse(s).expect("skill id")
}

fn prin(s: &str) -> Principal {
    Principal::parse(s).expect("principal")
}

fn file(path: &str, bytes: &[u8]) -> UploadedFile {
    UploadedFile {
        path: path.to_owned(),
        mode: FileMode::Regular,
        bytes: bytes.to_vec(),
    }
}

fn object_id(bytes: &[u8]) -> ObjectId {
    ObjectId(digest::sha256(bytes))
}

fn op(s: &str) -> OpId {
    OpId::parse(s).expect("op id")
}

/// A dummy 20-byte git locator for pure object_presence fence tests (no real git store touched).
fn goid(b: u8) -> [u8; 20] {
    [b; 20]
}

/// A genesis candidate (no parents) with the given files.
fn genesis(files: Vec<UploadedFile>) -> CandidateUpload {
    CandidateUpload {
        files,
        parents: vec![],
        author: "d_test".to_owned(),
        message: "topos publish".to_owned(),
    }
}

/// Stage a committed (accepted-trunk) version with its bytes durably installed + readable — for the
/// read-access tests. Migrates a genesis bundle (the objects become `present`), releases the migrate lease,
/// and records the commit's provenance + reachability (`skill_commit` + `commit_object`) — exactly the
/// readable, GC-rooted state a promoted version has, without driving a full pointer-move. Returns the
/// recomputed commit id.
async fn stage_committed(
    a: &Authority,

    w: &WorkspaceId,

    s: &SkillId,

    op_id: &str,

    files: Vec<UploadedFile>,
) -> CommitId {
    let staged = ingest_migrate(a, w, op_id, files, 100).await;
    a.db().release_lease(w, &op(op_id)).await.unwrap();
    let objects = lifecycle::distinct_object_ids(&staged.entries);
    a.db()
        .seed_commit(w, s, staged.version_id, &objects)
        .await
        .unwrap();
    staged.version_id
}

// ── the access rule: a rostered reader gets the bytes of a version ─────────────────────────────────

// ── isolation: cross-workspace + cross-skill negatives (release blockers) ──────────────────────────

// ── ingest guards: the no-empty-bundle policy + the canonical rules (the rehash/dedup/cross-skill +
// roster paths are now exercised through publish/propose, below) ────────────────────────────────────

// ── transaction discipline ────────────────────────────────────────────────────────────────────────

// ── object_presence fenced transitions (the CAS state machine, in isolation) ───────────────────────

// ── ingest → migrate → GC, end-to-end (the fence through the real ops) ──────────────────────────────

/// Ingest a genesis candidate then migrate it fully; returns the staged candidate.
async fn ingest_migrate(
    a: &Authority,

    w: &WorkspaceId,

    op_id: &str,

    files: Vec<UploadedFile>,

    now: i64,
) -> lifecycle::StagedCandidate {
    let staged = lifecycle::ingest(a, w, &op(op_id), genesis(files), now)
        .await
        .expect("ingest");
    lifecycle::migrate(a, w, &staged, now)
        .await
        .expect("migrate");
    staged
}

// ── cross-workspace isolation (release blockers) ───────────────────────────────────────────────────

// ── review hardening: recovery keep-set re-check · deleting-wait · janitor reuse-guard ───────────────

// ===== The size-routed large-object store (offload) — the release-blocker criteria =====

use topos_gitstore::LargeObjectStore as _;

/// A deterministic blob of `n` bytes filled with `seed` (distinct seeds → distinct content + object ids).
fn blob(n: usize, seed: u8) -> Vec<u8> {
    vec![seed; n]
}

// ── the contribute authority's gated proposal root (the GC + read proposal arm) ──────────────────────
//
// `publish --propose` roots a candidate's bytes through `proposal_object`, gated — for BOTH retention and
// read — on ONE derived predicate `open ∧ base == current`. These tests pin that the read arm and the two
// GC-claim arms evaluate it IDENTICALLY (the anti-drift guard), and that keep-set == read-authorization holds
// across the eventless stale transition: a reclaimed object reads 404, never an Integrity alarm. The
// propose/approve/reject write paths that PRODUCE these rows are exercised end-to-end further below; here the
// rows are seeded, so the gate itself is tested in isolation.

/// Migrate one object's bytes into the main store, then release its lease — so the ONLY thing that can root
/// it is a (seeded) proposal, isolating the proposal arm. Returns the migrated version's commit id.
async fn migrate_unrooted(
    a: &Authority,

    w: &WorkspaceId,

    op_id: &str,

    path: &str,

    bytes: &[u8],
) -> CommitId {
    let staged = ingest_migrate(a, w, op_id, vec![file(path, bytes)], 100).await;
    a.db().release_lease(w, &op(op_id)).await.unwrap();
    staged.version_id
}

const PROP_OP_1: &str = "a1111111-1111-4111-8111-111111111111";

// ── the pointer-move write (`set-current`): genesis · publish · revert · the gate · interleavings ──────
//
// These drive the WHOLE backbone in-process against a real Postgres + git store: ingest → migrate → the one
// serializable pointer-move transaction. A test acts as the client device — it signs the device-op over the
// SERVER-rehashed candidate values (exactly what the txn reconstructs), so a valid signature is the binding.

use ed25519_dalek::Signer as _;

use topos_core::sign::{
    CurrentPointer, DeviceOp, DeviceOpFields, device_op_preimage, verify_pointer,
};

use topos_types::{Generation, SignedCurrentRecord, TerminalOutcome};

use crate::DeviceSignedOp;

const NOW: i64 = 1_000_000;

const CREATED_AT: &str = "2026-06-28T00:00:00Z";

fn gn(epoch: u64, seq: u64) -> Generation {
    Generation { epoch, seq }
}

/// A deterministic device signing key (the test's "client device"); its public key is seeded into the
/// device registry, so the in-transaction authorization verifies the device-op against it.
fn dev_key(seed: u8) -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[seed; 32])
}

/// Register a device + roster its principal so the pointer-move's in-transaction authorization passes.
async fn register(
    fx: &Fixture,

    ws: &WorkspaceId,

    skill: &SkillId,

    dkid: &str,

    key: &ed25519_dalek::SigningKey,

    principal: &str,
) {
    let p = prin(principal);
    fx.authority
        .db()
        .seed_device(ws, dkid, &key.verifying_key().to_bytes(), &p, false)
        .await
        .unwrap();
    fx.authority.db().seed_roster(ws, skill, &p).await.unwrap();
}

/// Sign a device-op over the SERVER-trusted candidate identity (commit id + bundle digest + scope) — the
/// same fields the transaction rebuilds, so an honest device's signature verifies there.
#[allow(clippy::too_many_arguments)]
fn sign_op(
    key: &ed25519_dalek::SigningKey,

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    op_kind: DeviceOp,

    op_id: &OpId,

    expected: Generation,

    commit: [u8; 32],

    digest: [u8; 32],
) -> [u8; 64] {
    let op_id_bytes = uuid::Uuid::parse_str(op_id.as_str()).unwrap().into_bytes();
    let fields = DeviceOpFields {
        workspace_id: ws.as_str(),
        skill_id: skill.as_str(),
        op: op_kind,
        op_id: op_id_bytes,
        device_key_id: dkid,
        expected_epoch: expected.epoch,
        expected_seq: expected.seq,
        commit_id: commit,
        bundle_digest: digest,
    };
    key.sign(&device_op_preimage(&fields).unwrap()).to_bytes()
}

/// Ingest + sign-over-the-staged-values + migrate, returning the staged candidate + the signed device op —
/// so a test can drive the pointer-move itself (and inject a revoke/GC between migrate and the txn).
#[allow(clippy::too_many_arguments)]
async fn prepare(
    fx: &Fixture,

    key: &ed25519_dalek::SigningKey,

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    op_kind: DeviceOp,

    op_id_str: &str,

    candidate: CandidateUpload,

    expected: Generation,
) -> (lifecycle::StagedCandidate, DeviceSignedOp) {
    let op_id = op(op_id_str);
    let staged = lifecycle::ingest(&fx.authority, ws, &op_id, candidate, NOW)
        .await
        .unwrap();
    let signature = sign_op(
        key,
        dkid,
        ws,
        skill,
        op_kind,
        &op_id,
        expected,
        staged.version_id.0,
        staged.bundle_digest,
    );
    lifecycle::migrate(&fx.authority, ws, &staged, NOW)
        .await
        .unwrap();
    (
        staged,
        DeviceSignedOp {
            device_key_id: dkid.to_owned(),
            op: op_kind,
            signature,
            expected,
        },
    )
}

/// The common case: prepare + drive the pointer-move, returning the receipt.
#[allow(clippy::too_many_arguments)]
async fn publish(
    fx: &Fixture,

    key: &ed25519_dalek::SigningKey,

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    op_id_str: &str,

    candidate: CandidateUpload,

    expected: Generation,
) -> crate::SetCurrentReceipt {
    let (staged, device) = prepare(
        fx,
        key,
        dkid,
        ws,
        skill,
        DeviceOp::PublishDirect,
        op_id_str,
        candidate,
        expected,
    )
    .await;
    crate::set_current::publish(
        &fx.authority,
        ws,
        skill,
        &staged,
        &device,
        None,
        CREATED_AT,
        NOW,
    )
    .await
    .unwrap()
}

/// A normal 1-parent publish candidate.
fn child(parent: CommitId, files: Vec<UploadedFile>) -> CandidateUpload {
    CandidateUpload {
        files,
        parents: vec![parent],
        author: "d_test".to_owned(),
        message: "topos publish".to_owned(),
    }
}

fn hex32(s: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap();
    }
    out
}

fn b64_sig(s: &str) -> [u8; 64] {
    use base64::Engine as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s)
        .unwrap()
        .try_into()
        .unwrap()
}

/// Reconstruct the `CurrentPointer` STRICTLY from {scope, record} of a stored signed record and verify it
/// (key_id / schema_version are NOT part of the signed bytes). `scope` lets a test force a wrong scope.
fn verify_record(bytes: &[u8], ws: &str, skill: &str, pubkey: &[u8; 32]) -> bool {
    let rec: SignedCurrentRecord = serde_json::from_slice(bytes).unwrap();
    let ptr = CurrentPointer {
        workspace_id: ws,
        skill_id: skill,
        version_id: hex32(&rec.record.version_id),
        epoch: rec.record.generation.epoch,
        seq: rec.record.generation.seq,
    };
    verify_pointer(&ptr, &b64_sig(&rec.signature.value), pubkey)
}

/// Sign a revert device-op: the server constructs the forward commit, so the test signs over the SAME values
/// the txn will (good's digest determines the forward version_id).
#[allow(clippy::too_many_arguments)]
async fn sign_revert(
    fx: &Fixture,

    key: &ed25519_dalek::SigningKey,

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    good: CommitId,

    op_id: &OpId,

    expected: Generation,
) -> [u8; 64] {
    use topos_core::sign::{self, Commit};
    let good_digest = fx
        .authority
        .db()
        .skill_commit_bundle_digest(ws, skill, good)
        .await
        .unwrap()
        .unwrap();
    let current = fx
        .authority
        .db()
        .read_current_commit(ws, skill)
        .await
        .unwrap()
        .unwrap();
    let version_id = sign::commit_id(&Commit {
        parents: &[current.0],
        tree: good_digest,
        author: "d_test",
        message: "topos revert",
    })
    .unwrap();
    sign_op(
        key,
        dkid,
        ws,
        skill,
        DeviceOp::Revert,
        op_id,
        expected,
        version_id,
        good_digest,
    )
}

// ── the contribute authority end-to-end: publish --propose · review --approve|--reject (the write paths) ──
//
// These drive the REAL propose/approve/reject through `Authority` (and the shared `set_current::propose`)
// against a live Postgres + git store — the write paths that PRODUCE the proposal/approval rows the gated GC +
// read arms above consume. A test acts as the client device, signing each device-op over the SERVER-rehashed
// candidate values exactly as the transaction reconstructs them.

/// The commit `current` points at for a skill (the parent for the next candidate).
async fn current_commit(fx: &Fixture, w: &WorkspaceId, s: &SkillId) -> CommitId {
    fx.authority
        .db()
        .read_current_commit(w, s)
        .await
        .unwrap()
        .expect("a current pointer")
}

/// Ingest + migrate + sign(PublishPropose) + open the proposal; returns (receipt, candidate commit, digest).
#[allow(clippy::too_many_arguments)]
async fn do_propose(
    fx: &Fixture,

    key: &ed25519_dalek::SigningKey,

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    op_id_str: &str,

    candidate: CandidateUpload,

    expected: Generation,
) -> (crate::SetCurrentReceipt, CommitId, [u8; 32]) {
    let (staged, device) = prepare(
        fx,
        key,
        dkid,
        ws,
        skill,
        DeviceOp::PublishPropose,
        op_id_str,
        candidate,
        expected,
    )
    .await;
    let r = crate::set_current::propose(
        &fx.authority,
        ws,
        skill,
        &staged,
        &device,
        None,
        CREATED_AT,
        NOW,
    )
    .await
    .unwrap();
    (r, staged.version_id, staged.bundle_digest)
}

/// Sign a `review --approve` over the proposal's (commit, digest, base) and run it through the public API.
#[allow(clippy::too_many_arguments)]
async fn do_approve(
    fx: &Fixture,

    key: &ed25519_dalek::SigningKey,

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    op_id_str: &str,

    commit: CommitId,

    digest: [u8; 32],

    base: Generation,
) -> crate::SetCurrentReceipt {
    let op_id = op(op_id_str);
    let sig = sign_op(
        key,
        dkid,
        ws,
        skill,
        DeviceOp::ReviewApprove,
        &op_id,
        base,
        commit.0,
        digest,
    );
    let device = DeviceSignedOp {
        device_key_id: dkid.to_owned(),
        op: DeviceOp::ReviewApprove,
        signature: sig,
        expected: base,
    };
    fx.authority
        .review_approve(ws, skill, commit, device, &op_id, CREATED_AT, NOW)
        .await
        .unwrap()
}

/// Sign a `review --reject` over the proposal's (commit, digest, base) and run it through the public API.
#[allow(clippy::too_many_arguments)]
async fn do_reject(
    fx: &Fixture,

    key: &ed25519_dalek::SigningKey,

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    op_id_str: &str,

    commit: CommitId,

    digest: [u8; 32],

    base: Generation,
) -> crate::SetCurrentReceipt {
    let op_id = op(op_id_str);
    let sig = sign_op(
        key,
        dkid,
        ws,
        skill,
        DeviceOp::ReviewReject,
        &op_id,
        base,
        commit.0,
        digest,
    );
    let device = DeviceSignedOp {
        device_key_id: dkid.to_owned(),
        op: DeviceOp::ReviewReject,
        signature: sig,
        expected: base,
    };
    fx.authority
        .review_reject(ws, skill, commit, device, &op_id, CREATED_AT)
        .await
        .unwrap()
}

// ===== The authenticated read surface (read-token resolver + the bound reads) =====

// ── the proposals-listing read (`list_open_proposals`) — keep == read == LIST ─────────────────────────
//
// The thin, low-disclosure proposals listing reuses the SAME `open ∧ base == current` predicate the object
// and version reads use (the 5th verbatim copy), so a staled proposal vanishes from the list exactly as it
// drops out of read + retention. The roster JOIN is the authorization (non-rostered ⇒ empty, never a probe);
// the scope/path assert is the cross-skill/workspace leak guard (a mismatch ⇒ the indistinguishable 404).

// ── the split suites (most `use super::*;` for the shared fixtures/helpers above;
//    canonical_migration is self-contained — it probes raw migration SQL, not the Authority) ──
mod canonical_migration;
mod contribute;
mod display_name;
mod enrollment_governance;
mod ingest;
mod large_object;
mod lifecycle_fence;
mod lifecycle_gc;
mod proposals_root;
mod read_access;
mod read_surface;
mod receipts_migration;
mod restore;
mod session_read;
mod session_review;
mod session_roster;
mod set_current;
mod standup;
