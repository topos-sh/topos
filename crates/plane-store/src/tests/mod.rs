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
            // An enrollment config (load-or-generate the 0600 HMAC secret) — the enrollment/governance
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
// across the eventless stale transition: a reclaimed object reads 404, never an Integrity fault. The
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
// serializable pointer-move transaction. The device request presents only its `device_key_id`; the txn
// authenticates it by registry-row lookup (the non-revoked row bound to a rostered principal) — no signed
// frame, no possession proof.

use topos_types::{Generation, TerminalOutcome, WireCurrentRecord};

use crate::set_current::DeviceOpRequest;
use crate::{DeviceOp, DeviceOpAuth};

const NOW: i64 = 1_000_000;

const CREATED_AT: &str = "2026-06-28T00:00:00Z";

fn gn(epoch: u64, seq: u64) -> Generation {
    Generation { epoch, seq }
}

/// The test workspace credential a device is seeded with — derived from its `(workspace, device_key_id)`
/// so the seed (`seed_device`) and the presented request (`DeviceOpAuth`/`DeviceOpRequest`) stay in
/// lock-step. The workspace-credential model authenticates every device-lane op (reads/writes/governance)
/// by one bearer secret per (workspace × device); a `device_registry` GLOBAL-unique index on the stored
/// `credential_sha256` means the plaintext must be unique per device across the whole DB, so the
/// derivation binds BOTH the workspace and the device key id (a distinct device ⇒ a distinct credential).
fn cred(ws: &WorkspaceId, dkid: &str) -> String {
    cred_str(ws.as_str(), dkid)
}

/// [`cred`] over a raw workspace-id string — for call sites (governance) that hold the ws as a `&str`.
fn cred_str(ws: &str, dkid: &str) -> String {
    format!("cred_{ws}_{dkid}")
}

/// The sha256 of [`cred`] — the stored/compared credential form the internal [`DeviceOpRequest`] carries.
fn cred_sha(ws: &WorkspaceId, dkid: &str) -> [u8; 32] {
    digest::sha256(cred(ws, dkid).as_bytes())
}

/// A deterministic device PUBLIC key (the test's "client device") seeded into the registry. Nothing
/// verifies it — the credential a write presents is the `device_key_id` string, authenticated by a
/// registry-row lookup — so any fixed 32 bytes stand in for a real key.
fn dev_key(seed: u8) -> [u8; 32] {
    [seed; 32]
}

/// Register a device with its workspace credential + seat its principal as a confirmed member (the write
/// gate) and roster it on the skill (follow-state) so the pointer-move's in-transaction authorization
/// passes. The credential is [`cred`]`(dkid)`, so a matching request built through [`prepare`] /
/// [`revert_request`] / [`do_approve`] authenticates.
async fn register(
    fx: &Fixture,

    ws: &WorkspaceId,

    skill: &SkillId,

    dkid: &str,

    key: &[u8; 32],

    principal: &str,
) {
    let p = prin(principal);
    fx.authority
        .db()
        .seed_device(ws, dkid, key, &p, false, &cred(ws, dkid))
        .await
        .unwrap();
    // Every device write gates on a CONFIRMED workspace member (any role); seat one. The per-skill
    // `roster` table is gone (increment 3 lifted follow-state into person-scoped `skill_follows`);
    // membership alone authorizes reads AND writes, so no per-skill seeding is needed here.
    fx.authority
        .db()
        .seed_workspace_member(ws, &p, "member", "confirmed")
        .await
        .unwrap();
    let _ = skill; // the skill is still named by callers (their intent), but no longer seeded per-skill
}

/// Ingest + migrate, returning the staged candidate + the device request — so a test can drive the
/// pointer-move itself (and inject a revoke/GC between migrate and the txn). The request carries only the
/// presented `device_key_id`; the txn authenticates it by registry lookup (`_key`/`_skill` are unused now
/// that nothing signs, kept so call sites stay stable).
#[allow(clippy::too_many_arguments)]
async fn prepare(
    fx: &Fixture,

    _key: &[u8; 32],

    dkid: &str,

    ws: &WorkspaceId,

    _skill: &SkillId,

    op_kind: DeviceOp,

    op_id_str: &str,

    candidate: CandidateUpload,

    expected: Generation,
) -> (lifecycle::StagedCandidate, DeviceOpRequest) {
    let op_id = op(op_id_str);
    let staged = lifecycle::ingest(&fx.authority, ws, &op_id, candidate, NOW)
        .await
        .unwrap();
    lifecycle::migrate(&fx.authority, ws, &staged, NOW)
        .await
        .unwrap();
    (
        staged,
        DeviceOpRequest {
            credential_sha256: cred_sha(ws, dkid),
            device_key_id: dkid.to_owned(),
            op: op_kind,
            expected,
        },
    )
}

/// The common case: prepare + drive the pointer-move, returning the receipt.
#[allow(clippy::too_many_arguments)]
async fn publish(
    fx: &Fixture,

    key: &[u8; 32],

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

/// Parse a stored `current.record` — the UNSIGNED [`WireCurrentRecord`] document `read_current_record`
/// returns and a follower's pointer fetch serves (unsigned now; authority is the row behind the pointer,
/// integrity is the content-addressed `version_id` re-verified byte-for-byte on apply).
fn wire_record(bytes: &[u8]) -> WireCurrentRecord {
    serde_json::from_slice(bytes).expect("a stored current record parses as WireCurrentRecord")
}

/// A revert device auth — the server constructs the forward commit; the request presents only the
/// workspace credential + the CAS target generation (nothing signs the forward version id any more). The
/// public [`Authority::revert`](crate::Authority::revert) takes the presented [`DeviceOpAuth`]. `ws` binds
/// the credential to the seeded device (credentials are `(ws, dkid)`-scoped — see [`cred`]).
fn revert_request(ws: &WorkspaceId, dkid: &str, expected: Generation) -> DeviceOpAuth {
    DeviceOpAuth {
        credential: cred(ws, dkid),
        op: DeviceOp::Revert,
        expected,
    }
}

// ── the contribute authority end-to-end: publish --propose · review --approve|--reject (the write paths) ──
//
// These drive the REAL propose/approve/reject through `Authority` (and the shared `set_current::propose`)
// against a live Postgres + git store — the write paths that PRODUCE the proposal/approval rows the gated GC +
// read arms above consume. A test acts as the client device, presenting its `device_key_id`; the transaction
// authenticates it by registry-row lookup exactly as production does.

/// The commit `current` points at for a skill (the parent for the next candidate).
async fn current_commit(fx: &Fixture, w: &WorkspaceId, s: &SkillId) -> CommitId {
    fx.authority
        .db()
        .read_current_commit(w, s)
        .await
        .unwrap()
        .expect("a current pointer")
}

/// Ingest + migrate + open the proposal (`PublishPropose`); returns (receipt, candidate commit, digest).
#[allow(clippy::too_many_arguments)]
async fn do_propose(
    fx: &Fixture,

    key: &[u8; 32],

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
        None,
        CREATED_AT,
        NOW,
    )
    .await
    .unwrap();
    (r, staged.version_id, staged.bundle_digest)
}

/// Run a `review --approve` on the proposal's (commit, base) through the public API. `_key`/`digest` are
/// vestigial now that nothing signs — kept so the call sites stay stable.
#[allow(clippy::too_many_arguments)]
async fn do_approve(
    fx: &Fixture,

    _key: &[u8; 32],

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    op_id_str: &str,

    commit: CommitId,

    _digest: [u8; 32],

    base: Generation,
) -> crate::SetCurrentReceipt {
    let op_id = op(op_id_str);
    let auth = DeviceOpAuth {
        credential: cred(ws, dkid),
        op: DeviceOp::ReviewApprove,
        expected: base,
    };
    fx.authority
        .review_approve(ws, skill, commit, auth, &op_id, CREATED_AT, NOW)
        .await
        .unwrap()
}

/// Run a `review --reject` on the proposal's (commit, base) through the public API. `_key`/`digest` are
/// vestigial now that nothing signs — kept so the call sites stay stable.
#[allow(clippy::too_many_arguments)]
async fn do_reject(
    fx: &Fixture,

    _key: &[u8; 32],

    dkid: &str,

    ws: &WorkspaceId,

    skill: &SkillId,

    op_id_str: &str,

    commit: CommitId,

    _digest: [u8; 32],

    base: Generation,
) -> crate::SetCurrentReceipt {
    let op_id = op(op_id_str);
    let auth = DeviceOpAuth {
        credential: cred(ws, dkid),
        op: DeviceOp::ReviewReject,
        expected: base,
    };
    fx.authority
        .review_reject(ws, skill, commit, auth, &op_id, CREATED_AT)
        .await
        .unwrap()
}

/// Seed a device with its workspace credential + seat its principal as a confirmed member, then resolve
/// the device READ lane's [`crate::ReadScope`] on `skill`. This is the workspace-credential read path:
/// one bearer credential authenticates the device, and a CONFIRMED `workspace_member` row is the read
/// gate (the per-skill roster no longer scopes reads — a member reads any skill in the workspace).
async fn member_read_scope(
    a: &Authority,

    w: &WorkspaceId,

    s: &SkillId,

    dkid: &str,

    principal: &str,
) -> crate::ReadScope {
    let p = prin(principal);
    a.db()
        .seed_device(w, dkid, &dev_key(7), &p, false, &cred(w, dkid))
        .await
        .unwrap();
    a.db()
        .seed_workspace_member(w, &p, "member", "confirmed")
        .await
        .unwrap();
    a.resolve_read_scope(w.as_str(), s.as_str(), &cred(w, dkid))
        .await
        .unwrap()
}

// ===== The authenticated read surface (credential resolver + the bound reads) =====

// ── the proposals-listing read (`list_open_proposals`) — keep == read == LIST ─────────────────────────
//
// The thin, low-disclosure proposals listing reuses the SAME `open ∧ base == current` predicate the object
// and version reads use (the 5th verbatim copy), so a staled proposal vanishes from the list exactly as it
// drops out of read + retention. The roster JOIN is the authorization (non-rostered ⇒ empty, never a probe);
// the scope/path assert is the cross-skill/workspace leak guard (a mismatch ⇒ the indistinguishable 404).

// ── the split suites (most `use super::*;` for the shared fixtures/helpers above;
//    canonical_migration is self-contained — it probes raw migration SQL, not the Authority) ──
mod canonical_migration;
mod channels_delivery;
mod channels_lifecycle;
mod channels_migration;
mod channels_protect;
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
