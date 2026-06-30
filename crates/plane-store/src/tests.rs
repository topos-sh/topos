//! In-crate authority tests (the `pub(crate)` seed helper is only visible here, never to an external
//! integration crate). They exercise the access rule, cross-workspace and cross-skill isolation, the
//! upload/rehash guard, dedup-obliviousness, and the transaction discipline against a real SQLite
//! database + a real per-workspace git store.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest;

use crate::sqlite::{ClaimOutcome, InstallOutcome, Location, ObjectStatus};
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
    async fn new(tag: &str) -> Self {
        Self::build(tag, None).await
    }

    /// A fixture with an overridden size-routing threshold + reject cap — for the offload tests, which
    /// force placement (a tiny threshold routes ordinary test bytes to the large store) and exercise the
    /// reject cap with small payloads.
    async fn with_large_limits(tag: &str, threshold: u64, reject_cap: u64) -> Self {
        Self::build(tag, Some((threshold, reject_cap))).await
    }

    async fn build(tag: &str, limits: Option<(u64, u64)>) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-ps-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let mut authority = Authority::open_sqlite(
            &dir.join("plane.db"),
            &dir.join("stores"),
            &dir.join("large"),
        )
        .await
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
            deployment_mode: DeploymentMode::Cloud,
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

#[tokio::test]
async fn a_rostered_member_reads_the_bytes_of_a_version() {
    let fx = Fixture::new("read-ok").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let reader = prin("dev_read");
    a.db().seed_roster(&w, &s, &reader).await.unwrap();

    let body = b"# PR describe\nrun the thing\n";
    let script = b"#!/bin/sh\necho hi\n";
    stage_committed(
        a,
        &w,
        &s,
        "read-ok",
        vec![file("SKILL.md", body), file("run.sh", script)],
    )
    .await;

    // A rostered member reads each of the version's objects (the read path resolves via the access join).
    assert_eq!(
        a.read_object(&reader, &w, &s, object_id(body))
            .await
            .unwrap(),
        body
    );
    assert_eq!(
        a.read_object(&reader, &w, &s, object_id(script))
            .await
            .unwrap(),
        script
    );
}

#[tokio::test]
async fn unrostered_reader_gets_notfound_for_a_real_object() {
    let fx = Fixture::new("read-unrostered").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let uploader = prin("dev_up");
    a.db().seed_roster(&w, &s, &uploader).await.unwrap();
    let body = b"secret bytes";
    stage_committed(a, &w, &s, "unrostered", vec![file("SKILL.md", body)]).await;

    // A principal with no roster row gets the uniform not-found (never the bytes, never a 403).
    let outsider = prin("dev_outsider");
    assert!(matches!(
        a.read_object(&outsider, &w, &s, object_id(body)).await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn revocation_by_roster_deletion_stops_reads() {
    let fx = Fixture::new("revoke").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_x");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    let body = b"body";
    stage_committed(a, &w, &s, "revoke", vec![file("SKILL.md", body)]).await;
    assert_eq!(
        a.read_object(&p, &w, &s, object_id(body)).await.unwrap(),
        body
    );

    // Membership = a row exists; deleting it (the revocation mechanism) stops the read immediately.
    a.db().delete_roster(&w, &s, &p).await.unwrap();
    assert!(matches!(
        a.read_object(&p, &w, &s, object_id(body)).await,
        Err(AuthorityError::NotFound)
    ));
}

// ── isolation: cross-workspace + cross-skill negatives (release blockers) ──────────────────────────

#[tokio::test]
async fn cross_workspace_object_is_unreadable_under_another_scope() {
    let fx = Fixture::new("xws").await;
    let a = &fx.authority;
    let (wa, wb, s) = (ws("w_a"), ws("w_b"), skill("s_pr"));
    let p = prin("dev_p");

    // Stage a real object into workspace B.
    a.db().seed_roster(&wb, &s, &p).await.unwrap();
    let secret = b"workspace B private bytes";
    stage_committed(a, &wb, &s, "xws", vec![file("SKILL.md", secret)]).await;

    // The same principal, rostered for the same skill id in workspace A, cannot read B's object by
    // supplying B's object id under A's scope — the workspace_id binding makes it a uniform not-found.
    a.db().seed_roster(&wa, &s, &p).await.unwrap();
    assert!(matches!(
        a.read_object(&p, &wa, &s, object_id(secret)).await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn cross_skill_object_is_unreadable_and_indistinguishable_from_absent() {
    let fx = Fixture::new("xskill").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (x, y) = (skill("s_x"), skill("s_y"));
    let p = prin("dev_p");

    // An object reachable only via skill Y, in the shared per-workspace store.
    a.db().seed_roster(&w, &y, &p).await.unwrap();
    let y_bytes = b"skill Y only";
    stage_committed(a, &w, &y, "xskill", vec![file("SKILL.md", y_bytes)]).await;

    // A reader rostered for X (not Y) gets not-found for Y's object — byte-for-byte identical to asking
    // for an object that exists in no skill at all.
    a.db().seed_roster(&w, &x, &p).await.unwrap();
    let cross = a.read_object(&p, &w, &x, object_id(y_bytes)).await;
    let absent = a
        .read_object(&p, &w, &x, object_id(b"never uploaded"))
        .await;
    assert!(matches!(cross, Err(AuthorityError::NotFound)));
    assert!(matches!(absent, Err(AuthorityError::NotFound)));
}

/// Exercise the access join + the pointer table directly from staged rows (no upload), isolating the
/// authorization logic. The witness resolves only on the full rostered ∧ reachable match; every
/// mismatch — wrong principal, skill, workspace, or object — collapses to no witness.
#[tokio::test]
async fn seeded_access_join_resolves_a_witness_and_isolates_every_axis() {
    let fx = Fixture::new("seed-join").await;
    let a = &fx.authority;
    let (w, s, p) = (ws("w_acme"), skill("s_pr"), prin("dev_p"));
    let commit = CommitId([0x33; 32]);
    let obj = ObjectId([0x44; 32]);

    a.db().seed_roster(&w, &s, &p).await.unwrap();
    a.db().seed_commit(&w, &s, commit, &[obj]).await.unwrap();
    a.db().seed_current(&w, &s, commit, 1, 1).await.unwrap(); // exercises the pointer table + its FK

    let read = |w: WorkspaceId, s: SkillId, p: Principal, o: ObjectId| async move {
        a.db().authorize_object_read(&w, &s, &p, o).await.unwrap()
    };
    // Full match → the witness commit.
    assert_eq!(
        read(w.clone(), s.clone(), p.clone(), obj).await,
        Some(commit)
    );
    // Each axis broken in isolation → no witness.
    assert_eq!(
        read(w.clone(), s.clone(), prin("dev_other"), obj).await,
        None
    );
    assert_eq!(
        read(w.clone(), skill("s_other"), p.clone(), obj).await,
        None
    );
    assert_eq!(read(ws("w_other"), s.clone(), p.clone(), obj).await, None);
    assert_eq!(
        read(w.clone(), s.clone(), p.clone(), ObjectId([0x99; 32])).await,
        None
    );
}

/// Drive the cross-skill lineage predicate through the database gather (not just the pure decision):
/// a candidate whose parent is in this skill passes; one whose parent is only in another skill denies.
#[tokio::test]
async fn check_lineage_uses_seeded_provenance() {
    use crate::{CandidateCommit, LineageDecision};
    let fx = Fixture::new("lineage-db").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (x, y) = (skill("s_x"), skill("s_y"));
    let parent_x = CommitId([0x01; 32]);
    let parent_y = CommitId([0x02; 32]);
    a.db().seed_commit(&w, &x, parent_x, &[]).await.unwrap();
    a.db().seed_commit(&w, &y, parent_y, &[]).await.unwrap();

    let good = [CandidateCommit {
        id: CommitId([0x10; 32]),
        parents: vec![parent_x],
    }];
    let graft = [CandidateCommit {
        id: CommitId([0x11; 32]),
        parents: vec![parent_y],
    }];
    assert_eq!(
        a.check_lineage(&w, &x, &good).await.unwrap(),
        LineageDecision::Pass
    );
    assert_eq!(
        a.check_lineage(&w, &x, &graft).await.unwrap(),
        LineageDecision::Deny
    );
}

// ── ingest guards: the no-empty-bundle policy + the canonical rules (the rehash/dedup/cross-skill +
// roster paths are now exercised through publish/propose, below) ────────────────────────────────────

#[tokio::test]
async fn ingest_rejects_an_empty_or_malformed_bundle() {
    let fx = Fixture::new("ingest-reject").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    // Empty: the authority rejects a zero-file bundle itself (the git store would happily snapshot a
    // zero-entry tree; the client scanner cannot be trusted to have enforced this).
    assert!(matches!(
        lifecycle::ingest(a, &w, &op("empty"), genesis(vec![]), 100).await,
        Err(AuthorityError::RejectedUpload(_))
    ));
    // A forbidden path: the canonical reject rules fire ONCE, inside the kernel during staging.
    assert!(matches!(
        lifecycle::ingest(
            a,
            &w,
            &op("badpath"),
            genesis(vec![file("/abs/forbidden", b"x")]),
            100,
        )
        .await,
        Err(AuthorityError::RejectedUpload(_))
    ));
}

// ── transaction discipline ────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn foreign_key_is_enforced_a_dangling_pointer_insert_is_rejected() {
    let fx = Fixture::new("fk").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    // Seeding `current` for a commit with no provenance violates the foreign key (proving foreign_keys
    // is ON — silently ignored otherwise).
    let res = a
        .db()
        .seed_current(&w, &s, CommitId([0x22; 32]), 1, 1)
        .await;
    assert!(
        res.is_err(),
        "a dangling current insert must be rejected by the FK"
    );
}

// ── object_presence fenced transitions (the CAS state machine, in isolation) ───────────────────────

#[tokio::test]
async fn install_absent_to_present_is_idempotent_reuse() {
    let fx = Fixture::new("t-install").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let o = object_id(b"obj");
    // absent (no row) → present.
    assert_eq!(
        a.db()
            .install_object(&w, o, Location::Git, &goid(7), 3, 100)
            .await
            .unwrap(),
        InstallOutcome::Installed
    );
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Present
    );
    // a second install observes present and reuses (the dedup path) — never a double-install.
    assert_eq!(
        a.db()
            .install_object(&w, o, Location::Git, &goid(7), 3, 101)
            .await
            .unwrap(),
        InstallOutcome::AlreadyPresent
    );
}

#[tokio::test]
async fn claim_unreferenced_present_then_finalize_to_absent() {
    let fx = Fixture::new("t-claim").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let o = object_id(b"lonely");
    a.db()
        .install_object(&w, o, Location::Git, &goid(9), 1, 100)
        .await
        .unwrap();
    // No commit_object, no lease → the guarded claim succeeds and yields the git locator.
    match a.db().claim_for_delete(&w, o, 200).await.unwrap() {
        ClaimOutcome::Claimed { git_oid, .. } => assert_eq!(git_oid, goid(9)),
        ClaimOutcome::Spared => panic!("expected claimed"),
    }
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Deleting
    );
    // Finalize is gated on the claim token (the `now` the claim stamped: 200).
    a.db().finalize_delete(&w, o, 200, 300).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[tokio::test]
async fn claim_spares_a_commit_object_referenced_object() {
    let fx = Fixture::new("t-claim-co").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let o = object_id(b"reachable");
    a.db()
        .install_object(&w, o, Location::Git, &goid(1), 1, 100)
        .await
        .unwrap();
    // A commit references it (the read-authorization surface) → GC must spare it.
    a.db()
        .seed_commit(&w, &s, CommitId([0xC1; 32]), &[o])
        .await
        .unwrap();
    assert!(matches!(
        a.db().claim_for_delete(&w, o, 200).await.unwrap(),
        ClaimOutcome::Spared
    ));
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Present
    );
}

#[tokio::test]
async fn claim_spares_a_live_lease_and_reclaims_after_release() {
    let fx = Fixture::new("t-claim-lease").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let o = object_id(b"leased");
    a.db()
        .install_object(&w, o, Location::Git, &goid(2), 1, 100)
        .await
        .unwrap();
    // A live lease (expires in the future) names it → spared.
    a.db()
        .insert_lease(&w, &op("op1"), CommitId([0xA1; 32]), &[o], 9_999)
        .await
        .unwrap();
    assert!(matches!(
        a.db().claim_for_delete(&w, o, 200).await.unwrap(),
        ClaimOutcome::Spared
    ));
    // Releasing the lease makes it reclaimable.
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    assert!(matches!(
        a.db().claim_for_delete(&w, o, 200).await.unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
}

#[tokio::test]
async fn expired_lease_does_not_spare_but_committed_lease_always_does() {
    let fx = Fixture::new("t-lease-exp").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (o1, o2) = (object_id(b"exp"), object_id(b"perm"));
    a.db()
        .install_object(&w, o1, Location::Git, &goid(3), 1, 100)
        .await
        .unwrap();
    a.db()
        .install_object(&w, o2, Location::Git, &goid(4), 1, 100)
        .await
        .unwrap();
    // An expired lease (expires_at <= now) does NOT protect.
    a.db()
        .insert_lease(&w, &op("exp"), CommitId([0xE1; 32]), &[o1], 150)
        .await
        .unwrap();
    assert!(matches!(
        a.db().claim_for_delete(&w, o1, 200).await.unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
    // A committed (non-expiring) lease protects even far in the future. commit_lease is a CAS on the
    // commit id + lease liveness, so it must run while the lease is still live (now=100 < expires 150).
    let perm_commit = CommitId([0xE2; 32]);
    a.db()
        .insert_lease(&w, &op("perm"), perm_commit, &[o2], 150)
        .await
        .unwrap();
    a.db()
        .commit_lease(&w, &op("perm"), perm_commit, 100)
        .await
        .unwrap();
    assert!(matches!(
        a.db().claim_for_delete(&w, o2, 1_000_000).await.unwrap(),
        ClaimOutcome::Spared
    ));
}

#[tokio::test]
async fn deleting_is_non_resurrectable() {
    let fx = Fixture::new("t-noresurrect").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let o = object_id(b"dying");
    // A row already in `deleting` (a GC mid-unlink): a migrate's install must NOT bring it back to present.
    a.db()
        .seed_deleting_object(&w, o, &goid(5), 50)
        .await
        .unwrap();
    assert_eq!(
        a.db()
            .install_object(&w, o, Location::Git, &goid(5), 1, 200)
            .await
            .unwrap(),
        InstallOutcome::Deleting
    );
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Deleting
    );
    // And a claim on a `deleting` row is a no-op spare (the WHERE status='present' cannot fire).
    assert!(matches!(
        a.db().claim_for_delete(&w, o, 300).await.unwrap(),
        ClaimOutcome::Spared
    ));
}

#[tokio::test]
async fn tombstoned_blob_is_rejected_and_existing_row_goes_unavailable() {
    let fx = Fixture::new("t-tomb").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (fresh, existing) = (object_id(b"deny-fresh"), object_id(b"deny-existing"));
    // A denylisted blob with no row: install is refused, no present row is created.
    a.db()
        .insert_tombstone(&w, fresh, "leaked", 100)
        .await
        .unwrap();
    assert_eq!(
        a.db()
            .install_object(&w, fresh, Location::Git, &goid(6), 1, 110)
            .await
            .unwrap(),
        InstallOutcome::Unavailable
    );
    assert_eq!(
        a.db().object_status(&w, fresh).await.unwrap(),
        ObjectStatus::Absent
    );
    // A present object that is then tombstoned reaches the terminal `unavailable` state.
    a.db()
        .install_object(&w, existing, Location::Git, &goid(6), 1, 100)
        .await
        .unwrap();
    a.db()
        .insert_tombstone(&w, existing, "leaked", 120)
        .await
        .unwrap();
    assert_eq!(
        a.db().object_status(&w, existing).await.unwrap(),
        ObjectStatus::Unavailable
    );
    assert!(a.db().is_tombstoned(&w, existing).await.unwrap());
}

#[tokio::test]
async fn recovery_sweep_finalizes_only_stale_deleting() {
    let fx = Fixture::new("t-recover").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (old, fresh) = (object_id(b"crashed"), object_id(b"in-flight"));
    a.db()
        .seed_deleting_object(&w, old, &goid(8), 10)
        .await
        .unwrap(); // stamped long ago
    a.db()
        .seed_deleting_object(&w, fresh, &goid(8), 1000)
        .await
        .unwrap(); // a live GC's
    // Only the stale one (status_updated_at < threshold) is in the candidate list.
    let stale = a.db().stale_deleting(&w, 500).await.unwrap();
    assert_eq!(stale, vec![old]);
    let wss = a.db().workspaces_with_stale_deleting(500).await.unwrap();
    assert_eq!(wss, vec![w.clone()]);

    // The recovery CLAIM is one-winner: the first claim wins (and bumps status_updated_at out of stale
    // range), so a concurrent second claim sees nothing to take — closing the double-unlink race.
    let first = a
        .db()
        .claim_stale_for_recovery(&w, old, 500, 600)
        .await
        .unwrap();
    assert_eq!(first, Some((Location::Git, goid(8))));
    let second = a
        .db()
        .claim_stale_for_recovery(&w, old, 500, 600)
        .await
        .unwrap();
    assert_eq!(
        second, None,
        "a second concurrent sweeper must not also claim it"
    );
    // It is still `deleting` (the claim keeps the row deleting across the unlink), not resurrected.
    assert_eq!(
        a.db().object_status(&w, old).await.unwrap(),
        ObjectStatus::Deleting
    );
}

#[tokio::test]
async fn lease_rebuilds_its_object_set_on_op_id_reuse() {
    // op-id reuse with a different candidate must REPLACE the lease's object set, not merge — else a stale
    // object would be pinned non-expiring after commit_lease.
    let fx = Fixture::new("t-lease-reuse").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (x, y) = (object_id(b"first-cand"), object_id(b"second-cand"));
    a.db()
        .install_object(&w, x, Location::Git, &goid(1), 1, 100)
        .await
        .unwrap();
    a.db()
        .install_object(&w, y, Location::Git, &goid(2), 1, 100)
        .await
        .unwrap();
    // First lease names {X}; reusing the same op id then names {Y}.
    a.db()
        .insert_lease(&w, &op("re"), CommitId([0x1; 32]), &[x], 9_999)
        .await
        .unwrap();
    a.db()
        .insert_lease(&w, &op("re"), CommitId([0x2; 32]), &[y], 9_999)
        .await
        .unwrap();
    // X is no longer leased (reclaimable); Y is leased (spared).
    assert!(matches!(
        a.db().claim_for_delete(&w, x, 200).await.unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
    assert!(matches!(
        a.db().claim_for_delete(&w, y, 200).await.unwrap(),
        ClaimOutcome::Spared
    ));
}

#[tokio::test]
async fn committed_lease_is_not_clobbered_by_op_id_reuse() {
    // After a migrate commits its lease (non-expiring root of a good version), reusing the same op id must
    // be a no-op — never rewriting the lease or its object set, which would unroot the version.
    let fx = Fixture::new("t-committed-lease").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (x, y) = (object_id(b"rooted"), object_id(b"other"));
    a.db()
        .install_object(&w, x, Location::Git, &goid(1), 1, 100)
        .await
        .unwrap();
    a.db()
        .install_object(&w, y, Location::Git, &goid(2), 1, 100)
        .await
        .unwrap();
    let c1 = CommitId([0x1; 32]);
    a.db()
        .insert_lease(&w, &op("re"), c1, &[x], 9_999)
        .await
        .unwrap();
    a.db().commit_lease(&w, &op("re"), c1, 100).await.unwrap(); // X is now committed-rooted
    // Reuse the op id with a different candidate {Y}: it must NOT touch the committed lease.
    a.db()
        .insert_lease(&w, &op("re"), CommitId([0x2; 32]), &[y], 9_999)
        .await
        .unwrap();
    // X is still leased (the committed lease survived); Y was never adopted by this op.
    assert!(matches!(
        a.db().claim_for_delete(&w, x, 1_000_000).await.unwrap(),
        ClaimOutcome::Spared
    ));
    assert!(matches!(
        a.db().claim_for_delete(&w, y, 200).await.unwrap(),
        ClaimOutcome::Claimed { .. }
    ));
}

#[tokio::test]
async fn commit_lease_fails_on_a_stale_or_mismatched_lease() {
    let fx = Fixture::new("t-commit-stale").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let c = CommitId([0xC; 32]);
    a.db()
        .insert_lease(&w, &op("o"), c, &[object_id(b"z")], 150)
        .await
        .unwrap();
    // The lease has expired (now > expires_at): commit must fail closed (its objects may have been GC'd).
    assert!(a.db().commit_lease(&w, &op("o"), c, 200).await.is_err());
    // A commit-id mismatch (a stale finish over a reused op) also fails.
    a.db()
        .insert_lease(&w, &op("o2"), c, &[object_id(b"z")], 9_999)
        .await
        .unwrap();
    assert!(
        a.db()
            .commit_lease(&w, &op("o2"), CommitId([0xD; 32]), 100)
            .await
            .is_err()
    );
}

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

#[tokio::test]
async fn migrate_installs_durably_and_committed_lease_protects_from_gc() {
    let fx = Fixture::new("e-migrate").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"# skill\nrun it\n";
    let staged = ingest_migrate(a, &w, "op1", vec![file("SKILL.md", body)], 100).await;

    // The object is present, and its bytes verify from their FINAL path (a real render of the version).
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    let store = a.open_store(&w).unwrap();
    let rendered = store
        .render_verified(staged.version_id.0, staged.bundle_digest)
        .expect("render the migrated version");
    assert_eq!(rendered.files.len(), 1);
    assert_eq!(rendered.files[0].bytes, body);

    // A successful migrate makes its lease non-expiring, so even a far-future GC reclaims nothing.
    assert_eq!(gc::run_gc(a, &w, 1_000_000_000).await.unwrap(), 0);
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    // The quarantine was cleaned up post-commit.
    assert!(!a.workspace_quarantine_dir(&w, &op("op1")).exists());
}

#[tokio::test]
async fn gc_reclaims_an_abandoned_migrated_object_physically() {
    let fx = Fixture::new("e-abandon").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"abandon me";
    let staged = ingest_migrate(a, &w, "op1", vec![file("SKILL.md", body)], 100).await;
    // Abandon the migrate (the deferred pointer-move never took the root): release the committed lease.
    a.db().release_lease(&w, &op("op1")).await.unwrap();

    // No commit_object (migrate records none) + no live lease → GC reclaims it.
    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Absent
    );
    // Physical deletion: a fresh store can no longer render the orphaned version (its blob is gone).
    let fresh = a.open_store(&w).unwrap();
    assert!(
        fresh
            .render_verified(staged.version_id.0, staged.bundle_digest)
            .is_err(),
        "the blob must be physically unlinked"
    );
}

#[tokio::test]
async fn gc_retention_is_exactly_reachability() {
    let fx = Fixture::new("e-retain").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    // Two migrated objects, both abandoned (leases released) so only commit_object can root them.
    let keep = b"keep-me";
    let drop = b"drop-me";
    ingest_migrate(a, &w, "k", vec![file("keep.md", keep)], 100).await;
    ingest_migrate(a, &w, "d", vec![file("drop.md", drop)], 100).await;
    a.db().release_lease(&w, &op("k")).await.unwrap();
    a.db().release_lease(&w, &op("d")).await.unwrap();
    // Root `keep` via a commit_object edge (the read-authorization surface); leave `drop` unrooted.
    a.db()
        .seed_commit(&w, &s, CommitId([0x55; 32]), &[object_id(keep)])
        .await
        .unwrap();

    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1); // exactly `drop`
    assert_eq!(
        a.db().object_status(&w, object_id(keep)).await.unwrap(),
        ObjectStatus::Present
    );
    assert_eq!(
        a.db().object_status(&w, object_id(drop)).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[tokio::test]
async fn dedup_race_lease_protects_the_full_closure_under_a_slow_migrate() {
    // The release-blocker dedup race, exercised through the REAL migrate op (lease step), deterministically.
    let fx = Fixture::new("e-dedup").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let a_bytes = b"shared object A";
    let c_bytes = b"new object C";

    // V1 migrates A, then is abandoned → A is present + on disk but UNROOTED (no commit_object, no lease).
    ingest_migrate(a, &w, "v1", vec![file("a.txt", a_bytes)], 100).await;
    a.db().release_lease(&w, &op("v1")).await.unwrap();

    // V2 reuses A and adds C. Stage + lease the FULL set {A, C}, then — mid-migrate, before C installs —
    // run a GC. The lease must spare A (the dedup-skipped reused object); C is not present yet.
    let v2 = lifecycle::ingest(
        a,
        &w,
        &op("v2"),
        genesis(vec![file("a.txt", a_bytes), file("c.txt", c_bytes)]),
        200,
    )
    .await
    .unwrap();
    lifecycle::migrate_lease(a, &w, &v2, 200).await.unwrap();
    // Fault-injected slow migrate: GC interposes here.
    assert_eq!(
        gc::run_gc(a, &w, 200).await.unwrap(),
        0,
        "GC must reclaim nothing: A is protected by V2's full-closure lease, C is not yet present"
    );
    assert_eq!(
        a.db().object_status(&w, object_id(a_bytes)).await.unwrap(),
        ObjectStatus::Present
    );
    // Finish the migrate: A is reused (dedup), C installs.
    lifecycle::migrate_install(a, &w, &v2, 200).await.unwrap();
    lifecycle::migrate_finish(a, &w, &v2, 200).await.unwrap();
    let store = a.open_store(&w).unwrap();
    let rendered = store
        .render_verified(v2.version_id.0, v2.bundle_digest)
        .expect("render V2");
    let mut got: Vec<&[u8]> = rendered.files.iter().map(|f| f.bytes.as_slice()).collect();
    got.sort();
    let mut want = vec![a_bytes.as_slice(), c_bytes.as_slice()];
    want.sort();
    assert_eq!(
        got, want,
        "both A (reused) and C (installed) render byte-exact"
    );
}

#[tokio::test]
async fn two_concurrent_migrations_of_one_object_do_not_corrupt() {
    let fx = Fixture::new("e-concurrent").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"identical content";
    // Two independent ops staging the SAME bytes, migrated concurrently. The absent→present CAS makes one
    // install and the other reuse; neither corrupts (write_blob is content-addressed/idempotent).
    let sa = lifecycle::ingest(a, &w, &op("a"), genesis(vec![file("x", body)]), 100)
        .await
        .unwrap();
    let sb = lifecycle::ingest(a, &w, &op("b"), genesis(vec![file("x", body)]), 100)
        .await
        .unwrap();
    let (ra, rb) = tokio::join!(
        lifecycle::migrate(a, &w, &sa, 100),
        lifecycle::migrate(a, &w, &sb, 100),
    );
    ra.unwrap();
    rb.unwrap();
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    // Both versions render the identical, verified bytes.
    let store = a.open_store(&w).unwrap();
    for s in [&sa, &sb] {
        let r = store
            .render_verified(s.version_id.0, s.bundle_digest)
            .unwrap();
        assert_eq!(r.files[0].bytes, body);
    }
}

#[tokio::test]
async fn gc_never_touches_an_active_quarantine_but_the_janitor_sweeps_an_expired_one() {
    let fx = Fixture::new("e-quarantine").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    // Ingest WITHOUT migrating → bytes sit in the GC-excluded quarantine, not the main store.
    let staged = lifecycle::ingest(a, &w, &op("q"), genesis(vec![file("Q.md", b"queued")]), 100)
        .await
        .unwrap();
    let qdir = a.workspace_quarantine_dir(&w, &op("q"));
    assert!(qdir.exists(), "the quarantine objdir exists after ingest");

    // A GC pass reclaims nothing staged in the quarantine (it is not in the main store / has no present row).
    gc::run_gc(a, &w, 200).await.unwrap();
    assert!(qdir.exists(), "GC must never touch an active quarantine");
    assert_eq!(
        a.db()
            .object_status(&w, ObjectId(staged.entries[0].object_id))
            .await
            .unwrap(),
        ObjectStatus::Absent
    );

    // The janitor spares an unexpired quarantine and sweeps an expired one (TTL = ingest_now + 3600).
    assert_eq!(gc::quarantine_janitor(a, 200).await.unwrap(), 0);
    assert!(qdir.exists());
    assert_eq!(gc::quarantine_janitor(a, 100 + 3600 + 1).await.unwrap(), 1);
    assert!(
        !qdir.exists(),
        "the janitor sweeps an expired quarantine whole"
    );
}

#[tokio::test]
async fn recovery_sweep_finalizes_a_crashed_unlink_end_to_end() {
    let fx = Fixture::new("e-recovery").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"half-unlinked";
    let staged = ingest_migrate(a, &w, "op", vec![file("X.md", body)], 100).await;
    let oid = object_id(body);
    // Abandon the migrate so the object is genuinely unrooted — only such an object can be GC-claimed, so
    // this is the realistic precondition for a crashed GC (the recovery claim now re-verifies the keep-set,
    // so a still-rooted `deleting` row is spared, not finalized — see
    // `recovery_sweep_spares_a_deleting_object_re_rooted_by_a_commit_edge`).
    a.db().release_lease(&w, &op("op")).await.unwrap();
    // Simulate a GC that claimed the object (present → deleting) long ago, then crashed before finalizing.
    a.db()
        .seed_deleting_object(&w, oid, &staged.entries[0].git_oid, 10)
        .await
        .unwrap();
    assert_eq!(
        a.db().object_status(&w, oid).await.unwrap(),
        ObjectStatus::Deleting
    );
    // The recovery sweep (far past the stale threshold) finalizes it: re-unlink + absent.
    assert_eq!(gc::recovery_sweep(a, 1_000_000).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, oid).await.unwrap(),
        ObjectStatus::Absent
    );
    let fresh = a.open_store(&w).unwrap();
    assert!(
        fresh
            .render_verified(staged.version_id.0, staged.bundle_digest)
            .is_err(),
        "the crashed unlink is completed (bytes gone)"
    );
    // Idempotent: a second sweep finds nothing stale.
    assert_eq!(gc::recovery_sweep(a, 1_000_001).await.unwrap(), 0);
}

#[tokio::test]
async fn ingest_rejects_a_denylisted_blob() {
    let fx = Fixture::new("e-deny").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"a leaked secret";
    a.db()
        .insert_tombstone(&w, object_id(body), "leaked", 100)
        .await
        .unwrap();
    let res = lifecycle::ingest(a, &w, &op("d"), genesis(vec![file("S.md", body)]), 110).await;
    assert!(matches!(res, Err(AuthorityError::RejectedUpload(_))));
    // The denylist check runs BEFORE staging, so the purged bytes are never persisted to disk.
    assert!(
        !a.workspace_quarantine_dir(&w, &op("d")).exists(),
        "a denylisted candidate must not be staged into a quarantine"
    );
}

// ── cross-workspace isolation (release blockers) ───────────────────────────────────────────────────

#[tokio::test]
async fn gc_for_one_workspace_never_touches_another_with_identical_content() {
    let fx = Fixture::new("e-xws-gc").await;
    let a = &fx.authority;
    let (wa, wb) = (ws("w_a"), ws("w_b"));
    let body = b"identical bytes across tenants";
    // The SAME content migrated into BOTH workspaces — two DISTINCT physical objects + presence rows.
    ingest_migrate(a, &wa, "op", vec![file("X.md", body)], 100).await;
    let sb = ingest_migrate(a, &wb, "op", vec![file("X.md", body)], 100).await;
    // Abandon A's (so it is a GC candidate); keep B's rooted (committed lease).
    a.db().release_lease(&wa, &op("op")).await.unwrap();

    // GC for A reclaims A's object; B's identical-content object is untouched (workspace_id-bound).
    assert_eq!(gc::run_gc(a, &wa, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&wa, object_id(body)).await.unwrap(),
        ObjectStatus::Absent
    );
    assert_eq!(
        a.db().object_status(&wb, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    // B's bytes are intact on disk (its version still renders).
    let store_b = a.open_store(&wb).unwrap();
    assert_eq!(
        store_b
            .render_verified(sb.version_id.0, sb.bundle_digest)
            .unwrap()
            .files[0]
            .bytes,
        body
    );
}

#[tokio::test]
async fn quarantines_are_per_workspace_and_the_janitor_is_scoped() {
    let fx = Fixture::new("e-xws-q").await;
    let a = &fx.authority;
    let (wa, wb) = (ws("w_a"), ws("w_b"));
    // Same op id in two workspaces → distinct quarantine dirs.
    lifecycle::ingest(a, &wa, &op("k"), genesis(vec![file("A.md", b"a")]), 100)
        .await
        .unwrap();
    lifecycle::ingest(a, &wb, &op("k"), genesis(vec![file("B.md", b"b")]), 100)
        .await
        .unwrap();
    let qa = a.workspace_quarantine_dir(&wa, &op("k"));
    let qb = a.workspace_quarantine_dir(&wb, &op("k"));
    assert_ne!(qa, qb);
    assert!(qa.exists() && qb.exists());
    // Expire only A's window: the janitor sweeps A's quarantine and spares B's.
    a.db()
        .insert_quarantine(&wa, &op("k"), &qa.to_string_lossy(), 150)
        .await
        .unwrap();
    assert_eq!(gc::quarantine_janitor(a, 200).await.unwrap(), 1);
    assert!(!qa.exists(), "A's expired quarantine swept");
    assert!(qb.exists(), "B's quarantine spared (its TTL is far off)");
}

// ── review hardening: recovery keep-set re-check · deleting-wait · janitor reuse-guard ───────────────

#[tokio::test]
async fn recovery_sweep_spares_a_deleting_object_re_rooted_by_a_commit_edge() {
    // The recovery byte-loss guard for the RECOVERY path, where the keep-set root arrives AFTER the claim. A
    // crashed GC leaves a stale `deleting` row; before recovery runs, a `commit_object` edge over the same
    // object appears (making it read-authorized). recovery_sweep must re-verify the keep-set at delete time
    // and SPARE it, never unlink a now-readable, committed object's bytes. Fails (Integrity on the final read)
    // if `claim_stale_for_recovery` drops its `commit_object` re-check.
    let fx = Fixture::new("e-recover-reroot").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    let body = b"shared content the recovery must not reclaim";

    // (1) Fenced migrate of `body`, then abandon -> present, unrooted (a normal GC candidate). The migrated
    // version's git commit + blob exist on disk.
    let staged = ingest_migrate(a, &w, "op", vec![file("B.md", body)], 100).await;
    a.db().release_lease(&w, &op("op")).await.unwrap();
    let oid = object_id(body);

    // (2) A GC claims it (present -> deleting) then "crashes" before unlink/finalize: the row is `deleting`
    // with an old status_updated_at and the bytes are still on disk.
    assert!(matches!(
        a.db().claim_for_delete(&w, oid, 200).await.unwrap(),
        ClaimOutcome::Claimed { .. }
    ));

    // (3) A `commit_object` edge over the migrated commit now roots the object — it is read-authorized even
    // though its row is `deleting`.
    a.db()
        .seed_commit(&w, &s, staged.version_id, &[oid])
        .await
        .unwrap();
    assert_eq!(a.read_object(&p, &w, &s, oid).await.unwrap(), body);

    // (4) Recovery runs far past the stale threshold. It must SPARE the re-rooted object (recovered == 0),
    // leaving it `deleting` (non-resurrectable) with its bytes intact — the read keeps working.
    assert_eq!(
        gc::recovery_sweep(a, 1_000_000).await.unwrap(),
        0,
        "recovery must not reclaim a now-readable, commit-referenced object"
    );
    assert_eq!(
        a.db().object_status(&w, oid).await.unwrap(),
        ObjectStatus::Deleting
    );
    assert_eq!(
        a.read_object(&p, &w, &s, oid).await.unwrap(),
        body,
        "the committed object's bytes survive recovery"
    );
}

#[tokio::test]
async fn recovery_finalizes_a_leased_deleting_row_to_unblock_a_waiting_migrate() {
    // A migrate that hits a crashed-GC's stale `deleting` row leases its full object set (including this
    // object) BEFORE `install_one` waits for `absent`. Recovery MUST still finalize the stale row to unblock
    // that waiter — a lease over a `deleting` object means "waiting to re-install", not "readable" (only a
    // `commit_object` edge spares; see `recovery_sweep_spares_a_deleting_object_re_rooted_by_a_commit_edge`).
    // Regression: a recovery guard that also checked the lease would strand the migrate until the lease TTL
    // lapsed.
    let fx = Fixture::new("e-recover-leased").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"crashed then re-leased by a waiting migrate";
    // A real object + store (so recovery can open the store and unlink the loose object); then abandon it.
    let staged = ingest_migrate(a, &w, "mig", vec![file("X.md", body)], 100).await;
    let o = object_id(body);
    a.db().release_lease(&w, &op("mig")).await.unwrap();
    // A crashed GC left it stale `deleting`.
    a.db()
        .seed_deleting_object(&w, o, &staged.entries[0].git_oid, 10)
        .await
        .unwrap();
    // A NEW migrate (a retry of the same content) leases the object set and is now waiting in install_one for
    // `absent` — the lease was taken BEFORE the wait.
    a.db()
        .insert_lease(&w, &op("retry"), staged.version_id, &[o], 9_999)
        .await
        .unwrap();
    // Recovery still finalizes it (recovered == 1) so the waiter can re-copy — the lease must not spare it.
    assert_eq!(gc::recovery_sweep(a, 1_000_000).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[tokio::test]
async fn migrate_install_waits_out_deleting_then_recopies() {
    // Exercises install_one's deleting-wait branch (the sole justification for the normal `tokio` time dep):
    // an object mid-GC (`deleting`) is NEVER resurrected by a concurrent migrate — the install waits for
    // `absent`, then re-copies the bytes. A regression that treated `deleting` as a dedup reuse (e.g.
    // `return Ok(())`) would leave the row `deleting` and fail the final `Present` assertion.
    let fx = Fixture::new("e-deleting-wait").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"installed only after the unlink finishes";
    let staged = lifecycle::ingest(a, &w, &op("d"), genesis(vec![file("X.md", body)]), 100)
        .await
        .unwrap();
    let oid = ObjectId(staged.entries[0].object_id);
    // Seed the object as `deleting` (a GC mid-unlink) so the install must wait it out, not reuse it.
    a.db()
        .seed_deleting_object(&w, oid, &staged.entries[0].git_oid, 10)
        .await
        .unwrap();

    // Run the install concurrently with a task that finalizes the unlink (deleting -> absent). `join!` is
    // polled in order, so install_one observes `deleting` first and enters its (transaction-free) wait; the
    // flip then commits, and the install's next poll sees `absent` and re-copies.
    let install = lifecycle::migrate_install(a, &w, &staged, 200);
    let finish_unlink = async {
        // The seeded `deleting` row's token is its status_updated_at (10).
        a.db().finalize_delete(&w, oid, 10, 200).await.unwrap();
    };
    let (install_res, ()) = tokio::join!(install, finish_unlink);
    install_res.expect("install succeeds after the deleting row clears");

    assert_eq!(
        a.db().object_status(&w, oid).await.unwrap(),
        ObjectStatus::Present,
        "the migrate re-copied the bytes only after `absent`, never resurrecting `deleting`"
    );
}

#[tokio::test]
async fn tombstone_does_not_interrupt_an_in_flight_deletion() {
    // `insert_tombstone`'s `WHERE status IN ('present','absent')` deliberately leaves a `deleting` row alone
    // (flipping it to `unavailable` would strand the unlink — `finalize_delete` only fires on `deleting`).
    // The blob is still denylisted, and the in-flight unlink still completes to `absent`.
    let fx = Fixture::new("t-tomb-deleting").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let o = object_id(b"dying-but-denylisted");
    a.db()
        .seed_deleting_object(&w, o, &goid(7), 10)
        .await
        .unwrap();
    a.db().insert_tombstone(&w, o, "leaked", 100).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Deleting,
        "a tombstone must not interrupt an in-flight unlink"
    );
    assert!(a.db().is_tombstoned(&w, o).await.unwrap());
    // The unlink still finalizes normally (the seeded `deleting` row's token is its status_updated_at, 10).
    a.db().finalize_delete(&w, o, 10, 200).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[tokio::test]
async fn claim_expired_quarantine_spares_a_refreshed_reused_op() {
    // The janitor's claim-before-rm guard: a quarantine row whose expiry was refreshed into the future (op-id
    // reuse by a retry) must NOT be claimed for sweeping at a `now` past the OLD expiry — only a still-expired
    // row is. This is what stops the janitor from rm'ing an active, re-staged quarantine out from under an
    // in-flight migrate.
    let fx = Fixture::new("t-q-claim").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let dir = a.workspace_quarantine_dir(&w, &op("k"));
    a.db()
        .insert_quarantine(&w, &op("k"), &dir.to_string_lossy(), 150)
        .await
        .unwrap();
    // A retry reuses the op id and refreshes the expiry far into the future (the active in-flight upload).
    a.db()
        .insert_quarantine(&w, &op("k"), &dir.to_string_lossy(), 10_000)
        .await
        .unwrap();
    // At now = 200 (past the OLD 150 expiry) the refreshed row is spared, and stays tracked.
    assert!(
        !a.db()
            .claim_expired_quarantine(&w, &op("k"), 200)
            .await
            .unwrap(),
        "a refreshed (reused) quarantine must not be claimed"
    );
    assert_eq!(
        a.db().expired_quarantine_ops(&w, 10_000).await.unwrap(),
        vec![op("k")],
        "the active quarantine row survives"
    );
    // Once it truly expires, the claim wins and removes the row.
    assert!(
        a.db()
            .claim_expired_quarantine(&w, &op("k"), 10_001)
            .await
            .unwrap()
    );
    assert!(
        a.db()
            .expired_quarantine_ops(&w, 10_001)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn recovery_reclaim_fences_off_the_superseded_gc_claimant() {
    // A pre-merge review finding: a recovery sweep that re-claims a `deleting` row a live GC claimed (because a
    // long/frozen pass let it look stale) must FENCE OFF that original claimant — only one actor may unlink +
    // finalize, or a re-migrate's freshly re-installed bytes could be deleted out from under it (a
    // phantom-`present` byte loss). The fence is the claim token (`status_updated_at`): both
    // `confirm_deleting_owner` (gating the unlink) and the token-gated `finalize_delete` reject a superseded
    // claimant. Driven at the SQL layer with explicit timestamps (no real clock) so the interleaving is
    // deterministic.
    let fx = Fixture::new("t-claim-fence").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let o = object_id(b"contended");
    a.db()
        .install_object(&w, o, Location::Git, &goid(9), 1, 100)
        .await
        .unwrap();

    // (1) The live GC claims it with token T=100 (a back-dated, pass-fixed `now`).
    match a.db().claim_for_delete(&w, o, 100).await.unwrap() {
        ClaimOutcome::Claimed { git_oid, .. } => assert_eq!(git_oid, goid(9)),
        ClaimOutcome::Spared => panic!("expected claimed"),
    }
    // (2) A recovery sweep finds it stale (sua=100 < older_than=200) and re-claims it with token T=500.
    assert_eq!(
        a.db()
            .claim_stale_for_recovery(&w, o, 200, 500)
            .await
            .unwrap(),
        Some((Location::Git, goid(9))),
        "recovery re-claims the stale deleting row"
    );

    // (3) The superseded original GC has LOST ownership: its owner-check fails (so it skips its unlink)…
    assert!(
        !a.db().confirm_deleting_owner(&w, o, 100).await.unwrap(),
        "the superseded GC must not still own the row"
    );
    // …and its token-gated finalize is a no-op (it must NOT flip the row recovery now owns).
    a.db().finalize_delete(&w, o, 100, 600).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Deleting,
        "a superseded finalize must not advance the row"
    );

    // (4) The recovery owner (token=500) confirms ownership and finalizes the row exactly once.
    assert!(a.db().confirm_deleting_owner(&w, o, 500).await.unwrap());
    a.db().finalize_delete(&w, o, 500, 600).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[tokio::test]
async fn migrate_re_materializes_a_present_row_whose_bytes_a_crash_removed() {
    // A pre-merge review finding: a `present` row whose loose object a past crash silently removed (the
    // WAL power-loss residual) must NOT be blindly dedup-reused — `migrate_finish`'s non-expiring lease would
    // then root a version over gone bytes (a permanent, dedup-poisoning byte loss). install_one's belt stats
    // the loose object and re-materializes it from the candidate's quarantine instead of dedup-skipping.
    let fx = Fixture::new("e-belt").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"resurrect me from quarantine";
    let v1 = ingest_migrate(a, &w, "op1", vec![file("SKILL.md", body)], 100).await;
    let git_oid = v1.entries[0].git_oid;

    // Simulate the crash residual: physically remove the loose object but LEAVE the row `present`.
    a.open_store(&w)
        .unwrap()
        .delete_loose_object(git_oid)
        .unwrap();
    assert!(
        !a.open_store(&w).unwrap().object_exists(git_oid).unwrap(),
        "precondition: the bytes are physically gone"
    );
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present,
        "but the DB row still says present (the residual a migrate must not trust blindly)"
    );

    // A re-migrate of the same content hits install_one's Present branch — the belt must re-materialize the
    // bytes from this candidate's quarantine rather than dedup-skip over the gone object.
    let v2 = ingest_migrate(a, &w, "op2", vec![file("SKILL.md", body)], 200).await;

    assert!(
        a.open_store(&w).unwrap().object_exists(git_oid).unwrap(),
        "the belt re-materialized the loose object"
    );
    // The version now renders byte-exact (the bytes are back at their final path).
    let store = a.open_store(&w).unwrap();
    let rendered = store
        .render_verified(v2.version_id.0, v2.bundle_digest)
        .expect("render after the belt heals the missing bytes");
    assert_eq!(rendered.files.len(), 1);
    assert_eq!(rendered.files[0].bytes, body);
}

// ===== The size-routed large-object store (offload) — the release-blocker criteria =====

use topos_gitstore::LargeObjectStore as _;

/// A deterministic blob of `n` bytes filled with `seed` (distinct seeds → distinct content + object ids).
fn blob(n: usize, seed: u8) -> Vec<u8> {
    vec![seed; n]
}

#[tokio::test]
async fn placement_independent_identity_same_bytes_either_store() {
    // THE load-bearing property: the SAME bytes yield the SAME version_id AND bundle_digest whether routed
    // to git or to large-local (every id is precomputed over real-byte sha256s, before any store write). We
    // force the placement by varying the configurable threshold across two runs of an identical bundle.
    let big = blob(4096, 0xA1);
    let w = ws("w_acme");

    // Run 1: a huge threshold keeps the 4 KiB blob in the git store.
    let fx_git = Fixture::with_large_limits("id-git", 1 << 30, 1 << 30).await;
    let s_git = ingest_migrate(
        &fx_git.authority,
        &w,
        "op1",
        vec![file("model.bin", &big)],
        100,
    )
    .await;

    // Run 2: a tiny threshold routes the SAME blob to the large-object store.
    let fx_large = Fixture::with_large_limits("id-large", 1, 1 << 30).await;
    let s_large = ingest_migrate(
        &fx_large.authority,
        &w,
        "op1",
        vec![file("model.bin", &big)],
        100,
    )
    .await;

    assert_eq!(
        s_git.version_id.0, s_large.version_id.0,
        "version_id must be byte-identical regardless of placement"
    );
    assert_eq!(
        s_git.bundle_digest, s_large.bundle_digest,
        "bundle_digest must be byte-identical regardless of placement"
    );

    // …and the two runs genuinely placed the bytes in different stores (else the assertion is vacuous).
    let obj = object_id(&big);
    assert_eq!(
        fx_git
            .authority
            .db()
            .object_location(&w, obj)
            .await
            .unwrap(),
        Some(Location::Git)
    );
    assert_eq!(
        fx_large
            .authority
            .db()
            .object_location(&w, obj)
            .await
            .unwrap(),
        Some(Location::LargeLocal)
    );
    // The large run physically holds the bytes in the side store; the git run does not.
    assert!(fx_large.authority.large_store(&w).exists(obj.0).unwrap());
    assert!(!fx_git.authority.large_store(&w).exists(obj.0).unwrap());
}

#[tokio::test]
async fn routes_by_size_keeps_small_in_git_and_rejects_oversize_at_ingest() {
    // threshold 1 KiB, hard cap 4 KiB.
    let fx = Fixture::with_large_limits("route", 1024, 4096).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let small: &[u8] = b"a small prose file well under the threshold";
    let big = blob(2048, 0x77); // >= 1 KiB and <= 4 KiB → offloaded
    let staged = ingest_migrate(
        a,
        &w,
        "op1",
        vec![file("SKILL.md", small), file("model.bin", &big)],
        100,
    )
    .await;

    assert_eq!(
        a.db().object_location(&w, object_id(small)).await.unwrap(),
        Some(Location::Git),
        "a sub-threshold blob stays in git"
    );
    assert_eq!(
        a.db().object_location(&w, object_id(&big)).await.unwrap(),
        Some(Location::LargeLocal),
        "a blob at/above the threshold offloads"
    );
    // Commits + trees always stay in git — the version still renders (render reads the git tree + commit).
    let rendered = crate::read::render_version(a, &w, staged.version_id.0, staged.bundle_digest)
        .await
        .expect("the mixed version renders");
    assert_eq!(rendered.files.len(), 2);

    // A blob over the per-blob cap is rejected TYPED at ingest, recording nothing (no row, no quarantine).
    let oversize = blob(5000, 0x99); // > 4 KiB cap
    let err = lifecycle::ingest(
        a,
        &w,
        &op("op2"),
        genesis(vec![file("huge.bin", &oversize)]),
        200,
    )
    .await;
    assert!(
        matches!(err, Err(AuthorityError::RejectedUpload(_))),
        "an oversize blob must be rejected typed at ingest"
    );
    assert_eq!(
        a.db()
            .object_status(&w, object_id(&oversize))
            .await
            .unwrap(),
        ObjectStatus::Absent,
        "a rejected oversize blob records no presence row"
    );
    assert!(
        !a.workspace_quarantine_dir(&w, &op("op2")).exists(),
        "a rejected oversize blob stages nothing to the quarantine"
    );
}

#[tokio::test]
async fn renders_a_mixed_offloaded_and_git_bundle_byte_exact() {
    let fx = Fixture::with_large_limits("render-mix", 1024, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let prose: &[u8] = b"# SKILL\nrun the thing\n";
    let nested: &[u8] = b"#!/bin/sh\necho nested\n";
    let model = blob(3000, 0x42);
    let staged = ingest_migrate(
        a,
        &w,
        "op1",
        vec![
            file("SKILL.md", prose),
            file("model.bin", &model),
            file("scripts/run.sh", nested),
        ],
        100,
    )
    .await;
    assert_eq!(
        a.db().object_location(&w, object_id(&model)).await.unwrap(),
        Some(Location::LargeLocal)
    );
    assert_eq!(
        a.db().object_location(&w, object_id(prose)).await.unwrap(),
        Some(Location::Git)
    );

    let rendered = crate::read::render_version(a, &w, staged.version_id.0, staged.bundle_digest)
        .await
        .expect("render the offloaded bundle");
    assert_eq!(
        rendered.bundle_digest, staged.bundle_digest,
        "the recomputed digest must match the pin (consent holds across stores)"
    );
    let got: std::collections::HashMap<&str, &[u8]> = rendered
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.bytes.as_slice()))
        .collect();
    assert_eq!(got["SKILL.md"], prose);
    assert_eq!(got["model.bin"], model.as_slice());
    assert_eq!(got["scripts/run.sh"], nested);

    // A wrong pin is refused — the consent gate holds for an offloaded bundle too.
    assert!(matches!(
        crate::read::render_version(a, &w, staged.version_id.0, [0u8; 32]).await,
        Err(AuthorityError::Integrity(_))
    ));
}

#[tokio::test]
async fn offloaded_object_read_is_skill_scoped_404_never_by_bare_hash() {
    let fx = Fixture::with_large_limits("r1-offload", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (s, other) = (skill("s_pr"), skill("s_other"));
    let reader = prin("dev_read");
    let outsider = prin("dev_out");
    let body = blob(2048, 0x6F);
    let staged = ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    let obj = object_id(&body);
    assert_eq!(
        a.db().object_location(&w, obj).await.unwrap(),
        Some(Location::LargeLocal)
    );
    // Make the offloaded object readable for skill `s`: provenance + reachability + roster.
    a.db()
        .seed_commit(&w, &s, staged.version_id, &[obj])
        .await
        .unwrap();
    a.db().seed_roster(&w, &s, &reader).await.unwrap();
    a.db().seed_roster(&w, &other, &reader).await.unwrap();

    // A rostered reader of `s` gets the offloaded bytes (the read dispatched to the large store + re-verified).
    assert_eq!(a.read_object(&reader, &w, &s, obj).await.unwrap(), body);
    // An unrostered principal → the single indistinguishable NotFound (404, never 403).
    assert!(matches!(
        a.read_object(&outsider, &w, &s, obj).await,
        Err(AuthorityError::NotFound)
    ));
    // The reader, but via a DIFFERENT skill that does not reach the object → the SAME NotFound (never by
    // bare hash — the large surface is gated by exactly the same skill-scoped join as git).
    assert!(matches!(
        a.read_object(&reader, &w, &other, obj).await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn cross_workspace_offload_has_no_dedup_and_stays_isolated() {
    let fx = Fixture::with_large_limits("xws-offload", 1, 1 << 30).await;
    let a = &fx.authority;
    let (wa, wb) = (ws("w_acme"), ws("w_globex"));
    let s = skill("s_pr");
    let p = prin("dev_read");
    let body = blob(2048, 0xC0); // byte-identical content uploaded by both workspaces
    let sa = ingest_migrate(a, &wa, "opa", vec![file("model.bin", &body)], 100).await;
    let _sb = ingest_migrate(a, &wb, "opb", vec![file("model.bin", &body)], 100).await;
    let obj = object_id(&body);

    // Two distinct physical objects under separate per-workspace roots — no cross-workspace dedup.
    assert!(a.large_store(&wa).exists(obj.0).unwrap());
    assert!(a.large_store(&wb).exists(obj.0).unwrap());

    // Readable in A only; an A-rostered principal cannot reach it via B (cross-workspace isolation).
    a.db()
        .seed_commit(&wa, &s, sa.version_id, &[obj])
        .await
        .unwrap();
    a.db().seed_roster(&wa, &s, &p).await.unwrap();
    assert_eq!(a.read_object(&p, &wa, &s, obj).await.unwrap(), body);
    assert!(matches!(
        a.read_object(&p, &wb, &s, obj).await,
        Err(AuthorityError::NotFound)
    ));

    // Strong no-dedup guard: reclaiming B's (unrooted) copy must leave A's byte-identical object intact — if
    // the two workspaces shared one physical file, GC in B would destroy A's bytes too. (A is rooted by the
    // commit_object edge above; B is unrooted once its lease is released, so it is GC-eligible.)
    a.db().release_lease(&wb, &op("opb")).await.unwrap();
    assert_eq!(gc::run_gc(a, &wb, 1_000_000).await.unwrap(), 1);
    assert!(
        !a.large_store(&wb).exists(obj.0).unwrap(),
        "B's copy is reclaimed by B's GC"
    );
    assert!(
        a.large_store(&wa).exists(obj.0).unwrap(),
        "A's byte-identical object survives a GC in B — two distinct physical objects, no cross-ws dedup"
    );
    assert_eq!(a.read_object(&p, &wa, &s, obj).await.unwrap(), body);
}

#[tokio::test]
async fn gc_reclaims_an_offloaded_object_by_the_same_fence() {
    let fx = Fixture::with_large_limits("gc-offload", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x5A);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    let obj = object_id(&body);
    assert_eq!(
        a.db().object_location(&w, obj).await.unwrap(),
        Some(Location::LargeLocal)
    );
    assert!(a.large_store(&w).exists(obj.0).unwrap());

    // Abandon the migrate (the deferred pointer-move never took the root): release the committed lease.
    // No commit_object roots it, so GC reclaims it — and the unlink step dispatches to the LARGE store.
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Absent
    );
    assert!(
        !a.large_store(&w).exists(obj.0).unwrap(),
        "the offloaded object must be physically unlinked from the large store"
    );
}

#[tokio::test]
async fn a_live_lease_spares_an_offloaded_object_from_gc() {
    let fx = Fixture::with_large_limits("gc-lease", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x33);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    let obj = object_id(&body);
    // A successful migrate's lease is non-expiring, so even a far-future GC reclaims nothing — the fence's
    // lease protection holds for an offloaded object exactly as for a git one.
    assert_eq!(gc::run_gc(a, &w, 1_000_000_000).await.unwrap(), 0);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Present
    );
    assert!(a.large_store(&w).exists(obj.0).unwrap());
}

#[tokio::test]
async fn a_reclaimed_large_local_object_reports_no_live_location() {
    // A reclaimed large-local object leaves an `absent` row that STILL records `location = large-local`, but
    // `object_location` honors only a `present` row — so a stale location can never mis-route a later read to
    // the deleted side-store object (it reports None; reads dispatch on the live presence row, never a stale one).
    let fx = Fixture::with_large_limits("stale-loc", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x7E);
    let obj = object_id(&body);

    // Offload it, then abandon + GC it → the row goes `absent` but keeps `location = large-local`.
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Absent
    );
    // The stale `absent` row records `large-local`, but `object_location` reports no LIVE location.
    assert_eq!(a.db().object_location(&w, obj).await.unwrap(), None);
}

#[tokio::test]
async fn an_authorized_read_of_a_gone_offloaded_object_is_integrity_not_notfound() {
    // The skill-scoped-read invariant: a post-authz fetch failure on the LARGE surface is an Integrity fault,
    // never NotFound — so the indistinguishable 404 still only ever comes from the access join, not a miss.
    let fx = Fixture::with_large_limits("auth-integrity", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let s = skill("s_pr");
    let p = prin("dev_read");
    let body = blob(2048, 0x4D);
    let obj = object_id(&body);
    let staged = ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    a.db()
        .seed_commit(&w, &s, staged.version_id, &[obj])
        .await
        .unwrap();
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    // The object is authorized + offloaded; now remove the large bytes out-of-band (a provenance/store
    // divergence). The read is reachable only AFTER authz, so surfacing the fault discloses nothing.
    a.large_store(&w).delete(obj.0).unwrap();
    assert!(
        matches!(
            a.read_object(&p, &w, &s, obj).await,
            Err(AuthorityError::Integrity(_))
        ),
        "a gone offloaded object on an authorized read must be Integrity, never NotFound"
    );
}

#[tokio::test]
async fn offloaded_dedup_reuse_and_the_re_materialize_belt() {
    // The migrate Present branch for a large-local object: a second migrate of the same bytes dedup-reuses
    // the existing row, and — if a crash lost the large bytes — the belt re-materializes them from the
    // candidate's quarantine into the RECORDED store (large-local), never re-routing by size.
    let fx = Fixture::with_large_limits("dedup-belt", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x2B);
    let obj = object_id(&body);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    assert!(a.large_store(&w).exists(obj.0).unwrap());

    // Simulate a crash that lost the large bytes, then a second migrate of the SAME content — install_one
    // hits Present, reads the recorded large-local location, and re-materializes from op2's quarantine.
    a.large_store(&w).delete(obj.0).unwrap();
    assert!(!a.large_store(&w).exists(obj.0).unwrap());
    ingest_migrate(a, &w, "op2", vec![file("model.bin", &body)], 200).await;
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Present
    );
    assert!(
        a.large_store(&w).exists(obj.0).unwrap(),
        "the dedup-reuse belt must re-materialize a crash-lost large object into the recorded store"
    );
}

#[tokio::test]
async fn dedup_reuse_honors_the_recorded_location_when_the_threshold_diverges() {
    // The load-bearing Present-branch rule: a dedup-reuse re-materializes into the object's RECORDED store,
    // it NEVER re-routes by the new candidate's size. Construct a genuine divergence: migrate under a HUGE
    // threshold (the blob lands in git), then — simulating an operator lowering the threshold — migrate the
    // SAME bytes via a SECOND authority over the same stores with a tiny threshold (whose size-route would
    // now pick large-local). The object must stay in git; the large store must never receive it.
    let fx = Fixture::with_large_limits("recorded-loc", 1 << 30, 1 << 30).await; // huge → routes to git
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x1D);
    let obj = object_id(&body);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    assert_eq!(
        a.db().object_location(&w, obj).await.unwrap(),
        Some(Location::Git)
    );
    assert!(!a.large_store(&w).exists(obj.0).unwrap());

    // A second authority over the SAME stores, now with a tiny threshold (its size-route would say large).
    let a2 = Authority::open_sqlite(
        &fx.dir.join("plane.db"),
        &fx.dir.join("stores"),
        &fx.dir.join("large"),
    )
    .await
    .expect("open a second authority over the same stores")
    .with_large_limits(1, 1 << 30);
    ingest_migrate(&a2, &w, "op2", vec![file("model.bin", &body)], 200).await;

    // The recorded location is still git, and the large store never received the bytes — the belt honored
    // the recorded location instead of re-routing the existing object by the new candidate's size.
    assert_eq!(
        a2.db().object_location(&w, obj).await.unwrap(),
        Some(Location::Git)
    );
    assert!(
        !a2.large_store(&w).exists(obj.0).unwrap(),
        "a dedup-reuse must not re-route an existing git object to the large store by the new size"
    );
}

#[tokio::test]
async fn recovery_sweep_reclaims_a_crashed_offloaded_deleting_object() {
    // "a crashed deleting must still recover" — for an OFFLOADED object: a GC claim that crashed before
    // finalizing leaves a stale `deleting` large-local row; the recovery sweep re-claims it and the unlink
    // dispatches to the LARGE store, then finalizes it absent.
    let fx = Fixture::with_large_limits("recover-offload", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x6C);
    let obj = object_id(&body);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    // A GC claims it (present → deleting) at t=100 but "crashes" before unlinking/finalizing.
    assert!(matches!(
        a.db().claim_for_delete(&w, obj, 100).await.unwrap(),
        ClaimOutcome::Claimed {
            location: Location::LargeLocal,
            ..
        }
    ));
    assert!(a.large_store(&w).exists(obj.0).unwrap()); // bytes still on disk (crash before unlink)

    // The recovery sweep, far past the stale threshold, finalizes it — unlinking the LARGE object.
    assert_eq!(gc::recovery_sweep(a, 100_000).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Absent
    );
    assert!(
        !a.large_store(&w).exists(obj.0).unwrap(),
        "recovery must physically unlink the offloaded object from the large store"
    );
}

#[tokio::test]
async fn routing_boundaries_at_threshold_and_cap_are_exact() {
    // threshold 1024 (offload at/above), cap 2048 (reject above).
    let fx = Fixture::with_large_limits("boundary", 1024, 2048).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let at = blob(1024, 0x01); // == threshold → large
    let below = blob(1023, 0x02); // < threshold → git
    ingest_migrate(
        a,
        &w,
        "op1",
        vec![file("at.bin", &at), file("below.bin", &below)],
        100,
    )
    .await;
    assert_eq!(
        a.db().object_location(&w, object_id(&at)).await.unwrap(),
        Some(Location::LargeLocal),
        "a blob exactly at the threshold offloads (>=)"
    );
    assert_eq!(
        a.db().object_location(&w, object_id(&below)).await.unwrap(),
        Some(Location::Git),
        "a blob one byte below the threshold stays in git"
    );

    // Exactly at the cap is accepted; one byte over is rejected at ingest.
    let at_cap = blob(2048, 0x03);
    ingest_migrate(a, &w, "op2", vec![file("atcap.bin", &at_cap)], 200).await;
    assert_eq!(
        a.db()
            .object_location(&w, object_id(&at_cap))
            .await
            .unwrap(),
        Some(Location::LargeLocal)
    );
    let over_cap = blob(2049, 0x04);
    assert!(
        matches!(
            lifecycle::ingest(
                a,
                &w,
                &op("op3"),
                genesis(vec![file("over.bin", &over_cap)]),
                300
            )
            .await,
            Err(AuthorityError::RejectedUpload(_))
        ),
        "a blob one byte over the cap is rejected at ingest"
    );
}

#[tokio::test]
async fn single_object_read_of_a_git_file_in_a_mixed_bundle_succeeds() {
    // Regression: a git-resident object in a version that ALSO contains an offloaded blob must read fine. The
    // git arm reads the loose object directly by its locator, NOT by walking the whole version tree — which
    // would fault on the offloaded sibling's intentionally-absent git object before reaching the requested
    // blob (and return a spurious Integrity for a perfectly valid read).
    let fx = Fixture::with_large_limits("mixed-read", 1024, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let s = skill("s_pr");
    let p = prin("dev_read");
    let small: &[u8] = b"a small git-resident file, well below the threshold";
    let big = blob(2048, 0x5E); // >= threshold → offloaded (its git object is absent)
    let (small_id, big_id) = (object_id(small), object_id(&big));
    let staged = ingest_migrate(
        a,
        &w,
        "op1",
        vec![file("small.txt", small), file("big.bin", &big)],
        100,
    )
    .await;
    assert_eq!(
        a.db().object_location(&w, small_id).await.unwrap(),
        Some(Location::Git)
    );
    assert_eq!(
        a.db().object_location(&w, big_id).await.unwrap(),
        Some(Location::LargeLocal)
    );
    a.db()
        .seed_commit(&w, &s, staged.version_id, &[small_id, big_id])
        .await
        .unwrap();
    a.db().seed_roster(&w, &s, &p).await.unwrap();

    // The git-resident file reads correctly despite the offloaded sibling…
    assert_eq!(a.read_object(&p, &w, &s, small_id).await.unwrap(), small);
    // …and the offloaded file reads too (dispatched to the large store).
    assert_eq!(a.read_object(&p, &w, &s, big_id).await.unwrap(), big);
}

#[tokio::test]
async fn fenced_migrate_rejects_a_dotgit_path() {
    // The fenced migrate must reject a `.git` path exactly as the client write path does — the kernel
    // check_path allows `.git` (it only bars `.`/`..`/NUL/absolute), so ingest stages it, but the migrate's
    // tree build (the plumbing editor + restored component validation) refuses it, so no `.git` bundle is
    // ever recorded.
    let fx = Fixture::new("dotgit").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let staged = lifecycle::ingest(
        a,
        &w,
        &op("op1"),
        genesis(vec![file(".git/config", b"[core]\n")]),
        100,
    )
    .await
    .expect("ingest stages (check_path allows .git)");
    assert!(
        matches!(
            lifecycle::migrate(a, &w, &staged, 100).await,
            Err(AuthorityError::RejectedUpload(_))
        ),
        "the fenced migrate must reject a .git path at the tree build"
    );
}

#[tokio::test]
async fn gc_reclaims_large_objects_when_no_git_repo_exists() {
    // A workspace whose FIRST migrate routed every blob to the large store, then crashed before
    // migrate_finish created the git repo: the large-local rows must still be reclaimable. GC opens the git
    // store lazily (only for a git unlink), so it does not abort on the missing repo.
    let fx = Fixture::with_large_limits("no-git-gc", 1, 1 << 30).await; // tiny threshold → all blobs offload
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x4F);
    let obj = object_id(&body);

    // ingest + lease + install, but NOT migrate_finish (which is what creates the main git repo) — a crash
    // before finish. The blob is offloaded, so no git object/repo was ever created.
    let staged = lifecycle::ingest(
        a,
        &w,
        &op("op1"),
        genesis(vec![file("big.bin", &body)]),
        100,
    )
    .await
    .expect("ingest");
    lifecycle::migrate_lease(a, &w, &staged, 100).await.unwrap();
    lifecycle::migrate_install(a, &w, &staged, 100)
        .await
        .unwrap();
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Present
    );
    assert!(a.large_store(&w).exists(obj.0).unwrap());
    assert!(
        !fx.dir.join("stores").join("w_acme").exists(),
        "no git repo was created (all blobs offloaded)"
    );

    // Abandon the crashed migrate's lease, then GC — it must reclaim the large-local object WITHOUT a git repo.
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    assert_eq!(gc::run_gc(a, &w, 1_000_000).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Absent
    );
    assert!(!a.large_store(&w).exists(obj.0).unwrap());
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

#[tokio::test]
async fn an_open_non_stale_proposal_roots_and_reads_then_a_stale_one_reclaims_and_404s() {
    // The keep-set == read-surface crux: an open, non-stale proposal's unique object is kept + readable; the
    // instant a publish stales the proposal the SAME object drops out of read AND retention together (no
    // event, no reaper), and
    // a read of the reclaimed object is 404 — never an Integrity corruption alarm.
    let fx = Fixture::new("prop-crux").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    a.db().seed_roster(&w, &s, &reader).await.unwrap();

    // `current` points at a base commit Cb at (1,1) — the proposal's base.
    let cb = CommitId([0xB0; 32]);
    a.db().seed_commit(&w, &s, cb, &[]).await.unwrap();
    a.db().seed_current(&w, &s, cb, 1, 1).await.unwrap();

    // The proposal's unique object X: migrated (present + readable), rooted by nothing but the proposal.
    let xbytes = b"proposed unique bytes";
    let cp = migrate_unrooted(a, &w, PROP_OP_1, "NEW.md", xbytes).await;
    let x = object_id(xbytes);
    a.db()
        .seed_proposal(&w, "prop1", &s, cp, cb, 1, 1, "open", &prin("p_author"))
        .await
        .unwrap();
    a.db().seed_proposal_object(&w, "prop1", x).await.unwrap();

    // OPEN + NON-STALE: read authorizes via the proposal arm (returns the real bytes); GC spares X.
    assert_eq!(a.read_object(&reader, &w, &s, x).await.unwrap(), xbytes);
    assert_eq!(
        gc::run_gc(a, &w, 200).await.unwrap(),
        0,
        "an open, non-stale proposal roots its object"
    );
    assert_eq!(
        a.db().object_status(&w, x).await.unwrap(),
        ObjectStatus::Present
    );

    // STALE it: a publish advances `current` past the base — the eventless derived transition.
    a.db().force_current_generation(&w, &s, 1, 2).await.unwrap();

    // The read drops in the SAME step — 404 immediately, BEFORE any GC runs (a gate, not a reaper).
    assert!(matches!(
        a.read_object(&reader, &w, &s, x).await,
        Err(AuthorityError::NotFound)
    ));
    // GC now reclaims X (no trunk edge, no live lease, and the proposal is stale).
    assert_eq!(gc::run_gc(a, &w, 300).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, x).await.unwrap(),
        ObjectStatus::Absent
    );
    // A read of the reclaimed object is 404 — NEVER an Integrity alarm (keep-set == read surface).
    assert!(matches!(
        a.read_object(&reader, &w, &s, x).await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn recovery_claim_spares_an_open_proposals_object_and_reclaims_a_staled_one() {
    // The third copy of the predicate: `claim_stale_for_recovery` must spare a stale `deleting` row an open,
    // non-stale proposal roots, then reclaim it once the proposal goes stale — tracking the read gate exactly.
    let fx = Fixture::new("prop-recovery").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let cb = CommitId([0xB0; 32]);
    a.db().seed_commit(&w, &s, cb, &[]).await.unwrap();
    a.db().seed_current(&w, &s, cb, 1, 1).await.unwrap();
    let cp = CommitId([0xC0; 32]);
    a.db()
        .seed_proposal(&w, "prop1", &s, cp, cb, 1, 1, "open", &prin("p_author"))
        .await
        .unwrap();
    let x = object_id(b"recover-me");
    a.db().seed_proposal_object(&w, "prop1", x).await.unwrap();
    // A STALE `deleting` row (a crashed GC's leftover) over X: status_updated_at=0 < older_than below.
    a.db()
        .seed_deleting_object(&w, x, &goid(7), 0)
        .await
        .unwrap();

    // OPEN + NON-STALE: recovery SPARES X (None) — the proposal arm holds it, exactly like the read gate.
    assert_eq!(
        a.db()
            .claim_stale_for_recovery(&w, x, 1000, 1001)
            .await
            .unwrap(),
        None
    );

    // STALE it: recovery now RECLAIMS X (the gate dropped) — keep tracks read.
    a.db().force_current_generation(&w, &s, 1, 2).await.unwrap();
    assert!(
        a.db()
            .claim_stale_for_recovery(&w, x, 1000, 1002)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn a_rejected_proposals_unique_object_reclaims_and_reads_404() {
    // A non-`open` proposal never roots or authorizes — even at a matching base — so its unique bytes reclaim.
    let fx = Fixture::new("prop-reject").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    a.db().seed_roster(&w, &s, &reader).await.unwrap();
    let cb = CommitId([0xB0; 32]);
    a.db().seed_commit(&w, &s, cb, &[]).await.unwrap();
    a.db().seed_current(&w, &s, cb, 1, 1).await.unwrap();
    let xbytes = b"rejected bytes";
    let cp = migrate_unrooted(a, &w, PROP_OP_1, "R.md", xbytes).await;
    let x = object_id(xbytes);
    a.db()
        .seed_proposal(&w, "prop1", &s, cp, cb, 1, 1, "rejected", &prin("p_author"))
        .await
        .unwrap();
    a.db().seed_proposal_object(&w, "prop1", x).await.unwrap();

    assert!(matches!(
        a.read_object(&reader, &w, &s, x).await,
        Err(AuthorityError::NotFound)
    ));
    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, x).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[tokio::test]
async fn a_trunk_shared_object_stays_kept_and_readable_after_its_proposal_stales() {
    // An object reachable from BOTH the trunk (a `commit_object` edge) and a proposal stays kept + readable
    // when the proposal stales — the trunk arm is untouched; only the proposal's UNIQUE objects reclaim.
    let fx = Fixture::new("prop-shared").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    a.db().seed_roster(&w, &s, &reader).await.unwrap();
    let ybytes = b"shared bytes";
    let ccur = migrate_unrooted(a, &w, PROP_OP_1, "Y.md", ybytes).await;
    let y = object_id(ybytes);
    // Trunk: `current` at (1,1) points at Ccur, and Ccur edges Y.
    a.db().seed_commit(&w, &s, ccur, &[y]).await.unwrap();
    a.db().seed_current(&w, &s, ccur, 1, 1).await.unwrap();
    // A proposal ALSO roots Y (reuses it), base (1,1), open.
    let cp = CommitId([0xC0; 32]);
    a.db()
        .seed_proposal(&w, "prop1", &s, cp, ccur, 1, 1, "open", &prin("p_author"))
        .await
        .unwrap();
    a.db().seed_proposal_object(&w, "prop1", y).await.unwrap();

    // Stale the proposal; the TRUNK arm still keeps + reads Y.
    a.db().force_current_generation(&w, &s, 1, 2).await.unwrap();
    assert_eq!(a.read_object(&reader, &w, &s, y).await.unwrap(), ybytes);
    assert_eq!(
        gc::run_gc(a, &w, 300).await.unwrap(),
        0,
        "the trunk commit_object edge keeps the shared object"
    );
    assert_eq!(
        a.db().object_status(&w, y).await.unwrap(),
        ObjectStatus::Present
    );
}

#[tokio::test]
async fn genuine_corruption_under_an_open_proposal_is_integrity_not_masked_as_404() {
    // The read-time TOCTOU guard re-authorizes on a fetch miss and downgrades to 404 ONLY when the object is
    // no longer authorized (a legitimately reclaimed proposal object). An object STILL rooted by an open,
    // non-stale proposal whose bytes are gone is genuine corruption — the guard's re-authorize returns Some,
    // so the Integrity alarm must STAND, never be masked. (The guard's converse — the concurrent
    // authorize→stale→reclaim→fetch race that downgrades to 404 — is a window the single-threaded harness
    // cannot interleave; its outcome equals the reclaimed-object 404 the crux test asserts.)
    let fx = Fixture::new("prop-corrupt").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    a.db().seed_roster(&w, &s, &reader).await.unwrap();
    let cb = CommitId([0xB0; 32]);
    a.db().seed_commit(&w, &s, cb, &[]).await.unwrap();
    a.db().seed_current(&w, &s, cb, 1, 1).await.unwrap();
    let xbytes = b"present then corrupt";
    let cp = migrate_unrooted(a, &w, PROP_OP_1, "X.md", xbytes).await;
    let x = object_id(xbytes);
    a.db()
        .seed_proposal(&w, "prop1", &s, cp, cb, 1, 1, "open", &prin("p_author"))
        .await
        .unwrap();
    a.db().seed_proposal_object(&w, "prop1", x).await.unwrap();
    assert_eq!(a.read_object(&reader, &w, &s, x).await.unwrap(), xbytes);

    // Destroy the bytes underneath a still-open, non-stale proposal (the presence row stays `present`).
    let (loc, goid_x) = a.db().object_dispatch(&w, x).await.unwrap().unwrap();
    assert_eq!(loc, Location::Git);
    a.open_store(&w)
        .unwrap()
        .delete_loose_object(goid_x)
        .unwrap();

    // Read authorizes (G true), the fetch faults, re-authorize is STILL Some ⇒ Integrity stands (not 404).
    assert!(matches!(
        a.read_object(&reader, &w, &s, x).await,
        Err(AuthorityError::Integrity(_))
    ));
}

// ── the pointer-move write (`set-current`): genesis · publish · revert · the gate · interleavings ──────
//
// These drive the WHOLE backbone in-process against a real SQLite + git store: ingest → migrate → the one
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
    crate::set_current::publish(&fx.authority, ws, skill, &staged, &device, CREATED_AT, NOW)
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

#[tokio::test]
async fn genesis_creates_a_signed_pointer_at_1_1_and_verifies() {
    let fx = Fixture::new("sc-genesis").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(11);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    let r = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "11111111-1111-4111-8111-111111111111",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    assert!(r.is_ok());
    assert_eq!(r.current, Some(gn(1, 1)));

    // Read the signed record back + verify under the plane public key (the signer round-trip).
    let pubkey = fx.authority.plane_public_key().unwrap();
    let record = fx
        .authority
        .read_signed_record(&w, &s)
        .await
        .unwrap()
        .expect("signed");
    assert!(verify_record(&record, "w_acme", "s_deploy", &pubkey));

    // A one-bit flip fails; a wrong scope fails (the pointer cannot be replayed into another skill/ws).
    let mut tampered = record.clone();
    let i = tampered.len() / 2;
    tampered[i] ^= 0x01;
    // (The tampered bytes may not even deserialize; either way it must NOT verify.)
    let tampered_ok = std::panic::catch_unwind(|| {
        serde_json::from_slice::<SignedCurrentRecord>(&tampered)
            .map(|_| ())
            .is_ok()
    })
    .unwrap_or(false);
    if tampered_ok {
        assert!(!verify_record(&tampered, "w_acme", "s_deploy", &pubkey));
    }
    assert!(!verify_record(&record, "w_acme", "s_OTHER", &pubkey));
    assert!(!verify_record(&record, "w_OTHER", "s_deploy", &pubkey));
}

#[tokio::test]
async fn publish_advances_seq_within_the_epoch() {
    let fx = Fixture::new("sc-advance").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(12);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    let g = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "00000000-0000-4000-8000-000000000001",
        genesis(vec![file("a", b"1")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(g.current, Some(gn(1, 1)));
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    let r = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "00000000-0000-4000-8000-000000000002",
        child(c0, vec![file("a", b"2")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(r.current, Some(gn(1, 2)));
}

/// Interleaving A — two publishes based on the same generation: exactly one OK, the other a stable CONFLICT
/// carrying the live generation; the pointer advances exactly once.
#[tokio::test]
async fn concurrent_publishes_one_ok_one_conflict() {
    let fx = Fixture::new("sc-concurrent").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(13);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    let g = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "aaaaaaaa-0000-4000-8000-000000000000",
        genesis(vec![file("a", b"0")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(g.current, Some(gn(1, 1)));
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // Prepare two distinct candidates, both based on (1,1); then drive the two pointer-moves concurrently.
    let (sa, da) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "aaaaaaaa-0000-4000-8000-000000000001",
        child(c0, vec![file("a", b"A")]),
        gn(1, 1),
    )
    .await;
    let (sb, db) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "aaaaaaaa-0000-4000-8000-000000000002",
        child(c0, vec![file("a", b"B")]),
        gn(1, 1),
    )
    .await;
    let (ra, rb) = tokio::join!(
        crate::set_current::publish(&fx.authority, &w, &s, &sa, &da, CREATED_AT, NOW),
        crate::set_current::publish(&fx.authority, &w, &s, &sb, &db, CREATED_AT, NOW),
    );
    let (ra, rb) = (ra.unwrap(), rb.unwrap());
    let outcomes = [ra.outcome, rb.outcome];
    assert!(
        outcomes.contains(&TerminalOutcome::Ok),
        "one must be OK: {outcomes:?}"
    );
    assert!(
        outcomes.contains(&TerminalOutcome::Conflict),
        "one must CONFLICT: {outcomes:?}"
    );
    // The conflicter carries the LIVE generation, and the pointer advanced exactly once.
    let conflict = if ra.outcome == TerminalOutcome::Conflict {
        &ra
    } else {
        &rb
    };
    assert_eq!(conflict.current, Some(gn(1, 2)));
}

/// Interleaving C — a revert advances `seq` across a byte round-trip, so a stale move at the pre-revert
/// generation CONFLICTs (a digest-only CAS would wrongly accept it; the whole-(epoch,seq) CAS catches it).
#[tokio::test]
async fn revert_advances_seq_and_a_stale_publish_conflicts() {
    let fx = Fixture::new("sc-revert").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(14);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    // genesis X(β) → (1,1); publish Y(γ) → (1,2).
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "cccccccc-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"beta")]),
        gn(0, 0),
    )
    .await;
    let x = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "cccccccc-0000-4000-8000-000000000001",
        child(x, vec![file("f", b"gamma")]),
        gn(1, 1),
    )
    .await;

    // revert --to X → R(tree=β, parents=[Y]) → (1,3). seq advances; bytes return to β.
    let rop = op("cccccccc-0000-4000-8000-000000000002");
    let rsig = sign_revert(&fx, &key, "dk_a", &w, &s, x, &rop, gn(1, 2)).await;
    let rdev = DeviceSignedOp {
        device_key_id: "dk_a".to_owned(),
        op: DeviceOp::Revert,
        signature: rsig,
        expected: gn(1, 2),
    };
    let rev = fx
        .authority
        .revert(
            &w,
            &s,
            x,
            rdev,
            "d_test",
            "topos revert",
            &rop,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert!(rev.is_ok(), "revert outcome: {:?}", rev.outcome);
    assert_eq!(rev.current, Some(gn(1, 3)));

    // A stale publish pinned to the PRE-revert generation (1,2) → CONFLICT (live (1,3)), even though the
    // live tree is byte-identical to what it based on.
    let y = fx.authority.db().read_current_commit(&w, &s).await.unwrap();
    let _ = y;
    let (ss, ds) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "cccccccc-0000-4000-8000-000000000003",
        child(x, vec![file("f", b"delta")]),
        gn(1, 2),
    )
    .await;
    let stale = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(stale.outcome, TerminalOutcome::Conflict);
    assert_eq!(stale.current, Some(gn(1, 3)));
}

/// The restore-ABA: a backup/restore that bumps `epoch` while reusing `seq`. A stale op at the OLD
/// generation (matching `seq`, lower `epoch`) CONFLICTs — a seq-only CAS would wrongly accept it.
#[tokio::test]
async fn restore_aba_matching_seq_bumped_epoch_conflicts() {
    let fx = Fixture::new("sc-aba").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(15);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "dddddddd-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"0")]),
        gn(0, 0),
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "dddddddd-0000-4000-8000-000000000001",
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await; // (1,2)

    // Restore bumps epoch but reuses seq: (1,2) → (2,2).
    fx.authority
        .db()
        .force_current_generation(&w, &s, 2, 2)
        .await
        .unwrap();
    let c1 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // A stale op pinned to (1,2) — matching seq, lower epoch — must CONFLICT.
    let (ss, ds) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "dddddddd-0000-4000-8000-000000000002",
        child(c1, vec![file("f", b"2")]),
        gn(1, 2),
    )
    .await;
    let stale = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(stale.outcome, TerminalOutcome::Conflict);
    assert_eq!(stale.current, Some(gn(2, 2)));
}

/// Interleaving E — a lost-ack retry: the original op committed (seq=2), the team moved on (seq=3), and the
/// retry returns the BYTE-IDENTICAL original receipt (the original signed record), not a spurious conflict.
#[tokio::test]
async fn lost_ack_retry_replays_the_identical_receipt() {
    let fx = Fixture::new("sc-lostack").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(16);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "eeeeeeee-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"0")]),
        gn(0, 0),
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // Op K commits at (1,2). Keep staged + device so we can replay the SAME op.
    let (sk, dk) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "eeeeeeee-0000-4000-8000-000000000001",
        child(c0, vec![file("f", b"k")]),
        gn(1, 1),
    )
    .await;
    let first = crate::set_current::publish(&fx.authority, &w, &s, &sk, &dk, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(first.current, Some(gn(1, 2)));
    let ck = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // The team moves on to (1,3).
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "eeeeeeee-0000-4000-8000-000000000002",
        child(ck, vec![file("f", b"next")]),
        gn(1, 2),
    )
    .await;

    // Retry op K (its ack was lost): the replay returns the ORIGINAL receipt byte-for-byte (the (1,2) signed
    // record), even though current is now (1,3).
    let retry = crate::set_current::publish(&fx.authority, &w, &s, &sk, &dk, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(retry, first);
    assert_eq!(retry.current, Some(gn(1, 2)));
}

/// A device revoked BEFORE the promotion (committed ahead of the pointer-move txn) blocks the move.
#[tokio::test]
async fn a_revoke_before_promotion_blocks_the_move() {
    let fx = Fixture::new("sc-revoke").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(17);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "f0000000-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"0")]),
        gn(0, 0),
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // Prepare a publish, then revoke the device BETWEEN migrate and the pointer-move.
    let (ss, ds) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "f0000000-0000-4000-8000-000000000001",
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    fx.authority.db().revoke_device(&w, "dk_a").await.unwrap();
    let r = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    // The pointer did NOT move.
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
}

/// After a successful promote + lease-release, a GC pass does NOT reclaim the new `current`'s objects (the
/// `skill_commit` + `commit_object` edges root them) — the re-rooting handoff has no reclaim window.
#[tokio::test]
async fn post_promote_gc_does_not_reclaim_current_objects() {
    let fx = Fixture::new("sc-gcreach").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(18);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    let body = b"the current bytes";
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "10000000-0000-4000-8000-000000000000",
        genesis(vec![file("f", body)]),
        gn(0, 0),
    )
    .await;
    let obj = object_id(body);
    assert_eq!(
        fx.authority.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Present
    );

    // A full GC pass reclaims NOTHING current reaches.
    let reclaimed = gc::run_gc(&fx.authority, &w, NOW + 1_000_000)
        .await
        .unwrap();
    assert_eq!(reclaimed, 0);
    assert_eq!(
        fx.authority.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Present
    );
}

/// A first-parent mismatch (the candidate's first parent is an in-skill ancestor that is NOT current) is
/// DENIED even when the CAS matches — the parent assert is orthogonal to the generation compare.
#[tokio::test]
async fn first_parent_mismatch_is_denied_even_when_the_cas_matches() {
    let fx = Fixture::new("sc-firstparent").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(19);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    // genesis c0 → (1,1); publish c1 (parents=[c0]) → (1,2). current = c1.
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "20000000-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"0")]),
        gn(0, 0),
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "20000000-0000-4000-8000-000000000001",
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    let c1 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // A candidate parented on c0 (an in-skill ancestor — lineage passes) but NOT on current (c1), pinned to
    // the matching generation (1,2). The CAS passes; the first-parent assert rejects it.
    let (ss, ds) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "20000000-0000-4000-8000-000000000002",
        child(c0, vec![file("f", b"2")]),
        gn(1, 2),
    )
    .await;
    let r = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c1)
    ); // unmoved
    // The receipt carries the live commit id for a clock-anomaly alarm.
    let detail = r.details.unwrap();
    assert_eq!(detail["code"], "FIRST_PARENT_MISMATCH");
}

/// A two-parent author-merge candidate is rejected wholesale in the backbone (merges are a later increment).
#[tokio::test]
async fn a_two_parent_merge_is_denied() {
    let fx = Fixture::new("sc-merge").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(20);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "30000000-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"0")]),
        gn(0, 0),
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "30000000-0000-4000-8000-000000000001",
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    let c1 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // A 2-parent candidate [c1, c0] (both in-skill, parents[0]==current) — rejected for parents.len() > 1.
    let candidate = CandidateUpload {
        files: vec![file("f", b"m")],
        parents: vec![c1, c0],
        author: "d_test".to_owned(),
        message: "merge".to_owned(),
    };
    let (ss, ds) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "30000000-0000-4000-8000-000000000002",
        candidate,
        gn(1, 2),
    )
    .await;
    let r = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
}

/// The review-required gate: a direct publish preflight short-circuits to APPROVAL_REQUIRED having ingested
/// nothing; and the in-transaction read is authoritative if a migrate somehow happened first.
#[tokio::test]
async fn review_required_gates_a_direct_publish() {
    let fx = Fixture::new("sc-gate").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(21);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();

    // Genesis BYPASSES the gate (someone must create the first version; it cannot be proposed against a
    // base that does not exist) — even under review-required, it promotes.
    let g = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "40000000-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"0")]),
        gn(0, 0),
    )
    .await;
    assert!(g.is_ok());
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // A NON-genesis direct publish IS gated. Preflight: APPROVAL_REQUIRED, having ingested/migrated nothing.
    let op_id = op("40000000-0000-4000-8000-000000000001");
    let pre = crate::set_current::publish_preflight(
        &fx.authority,
        &w,
        &s,
        DeviceOp::PublishDirect,
        "dk_a",
        &op_id,
        None,
        None,
        gn(1, 1),
        CREATED_AT,
    )
    .await
    .unwrap();
    assert_eq!(pre.unwrap().outcome, TerminalOutcome::ApprovalRequired);

    // The in-txn gate is authoritative too: a direct publish that DID migrate first still fails closed, and
    // the pointer does not move.
    let (ss, ds) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "40000000-0000-4000-8000-000000000002",
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    let r = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::ApprovalRequired);
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
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

/// A revert may only target a version of the SAME skill — reverting to another skill's commit (same
/// workspace) is refused, so the forward commit can never graft a foreign tree under this skill's edges.
#[tokio::test]
async fn revert_to_another_skills_commit_is_refused() {
    let fx = Fixture::new("sc-xskill-revert").await;
    let w = ws("w_acme");
    let (s1, s2) = (skill("s_one"), skill("s_two"));
    let key = dev_key(30);
    register(&fx, &w, &s1, "dk_a", &key, "p_dev").await;
    register(&fx, &w, &s2, "dk_a", &key, "p_dev").await;

    // s2 creates a commit c2 (owned by s2); s1 has its own current.
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s2,
        "30000000-0000-4000-8000-aaaaaaaaaaaa",
        genesis(vec![file("f", b"s2 secret")]),
        gn(0, 0),
    )
    .await;
    let c2 = fx
        .authority
        .db()
        .read_current_commit(&w, &s2)
        .await
        .unwrap()
        .unwrap();
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s1,
        "30000000-0000-4000-8000-bbbbbbbbbbbb",
        genesis(vec![file("f", b"s1 bytes")]),
        gn(0, 0),
    )
    .await;
    let s1_before = fx
        .authority
        .db()
        .read_current_commit(&w, &s1)
        .await
        .unwrap()
        .unwrap();

    // s1 reverts to c2 (s2's commit) — refused; the skill-scoped digest lookup returns nothing.
    let rop = op("30000000-0000-4000-8000-cccccccccccc");
    let rdev = DeviceSignedOp {
        device_key_id: "dk_a".to_owned(),
        op: DeviceOp::Revert,
        signature: [0u8; 64],
        expected: gn(1, 1),
    };
    let r = fx
        .authority
        .revert(
            &w,
            &s1,
            c2,
            rdev,
            "d_test",
            "topos revert",
            &rop,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    // s1's pointer did not move (no foreign tree grafted).
    assert_eq!(
        fx.authority
            .db()
            .read_current_commit(&w, &s1)
            .await
            .unwrap(),
        Some(s1_before)
    );
}

/// A candidate of new bytes submitted to the PUBLISH entry signed as a non-direct op (e.g. `Revert`) is
/// rejected before ingest — otherwise it would skip the review gate while reaching the promote path.
#[tokio::test]
async fn publish_signed_as_a_non_direct_op_is_rejected_before_ingest() {
    let fx = Fixture::new("sc-opbypass").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(31);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();

    let op_id = op("31000000-0000-4000-8000-000000000000");
    let dev = DeviceSignedOp {
        device_key_id: "dk_a".to_owned(),
        op: DeviceOp::Revert,
        signature: [0u8; 64],
        expected: gn(0, 0),
    };
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &op_id,
            genesis(vec![file("f", b"sneaky")]),
            dev,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    // Nothing was promoted, and (ingested nothing) no quarantine row was opened.
    assert!(
        fx.authority
            .db()
            .read_current_commit(&w, &s)
            .await
            .unwrap()
            .is_none()
    );
}

/// A CONFLICTed publish releases its (non-expiring) promotion lease, so the abandoned candidate's unique
/// objects become GC-reclaimable rather than rooted forever.
#[tokio::test]
async fn a_conflict_releases_the_lease_so_abandoned_objects_are_reclaimable() {
    let fx = Fixture::new("sc-conflict-lease").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(32);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "32000000-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"0")]),
        gn(0, 0),
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // Two candidates based on (1,1); B carries a UNIQUE object. A wins, B conflicts.
    let b_body = b"unique-to-the-loser";
    let (sa, da) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "32000000-0000-4000-8000-00000000000a",
        child(c0, vec![file("f", b"A")]),
        gn(1, 1),
    )
    .await;
    let (sb, db) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "32000000-0000-4000-8000-00000000000b",
        child(c0, vec![file("f", b_body)]),
        gn(1, 1),
    )
    .await;
    let b_obj = object_id(b_body);

    assert!(
        crate::set_current::publish(&fx.authority, &w, &s, &sa, &da, CREATED_AT, NOW)
            .await
            .unwrap()
            .is_ok()
    );
    let rb = crate::set_current::publish(&fx.authority, &w, &s, &sb, &db, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(rb.outcome, TerminalOutcome::Conflict);

    // B's unique object is present but now unrooted (no edge, lease released) → a GC pass reclaims it.
    assert_eq!(
        fx.authority.db().object_status(&w, b_obj).await.unwrap(),
        ObjectStatus::Present
    );
    let reclaimed = gc::run_gc(&fx.authority, &w, NOW + 1_000_000)
        .await
        .unwrap();
    assert!(
        reclaimed >= 1,
        "the abandoned candidate's object must be reclaimable"
    );
    assert_eq!(
        fx.authority.db().object_status(&w, b_obj).await.unwrap(),
        ObjectStatus::Absent
    );
}

/// A revert's lost-ack retry replays the ORIGINAL OK — not OP_ID_REUSED — even though `current` has
/// advanced and a fresh forward commit would re-parent on it (the op id replays on its stable identity).
#[tokio::test]
async fn a_revert_lost_ack_retry_replays_the_original_ok() {
    let fx = Fixture::new("sc-revert-replay").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(33);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "33000000-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"beta")]),
        gn(0, 0),
    )
    .await;
    let x = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "33000000-0000-4000-8000-000000000001",
        child(x, vec![file("f", b"gamma")]),
        gn(1, 1),
    )
    .await;

    // First revert (op K) → (1,3).
    let rop = op("33000000-0000-4000-8000-000000000002");
    let rsig = sign_revert(&fx, &key, "dk_a", &w, &s, x, &rop, gn(1, 2)).await;
    let rdev = DeviceSignedOp {
        device_key_id: "dk_a".to_owned(),
        op: DeviceOp::Revert,
        signature: rsig,
        expected: gn(1, 2),
    };
    let first = fx
        .authority
        .revert(
            &w,
            &s,
            x,
            rdev.clone(),
            "d_test",
            "topos revert",
            &rop,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert!(first.is_ok());
    assert_eq!(first.current, Some(gn(1, 3)));

    // Retry the SAME op K (its ack was lost). current is now the forward commit; a fresh revert would
    // re-parent on that and derive a different commit id — but the op id replays the byte-identical OK.
    let retry = fx
        .authority
        .revert(
            &w,
            &s,
            x,
            rdev,
            "d_test",
            "topos revert",
            &rop,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(retry, first);
    assert_eq!(retry.current, Some(gn(1, 3)));
}

/// A non-canonical UUID op id (the valid-but-unhyphenated 32-hex form) is rejected — its string is a
/// distinct receipt key that decodes to the SAME 16 signed bytes, so accepting it would split the
/// idempotency slot. Requiring the canonical hyphenated form keeps the key 1:1 with the signed identity.
#[tokio::test]
async fn a_non_canonical_uuid_op_id_is_rejected() {
    let fx = Fixture::new("sc-opid-canon").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(34);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    // 32-hex simple form of a valid UUID (no hyphens) — accepted by OpId::parse + uuid::parse_str, rejected
    // by the canonical-form check.
    let r = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "34000000000040008000000000000000",
        genesis(vec![file("f", b"x")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    assert!(
        fx.authority
            .db()
            .read_current_commit(&w, &s)
            .await
            .unwrap()
            .is_none()
    );
}

// ── the contribute authority end-to-end: publish --propose · review --approve|--reject (the write paths) ──
//
// These drive the REAL propose/approve/reject through `Authority` (and the shared `set_current::propose`)
// against a live SQLite + git store — the write paths that PRODUCE the proposal/approval rows the gated GC +
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
    let r =
        crate::set_current::propose(&fx.authority, ws, skill, &staged, &device, CREATED_AT, NOW)
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

#[tokio::test]
async fn propose_opens_a_proposal_without_moving_current() {
    let fx = Fixture::new("pr-open").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(20);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "20000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let before = fx.authority.read_signed_record(&w, &s).await.unwrap();

    let unique = b"a brand new reference doc";
    let (r, _cp, _d) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "20000000-0000-4000-8000-000000000002",
        child(g, vec![file("SKILL.md", b"v0"), file("NEW.md", unique)]),
        gn(1, 1),
    )
    .await;

    // NEEDS_REVIEW, nothing signed, `current` byte-for-byte unchanged (same commit + same signed record).
    assert_eq!(r.outcome, TerminalOutcome::NeedsReview);
    assert!(r.signed_record.is_none());
    assert!(r.current.is_none());
    assert_eq!(current_commit(&fx, &w, &s).await, g);
    assert_eq!(
        fx.authority.read_signed_record(&w, &s).await.unwrap(),
        before
    );

    // The proposal's UNIQUE object is readable (the proposal read arm) and GC keeps it while open + non-stale.
    let x = object_id(unique);
    assert_eq!(
        fx.authority
            .read_object(&prin("p_author"), &w, &s, x)
            .await
            .unwrap(),
        unique
    );
    assert_eq!(gc::run_gc(&fx.authority, &w, NOW).await.unwrap(), 0);
}

#[tokio::test]
async fn a_propose_against_an_absent_current_fails_typed_uploading_nothing() {
    let fx = Fixture::new("pr-genesis").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(20);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    // No genesis publish: `current` is absent. A `--propose` must fail typed (a proposal needs a base) and
    // upload nothing — the first version is a direct genesis publish.
    let device = DeviceSignedOp {
        device_key_id: "dk".to_owned(),
        op: DeviceOp::PublishPropose,
        signature: [0u8; 64],
        expected: gn(0, 0),
    };
    let r = fx
        .authority
        .propose(
            &w,
            &s,
            &op("20000000-0000-4000-8000-000000000099"),
            genesis(vec![file("SKILL.md", b"v0")]),
            device,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    assert!(
        fx.authority
            .read_signed_record(&w, &s)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_proposal_staled_by_a_publish_then_gc_reclaims_its_unique_object_and_reads_404() {
    // The keep-set == read-surface crux through the REAL write paths: propose roots a unique object (kept +
    // readable); a direct publish stales the proposal; GC reclaims the unique object; a read is 404, never Integrity.
    let fx = Fixture::new("pr-stale-gc").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(21);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "21000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let unique = b"proposed-only bytes";
    do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "21000000-0000-4000-8000-000000000002",
        child(g, vec![file("SKILL.md", b"v0"), file("NEW.md", unique)]),
        gn(1, 1),
    )
    .await;
    let x = object_id(unique);
    // Kept + readable while open.
    assert_eq!(gc::run_gc(&fx.authority, &w, NOW).await.unwrap(), 0);
    assert!(
        fx.authority
            .read_object(&prin("p_author"), &w, &s, x)
            .await
            .is_ok()
    );

    // A direct publish advances `current` → the proposal is now stale.
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "21000000-0000-4000-8000-000000000003",
        child(g, vec![file("SKILL.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    // The read drops immediately; GC reclaims the now-unrooted unique object; the read stays 404 (not Integrity).
    assert!(matches!(
        fx.authority.read_object(&prin("p_author"), &w, &s, x).await,
        Err(AuthorityError::NotFound)
    ));
    assert_eq!(gc::run_gc(&fx.authority, &w, NOW).await.unwrap(), 1);
    assert_eq!(
        fx.authority.db().object_status(&w, x).await.unwrap(),
        ObjectStatus::Absent
    );
    assert!(matches!(
        fx.authority.read_object(&prin("p_author"), &w, &s, x).await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn propose_then_approve_promotes_sideways_and_replays_idempotently() {
    let fx = Fixture::new("pr-approve").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(22);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "22000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let unique = b"approved reference";
    let (_, cp, digest) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "22000000-0000-4000-8000-000000000002",
        child(g, vec![file("SKILL.md", b"v0"), file("NEW.md", unique)]),
        gn(1, 1),
    )
    .await;

    // Approve promotes sideways: current advances (1,1)->(1,2), signed; the candidate becomes `current`.
    let r = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "22000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert!(r.is_ok());
    assert_eq!(r.current, Some(gn(1, 2)));
    assert!(r.signed_record.is_some());
    assert_eq!(current_commit(&fx, &w, &s).await, cp);

    // The handoff: the once-proposal-only object is now TRUNK-rooted (commit_object) — survives GC, stays read.
    let x = object_id(unique);
    assert_eq!(gc::run_gc(&fx.authority, &w, NOW).await.unwrap(), 0);
    assert!(
        fx.authority
            .read_object(&prin("p_author"), &w, &s, x)
            .await
            .is_ok()
    );

    // A same-op_id replay returns the byte-identical receipt (no second promote).
    let replay = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "22000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(replay, r);
    assert_eq!(current_commit(&fx, &w, &s).await, cp);
}

#[tokio::test]
async fn interleaving_b_a_stale_approve_conflicts_then_rebase_and_approve_succeeds() {
    let fx = Fixture::new("pr-interleave-b").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(23);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "23000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let c0 = current_commit(&fx, &w, &s).await;
    // Propose p1 on base (1,1).
    let (_, p1, d1) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "23000000-0000-4000-8000-000000000002",
        child(c0, vec![file("a.md", b"p1")]),
        gn(1, 1),
    )
    .await;
    // A direct publish advances `current` to (1,2): p1 is now STALE.
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "23000000-0000-4000-8000-000000000003",
        child(c0, vec![file("a.md", b"maya")]),
        gn(1, 1),
    )
    .await;
    let c1 = current_commit(&fx, &w, &s).await;
    // Approve p1 at its stale base (1,1) ⇒ CONFLICT carrying the live generation — NOT a DENIED, NOT a promote.
    let conflict = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "23000000-0000-4000-8000-000000000004",
        p1,
        d1,
        gn(1, 1),
    )
    .await;
    assert_eq!(conflict.outcome, TerminalOutcome::Conflict);
    assert_eq!(conflict.current, Some(gn(1, 2)));
    assert_eq!(current_commit(&fx, &w, &s).await, c1);

    // Rebase: propose p2 on the NEW tip (base (1,2)); approve p2 ⇒ OK (current -> (1,3)).
    let (_, p2, d2) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "23000000-0000-4000-8000-000000000005",
        child(c1, vec![file("a.md", b"p1-rebased")]),
        gn(1, 2),
    )
    .await;
    let ok = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "23000000-0000-4000-8000-000000000006",
        p2,
        d2,
        gn(1, 2),
    )
    .await;
    assert!(ok.is_ok());
    assert_eq!(ok.current, Some(gn(1, 3)));
    assert_eq!(current_commit(&fx, &w, &s).await, p2);
}

#[tokio::test]
async fn interleaving_c_aba_a_stale_approve_conflicts_even_when_the_live_tree_matches_the_base() {
    // …X(beta)->Y(gamma); revert --to X makes current.tree == X.tree == the proposal's base tree, yet the
    // generation advanced. A late approve at the stale base must CONFLICT — a digest-only CAS would wrongly
    // accept (current.tree == base.tree); the whole-(epoch,seq) CAS catches it.
    let fx = Fixture::new("pr-interleave-c").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(24);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    // X (beta) at (1,1).
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "24000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"beta")]),
        gn(0, 0),
    )
    .await;
    let x = current_commit(&fx, &w, &s).await;
    // Propose Q on base X (1,1).
    let (_, q, dq) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "24000000-0000-4000-8000-000000000002",
        child(x, vec![file("a.md", b"q-change")]),
        gn(1, 1),
    )
    .await;
    // Publish Y (gamma) -> (1,2).
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "24000000-0000-4000-8000-000000000003",
        child(x, vec![file("a.md", b"gamma")]),
        gn(1, 1),
    )
    .await;
    // Revert --to X -> R(tree=beta, parents=[Y]) -> (1,3). Now current.tree == beta == Q's base tree.
    let rop = op("24000000-0000-4000-8000-000000000004");
    let rsig = sign_revert(&fx, &key, "dk", &w, &s, x, &rop, gn(1, 2)).await;
    let rdev = DeviceSignedOp {
        device_key_id: "dk".to_owned(),
        op: DeviceOp::Revert,
        signature: rsig,
        expected: gn(1, 2),
    };
    let rev = fx
        .authority
        .revert(
            &w,
            &s,
            x,
            rdev,
            "d_test",
            "topos revert",
            &rop,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(rev.current, Some(gn(1, 3)));

    // Approve Q at its stale base (1,1) ⇒ CONFLICT (live (1,3)), even though the live tree now matches beta.
    let conflict = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "24000000-0000-4000-8000-000000000005",
        q,
        dq,
        gn(1, 1),
    )
    .await;
    assert_eq!(conflict.outcome, TerminalOutcome::Conflict);
    assert_eq!(conflict.current, Some(gn(1, 3)));
}

#[tokio::test]
async fn approving_an_already_accepted_proposal_conflicts_and_never_promotes_twice() {
    let fx = Fixture::new("pr-double-approve").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(25);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "25000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let (_, cp, digest) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "25000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    let ok = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "25000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert!(ok.is_ok());
    // A DIFFERENT op_id approving the already-accepted (Cp, base) ⇒ typed CONFLICT (current moved), no 2nd promote.
    let again = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "25000000-0000-4000-8000-000000000004",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(again.outcome, TerminalOutcome::Conflict);
    assert_eq!(current_commit(&fx, &w, &s).await, cp);
}

#[tokio::test]
async fn four_eyes_blocks_self_approve_only_under_review_required() {
    let fx = Fixture::new("pr-4eyes-on").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let author = dev_key(26);
    let reviewer = dev_key(27);
    register(&fx, &w, &s, "dk_author", &author, "p_author").await;
    register(&fx, &w, &s, "dk_reviewer", &reviewer, "p_reviewer").await;
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();
    // Genesis (a genesis publish bypasses the gate — someone must create the first version).
    publish(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        "26000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let (_, cp, digest) = do_propose(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        "26000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    // The proposer self-approving under review_required ⇒ DENIED (four-eyes); `current` unmoved.
    let denied = do_approve(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        "26000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(denied.outcome, TerminalOutcome::Denied);
    assert_eq!(current_commit(&fx, &w, &s).await, g);
    // A SECOND actor approves ⇒ OK.
    let ok = do_approve(
        &fx,
        &reviewer,
        "dk_reviewer",
        &w,
        &s,
        "26000000-0000-4000-8000-000000000004",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert!(ok.is_ok());
}

#[tokio::test]
async fn a_solo_author_may_self_approve_when_review_required_is_off() {
    let fx = Fixture::new("pr-4eyes-off").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let author = dev_key(28);
    register(&fx, &w, &s, "dk", &author, "p_author").await;
    // review_required is OFF (the default) — a deferred self-publish is allowed.
    publish(
        &fx,
        &author,
        "dk",
        &w,
        &s,
        "28000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let (_, cp, digest) = do_propose(
        &fx,
        &author,
        "dk",
        &w,
        &s,
        "28000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    let ok = do_approve(
        &fx,
        &author,
        "dk",
        &w,
        &s,
        "28000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert!(
        ok.is_ok(),
        "self-approve is allowed with review_required off"
    );
    assert_eq!(current_commit(&fx, &w, &s).await, cp);
}

#[tokio::test]
async fn a_staled_then_gc_reclaimed_proposal_approve_conflicts_not_integrity() {
    // After a proposal stales AND GC reclaims its unique bytes, a late approve must be a clean CONFLICT — the
    // pre-transaction render fault is classified as stale (current moved), never surfaced as a corruption alarm.
    let fx = Fixture::new("pr-stale-approve").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(29);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "29000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let (_, cp, digest) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "29000000-0000-4000-8000-000000000002",
        child(
            g,
            vec![file("a.md", b"v0"), file("NEW.md", b"unique-proposed")],
        ),
        gn(1, 1),
    )
    .await;
    // Stale it, then GC reclaims its unique object.
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "29000000-0000-4000-8000-000000000003",
        child(g, vec![file("a.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(gc::run_gc(&fx.authority, &w, NOW).await.unwrap(), 1);
    // The approve at the stale base ⇒ CONFLICT (Ok value), NOT an Integrity error.
    let conflict = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "29000000-0000-4000-8000-000000000004",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(conflict.outcome, TerminalOutcome::Conflict);
}

#[tokio::test]
async fn reject_flips_open_to_rejected_and_the_unique_object_reclaims() {
    let fx = Fixture::new("pr-reject").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(40);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "40000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let unique = b"rejected-only bytes";
    let (_, cp, digest) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "40000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"v0"), file("NEW.md", unique)]),
        gn(1, 1),
    )
    .await;
    let x = object_id(unique);
    assert!(
        fx.authority
            .read_object(&prin("p_author"), &w, &s, x)
            .await
            .is_ok()
    );

    // Reject ⇒ OK (a reject success carries no pointer data); `current` untouched, nothing signed.
    let r = do_reject(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "40000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::Ok);
    assert!(r.signed_record.is_none());
    assert_eq!(current_commit(&fx, &w, &s).await, g);

    // The rejected proposal's unique object is no longer readable and GC reclaims it.
    assert!(matches!(
        fx.authority.read_object(&prin("p_author"), &w, &s, x).await,
        Err(AuthorityError::NotFound)
    ));
    assert_eq!(gc::run_gc(&fx.authority, &w, NOW).await.unwrap(), 1);
    assert_eq!(
        fx.authority.db().object_status(&w, x).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[tokio::test]
async fn rejecting_an_already_rejected_proposal_is_idempotent_and_approve_after_reject_is_typed() {
    let fx = Fixture::new("pr-reject-idem").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(41);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let (_, cp, digest) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    do_reject(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    // A reject under a NEW op_id of the already-rejected proposal ⇒ idempotent OK.
    let again = do_reject(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000004",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(again.outcome, TerminalOutcome::Ok);
    // And an approve after a reject ⇒ typed DENIED (no open proposal, base still fresh), never a promote.
    let approve = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000005",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(approve.outcome, TerminalOutcome::Denied);
    assert_eq!(current_commit(&fx, &w, &s).await, g);
}

#[tokio::test]
async fn an_unrostered_principal_cannot_reject() {
    let fx = Fixture::new("pr-reject-authz").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let author = dev_key(42);
    let stranger = dev_key(43);
    register(&fx, &w, &s, "dk_author", &author, "p_author").await;
    // The stranger's device is registered but NOT rostered for the skill.
    fx.authority
        .db()
        .seed_device(
            &w,
            "dk_stranger",
            &stranger.verifying_key().to_bytes(),
            &prin("p_stranger"),
            false,
        )
        .await
        .unwrap();
    publish(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        "42000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let (_, cp, digest) = do_propose(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        "42000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    // The unrostered stranger's reject ⇒ DENIED; the proposal stays open (its object still readable).
    let denied = do_reject(
        &fx,
        &stranger,
        "dk_stranger",
        &w,
        &s,
        "42000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(denied.outcome, TerminalOutcome::Denied);
    // The author (rostered) can still approve it (it was never rejected).
    let ok = do_approve(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        "42000000-0000-4000-8000-000000000004",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert!(ok.is_ok());
}

#[tokio::test]
async fn the_review_required_loop_direct_is_approval_required_propose_needs_review_approve_ok() {
    // Under review_required a DIRECT publish is APPROVAL_REQUIRED (the dead-end), an explicit --propose is
    // NEEDS_REVIEW (the remedy), and a second-actor approve promotes — never confusing the two outcomes.
    let fx = Fixture::new("pr-rr-loop").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let author = dev_key(44);
    let reviewer = dev_key(45);
    register(&fx, &w, &s, "dk_author", &author, "p_author").await;
    register(&fx, &w, &s, "dk_reviewer", &reviewer, "p_reviewer").await;
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();
    // Genesis bypasses the gate.
    publish(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        "44000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    // A non-genesis DIRECT publish ⇒ APPROVAL_REQUIRED (the gate; uploads nothing readable, current unmoved).
    let (staged, device) = prepare(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "44000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"direct")]),
        gn(1, 1),
    )
    .await;
    let direct =
        crate::set_current::publish(&fx.authority, &w, &s, &staged, &device, CREATED_AT, NOW)
            .await
            .unwrap();
    assert_eq!(direct.outcome, TerminalOutcome::ApprovalRequired);
    assert_eq!(current_commit(&fx, &w, &s).await, g);
    // The remedy: an explicit --propose ⇒ NEEDS_REVIEW.
    let (p, cp, digest) = do_propose(
        &fx,
        &author,
        "dk_author",
        &w,
        &s,
        "44000000-0000-4000-8000-000000000003",
        child(g, vec![file("a.md", b"proposed")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(p.outcome, TerminalOutcome::NeedsReview);
    // A second actor approves ⇒ OK.
    let ok = do_approve(
        &fx,
        &reviewer,
        "dk_reviewer",
        &w,
        &s,
        "44000000-0000-4000-8000-000000000004",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert!(ok.is_ok());
    assert_eq!(ok.current, Some(gn(1, 2)));
}

#[tokio::test]
async fn the_proposals_table_rejects_out_of_range_generations() {
    // SF-4: the safe-integer CHECK pins every stored (epoch, seq) to the JCS ceiling a follower could verify.
    let fx = Fixture::new("pr-safeint").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let over = fx
        .authority
        .db()
        .seed_proposal(
            &w,
            "p-overflow",
            &s,
            CommitId([0xC0; 32]),
            CommitId([0xB0; 32]),
            i64::MAX,
            1,
            "open",
            &prin("p_author"),
        )
        .await;
    assert!(
        over.is_err(),
        "an out-of-range base_epoch must violate the CHECK"
    );
}

#[tokio::test]
async fn a_publish_by_an_unrostered_principal_is_denied_and_records_nothing_readable() {
    // The pointer-move's in-transaction authorization (the roster check) replaces the retired upload's
    // roster gate: a registered-but-unrostered device migrates its candidate but cannot promote it, and
    // records no commit_object — so the object is unreadable.
    let fx = Fixture::new("authz-unrostered").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(50);
    fx.authority
        .db()
        .seed_device(
            &w,
            "dk",
            &key.verifying_key().to_bytes(),
            &prin("p_stranger"),
            false,
        )
        .await
        .unwrap();
    let body = b"injected";
    let (staged, device) = prepare(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "50000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", body)]),
        gn(0, 0),
    )
    .await;
    let r = crate::set_current::publish(&fx.authority, &w, &s, &staged, &device, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    fx.authority
        .db()
        .seed_roster(&w, &s, &prin("p_reader"))
        .await
        .unwrap();
    assert!(matches!(
        fx.authority
            .read_object(&prin("p_reader"), &w, &s, object_id(body))
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn a_publish_cannot_adopt_another_skills_commit() {
    // The cross-skill adoption guard, in the SHARED write body (so it covers publish / propose / approve
    // alike): a content-addressed commit belongs to exactly one skill, so re-creating its identical bytes
    // under another skill is refused — even by a principal rostered for both.
    let fx = Fixture::new("authz-xskill").await;
    let (w, x, y) = (ws("w_acme"), skill("s_x"), skill("s_y"));
    let key = dev_key(51);
    register(&fx, &w, &x, "dk", &key, "p_dev").await;
    register(&fx, &w, &y, "dk", &key, "p_dev").await;
    // X creates genesis commit C (owned by X).
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &x,
        "51000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"shared")]),
        gn(0, 0),
    )
    .await;
    // Y migrates the IDENTICAL bytes → the same commit C; promoting it under Y is denied (it is X's commit).
    let (staged, device) = prepare(
        &fx,
        &key,
        "dk",
        &w,
        &y,
        DeviceOp::PublishDirect,
        "51000000-0000-4000-8000-000000000002",
        genesis(vec![file("a.md", b"shared")]),
        gn(0, 0),
    )
    .await;
    let r = crate::set_current::publish(&fx.authority, &w, &y, &staged, &device, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
}

#[tokio::test]
async fn approve_after_reject_then_gc_is_denied_not_integrity() {
    // After a proposal is rejected AND a GC reclaims its now-unrooted unique bytes — while `current` is still
    // at the base (reject moves no pointer) — an approve's pre-transaction render faults over the missing
    // bytes. It must NOT surface as Integrity (a 500 / no receipt): the proposal is no longer open, so the
    // bytes were LEGITIMATELY reclaimed, and the transaction must produce a typed, receipted DENIED.
    let fx = Fixture::new("pr-reject-gc-approve").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(46);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "46000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let unique = b"reject-then-gc bytes";
    let (_, cp, digest) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "46000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"v0"), file("NEW.md", unique)]),
        gn(1, 1),
    )
    .await;
    do_reject(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "46000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    // The rejected proposal's unique object is now unrooted — GC reclaims it (current still at the base).
    assert_eq!(gc::run_gc(&fx.authority, &w, NOW).await.unwrap(), 1);
    // The approve renders a now-missing object, but the proposal is no longer open ⇒ a typed DENIED, not Integrity.
    let r = do_approve(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "46000000-0000-4000-8000-000000000004",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(current_commit(&fx, &w, &s).await, g);
}

#[tokio::test]
async fn revert_to_a_proposal_commit_is_refused_so_it_cannot_bypass_review() {
    // A proposal commit carries a `skill_commit` provenance row (so its digest resolves) but NO `commit_object`
    // root — it is not an accepted version. Reverting to it would forward-promote its un-reviewed tree past the
    // review gate + four-eyes (revert bypasses both). The accepted-trunk gate must refuse it, leaving `current`
    // unmoved and never serving the proposal's bytes.
    let fx = Fixture::new("pr-revert-proposal").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(52);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "52000000-0000-4000-8000-000000000001",
        genesis(vec![file("a.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    // Propose un-reviewed bytes (never accepted).
    let (_, cp, _digest) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "52000000-0000-4000-8000-000000000002",
        child(g, vec![file("a.md", b"un-reviewed")]),
        gn(1, 1),
    )
    .await;
    // revert --to <the proposal commit> ⇒ PERMANENT_FAILURE; `current` must NOT advance to the proposal's tree.
    let rop = op("52000000-0000-4000-8000-000000000003");
    let rsig = sign_revert(&fx, &key, "dk", &w, &s, cp, &rop, gn(1, 1)).await;
    let rdev = DeviceSignedOp {
        device_key_id: "dk".to_owned(),
        op: DeviceOp::Revert,
        signature: rsig,
        expected: gn(1, 1),
    };
    let r = fx
        .authority
        .revert(
            &w,
            &s,
            cp,
            rdev,
            "d_test",
            "topos revert",
            &rop,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::PermanentFailure);
    assert_eq!(
        current_commit(&fx, &w, &s).await,
        g,
        "current must stay at genesis, never the un-reviewed proposal tree"
    );
}

// ===== The authenticated read surface (read-token resolver + the bound reads) =====

#[tokio::test]
async fn resolve_read_token_resolves_a_scope_and_a_miss_is_notfound() {
    let fx = Fixture::new("rt-token").await;
    let a = &fx.authority;
    let (w, s, p) = (ws("w_acme"), skill("s_pr"), prin("dev_read"));
    a.db()
        .seed_read_token(&w, &s, &p, "tok-secret-123")
        .await
        .unwrap();

    // A known token resolves to its exact (workspace, skill, principal) scope.
    let scope = a.resolve_read_token("tok-secret-123", 0).await.unwrap();
    assert_eq!(scope.ws().as_str(), "w_acme");
    assert_eq!(scope.skill().as_str(), "s_pr");
    assert_eq!(scope.principal().as_str(), "dev_read");

    // An unknown token is the single indistinguishable not-found (a caller cannot probe which tokens exist).
    assert!(matches!(
        a.resolve_read_token("tok-WRONG", 0).await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn read_current_present_absent_and_corrupt_blob() {
    let fx = Fixture::new("rt-readcur").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(40);
    register(&fx, &w, &s, "dk", &key, "p_dev").await;
    fx.authority
        .db()
        .seed_read_token(&w, &s, &prin("p_dev"), "tok-cur")
        .await
        .unwrap();
    let scope = fx.authority.resolve_read_token("tok-cur", 0).await.unwrap();

    // Absent: no pointer has moved yet → None.
    assert!(fx.authority.read_current(&scope).await.unwrap().is_none());

    // Present: a genesis publish signs a record at (1,1); read_current extracts the generation + the raw bytes.
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "40000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let cp = fx
        .authority
        .read_current(&scope)
        .await
        .unwrap()
        .expect("present");
    assert_eq!(cp.generation, gn(1, 1));
    assert_eq!(
        cp.signed_record,
        fx.authority
            .read_signed_record(&w, &s)
            .await
            .unwrap()
            .unwrap(),
        "read_current serves exactly the stored signed record bytes"
    );

    // Corrupt: an unparseable stored record blob is an Integrity fault, NEVER a not-found (the record exists).
    fx.authority
        .db()
        .force_signed_record(&w, &s, b"{ not json")
        .await
        .unwrap();
    assert!(matches!(
        fx.authority.read_current(&scope).await,
        Err(AuthorityError::Integrity(_))
    ));
}

#[tokio::test]
async fn serve_object_serves_in_scope_and_rejects_a_scope_or_path_mismatch() {
    let fx = Fixture::new("rt-serve").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_read");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    let body = b"served bytes";
    stage_committed(a, &w, &s, "serve", vec![file("SKILL.md", body)]).await;
    a.db()
        .seed_read_token(&w, &s, &p, "tok-serve")
        .await
        .unwrap();
    let scope = a.resolve_read_token("tok-serve", 0).await.unwrap();
    let oid_hex = digest::to_hex(&object_id(body).0);

    // Happy: the scope's (ws, skill) matches the path's → the bytes.
    assert_eq!(
        a.serve_object(&scope, "w_acme", "s_pr", &oid_hex)
            .await
            .unwrap(),
        body
    );
    // A path whose skill differs from the scope's → the indistinguishable not-found (bound to one skill).
    assert!(matches!(
        a.serve_object(&scope, "w_acme", "s_OTHER", &oid_hex).await,
        Err(AuthorityError::NotFound)
    ));
    // …and a path whose workspace differs.
    assert!(matches!(
        a.serve_object(&scope, "w_OTHER", "s_pr", &oid_hex).await,
        Err(AuthorityError::NotFound)
    ));
    // A malformed (non-hex) object id is the uniform not-found too, never a distinct error from here.
    assert!(matches!(
        a.serve_object(&scope, "w_acme", "s_pr", "not-a-valid-hex-id")
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn read_version_metadata_accepted_proposal_arm_and_unauthorized() {
    let fx = Fixture::new("rt-vmeta").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(41);
    register(&fx, &w, &s, "dk", &key, "p_author").await;

    // A genesis publish: the REAL pointer-move records skill_commit WITH its digest + the commit_object edges
    // (the accepted-trunk root) + the bytes — the readable state the trunk arm authorizes over.
    let body = b"# skill\nrun it\n";
    let pub_receipt = publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", body)]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    // The enriched OK receipt carries the server-rehashed version id + digest.
    assert_eq!(pub_receipt.version_id, Some(g));
    let g_digest = pub_receipt
        .bundle_digest
        .expect("an OK receipt carries the digest");

    // A rostered reader's scope (version-read authz is roster-based — no device needed).
    let reader = prin("p_reader");
    fx.authority
        .db()
        .seed_roster(&w, &s, &reader)
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_read_token(&w, &s, &reader, "tok-vm")
        .await
        .unwrap();
    let scope = fx.authority.resolve_read_token("tok-vm", 0).await.unwrap();
    let g_hex = digest::to_hex(&g.0);

    // rostered + accepted → ok: exact id, the complete (empty, genesis) parent set, the file leaf, the digest.
    let meta = fx
        .authority
        .read_version_metadata(&scope, "w_acme", "s_deploy", &g_hex)
        .await
        .unwrap();
    assert_eq!(meta.version_id, g.0);
    assert!(meta.parents.is_empty());
    assert_eq!(meta.bundle_digest, g_digest);
    assert_eq!(meta.files.len(), 1);
    assert_eq!(meta.files[0].path, "SKILL.md");
    assert_eq!(meta.files[0].object_id, object_id(body).0);

    // non-rostered → NotFound: a token for a principal with NO roster row resolves, but the version read is
    // the indistinguishable not-found (never a 403).
    fx.authority
        .db()
        .seed_read_token(&w, &s, &prin("p_outsider"), "tok-out")
        .await
        .unwrap();
    let outscope = fx.authority.resolve_read_token("tok-out", 0).await.unwrap();
    assert!(matches!(
        fx.authority
            .read_version_metadata(&outscope, "w_acme", "s_deploy", &g_hex)
            .await,
        Err(AuthorityError::NotFound)
    ));

    // An OPEN, non-stale proposal's version IS readable (the proposal arm): do_propose records the candidate's
    // skill_commit + digest, migrates its bytes, and opens the proposal at base (1,1) (current is unmoved).
    let (_r, cp, _d) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000002",
        child(
            g,
            vec![
                file("SKILL.md", body),
                file("NEW.md", b"a new reference doc"),
            ],
        ),
        gn(1, 1),
    )
    .await;
    let cp_hex = digest::to_hex(&cp.0);
    let pmeta = fx
        .authority
        .read_version_metadata(&scope, "w_acme", "s_deploy", &cp_hex)
        .await
        .unwrap();
    assert_eq!(pmeta.parents, vec![g.0]);
    assert_eq!(pmeta.files.len(), 2);

    // Stale it: a direct publish advances `current` past the proposal's base (1,1). The proposal is now stale
    // (open, but base != current), so its version reads NotFound — proving we NEVER authorize on bare
    // skill_commit (the candidate's provenance row, with its digest, still exists).
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000003",
        child(g, vec![file("SKILL.md", b"a different v2")]),
        gn(1, 1),
    )
    .await;
    assert!(matches!(
        fx.authority
            .read_version_metadata(&scope, "w_acme", "s_deploy", &cp_hex)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

// ══════════════════════════════════════════════════════════════════════════════════════════════════════
// Enrollment + governance issuance — the device-flow → grant → redeem distribution path, the possession
// proof's teeth, the deterministic-credential idempotency, the cloud/self-host roster gate, instant device
// revoke, the governance role matrix, and server-derived device ids. Behaviors, never roadmap tags.
// ══════════════════════════════════════════════════════════════════════════════════════════════════════
mod enrollment_and_governance {
    use ed25519_dalek::{Signer as _, SigningKey};
    use topos_core::sign::{
        EnrollFields, GovernanceOpFields, GovernanceOpKind, enroll_preimage, governance_op_preimage,
    };

    use super::*;
    use crate::enroll::device_key_id_for;
    use crate::{
        CreateInviteOutcome, DeviceAuthPoll, GovernanceOp, GovernanceOutcome, GovernanceSignedOp,
        GrantIssued, PasscodeComplete, RedeemOutcome, Role,
    };

    const NOW: i64 = 1_000;

    /// A canonical lowercase-hyphenated UUID op id seeded by `n`.
    fn op_id(n: u64) -> String {
        format!("00000000-0000-4000-8000-{n:012x}")
    }

    /// The raw Ed25519 public key for a seed.
    fn device_pub(seed: &[u8; 32]) -> [u8; 32] {
        SigningKey::from_bytes(seed).verifying_key().to_bytes()
    }

    /// Pull the opaque token out of a `/i/<token>` link.
    fn token_of(link: &str) -> String {
        link.rsplit('/').next().expect("a link tail").to_owned()
    }

    /// Sign a governance op the way an owner's device would (rebuild the kernel frame, sign the preimage).
    fn sign_governance(
        owner_seed: &[u8; 32],
        ws: &str,
        op_id: &str,
        device_key_id: &str,
        op: GovernanceOp,
    ) -> GovernanceSignedOp {
        let op_id_bytes = uuid::Uuid::parse_str(op_id)
            .expect("canonical uuid")
            .into_bytes();
        let signature = {
            let emails: Vec<&str>;
            let skills: Vec<&str>;
            let kind = match &op {
                GovernanceOp::Invite {
                    role,
                    expires_at,
                    emails: e,
                    skills: s,
                } => {
                    emails = e.iter().map(Principal::as_str).collect();
                    skills = s.iter().map(|(id, _)| id.as_str()).collect();
                    GovernanceOpKind::Invite {
                        role: role.signing_byte(),
                        expires_at: u64::try_from(expires_at.unwrap_or(0)).unwrap_or(0),
                        emails: &emails,
                        skills: &skills,
                    }
                }
                GovernanceOp::RosterSet { role, target } => GovernanceOpKind::RosterSet {
                    role: role.signing_byte(),
                    target: target.as_str(),
                },
                GovernanceOp::RosterRemove { target } => GovernanceOpKind::RosterRemove {
                    target: target.as_str(),
                },
                GovernanceOp::DeviceRevoke {
                    target_device_key_id,
                } => GovernanceOpKind::DeviceRevoke {
                    target_device_key_id: target_device_key_id.as_str(),
                },
            };
            let fields = GovernanceOpFields {
                workspace_id: ws,
                op_id: op_id_bytes,
                device_key_id,
                op: kind,
            };
            let preimage = governance_op_preimage(&fields).expect("preimage");
            SigningKey::from_bytes(owner_seed)
                .sign(&preimage)
                .to_bytes()
        };
        GovernanceSignedOp {
            device_key_id: device_key_id.to_owned(),
            op,
            signature,
        }
    }

    /// Sign an enrollment possession proof the way the enrolling device would.
    fn sign_enroll(
        device_seed: &[u8; 32],
        ws: &str,
        grant_hash: [u8; 32],
        device_auth_id: &str,
        device_key_id: &str,
        device_public_key: [u8; 32],
        offered: &[&str],
    ) -> [u8; 64] {
        let fields = EnrollFields {
            workspace_id: ws,
            grant_hash,
            device_auth_id,
            device_key_id,
            device_public_key,
            offered_skill_ids: offered,
        };
        let preimage = enroll_preimage(&fields).expect("preimage");
        SigningKey::from_bytes(device_seed)
            .sign(&preimage)
            .to_bytes()
    }

    /// Seat an owner: a workspace row, an `owner`/`confirmed` member, and the owner's registered device.
    /// Returns `(owner_seed, owner_principal, owner_device_key_id)`.
    async fn seat_owner(
        a: &Authority,
        w: &WorkspaceId,
        mode: &str,
    ) -> ([u8; 32], Principal, String) {
        a.db()
            .seed_workspace(w, "Acme", "verified", mode)
            .await
            .unwrap();
        let owner_seed = [7u8; 32];
        let owner_pub = device_pub(&owner_seed);
        let owner_dk = device_key_id_for(&owner_pub);
        let owner = prin("owner@acme.com");
        a.db()
            .seed_workspace_member(w, &owner, "owner", "confirmed")
            .await
            .unwrap();
        a.db()
            .seed_device(w, &owner_dk, &owner_pub, &owner, false)
            .await
            .unwrap();
        (owner_seed, owner, owner_dk)
    }

    /// Owner-create an invite offering `skill` to `invitee`; return its opaque token.
    async fn make_invite(
        a: &Authority,
        w: &WorkspaceId,
        owner_seed: &[u8; 32],
        owner_dk: &str,
        op: &str,
        invitee: &str,
        skill_name: &str,
    ) -> String {
        let signed = sign_governance(
            owner_seed,
            w.as_str(),
            op,
            owner_dk,
            GovernanceOp::Invite {
                role: Role::Member,
                expires_at: None,
                emails: vec![prin(invitee)],
                skills: vec![(skill(skill_name), Some("Deploy".to_owned()))],
            },
        );
        match a.create_invite(w, op, signed, "t0").await.unwrap() {
            CreateInviteOutcome::Created(c) => token_of(&c.link),
            other => panic!("expected Created, got {other:?}"),
        }
    }

    /// Drive a CLOUD device flow to a grant: start → poll(Pending) → passcode → poll(Granted). `confirm_as`
    /// is the email proven on the verification page (the grant's principal).
    async fn cloud_flow_to_grant(
        a: &Authority,
        invite_token: &str,
        device_seed: &[u8; 32],
        confirm_as: &str,
    ) -> GrantIssued {
        let dpub = device_pub(device_seed);
        let start = a
            .start_device_auth(invite_token, &dpub, "laptop", NOW, "t0")
            .await
            .unwrap();
        assert!(matches!(
            a.poll_device_auth(&start.device_code, NOW, "t0")
                .await
                .unwrap(),
            DeviceAuthPoll::Pending
        ));
        let pc = a
            .start_passcode(&start.user_code, confirm_as, NOW, "t0")
            .await
            .unwrap();
        assert_eq!(
            a.complete_passcode(&start.user_code, confirm_as, &pc.passcode, NOW)
                .await
                .unwrap(),
            PasscodeComplete::Confirmed
        );
        match a
            .poll_device_auth(&start.device_code, NOW, "t0")
            .await
            .unwrap()
        {
            DeviceAuthPoll::Granted(g) => g,
            other => panic!("expected Granted, got {other:?}"),
        }
    }

    /// Redeem a grant with the (honest) enrolling device.
    async fn redeem(
        a: &Authority,
        grant: &GrantIssued,
        device_seed: &[u8; 32],
        dpub: [u8; 32],
    ) -> RedeemOutcome {
        let grant_hash = digest::sha256(grant.grant_token.as_bytes());
        let offered: Vec<&str> = grant.offered_skills.iter().map(SkillId::as_str).collect();
        let sig = sign_enroll(
            device_seed,
            grant.workspace_id.as_str(),
            grant_hash,
            &grant.device_auth_id,
            &grant.device_key_id,
            dpub,
            &offered,
        );
        a.redeem_enrollment(&grant.grant_token, &sig, dpub, NOW, "t0")
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn cloud_device_flow_to_redeem_mints_a_resolvable_read_token() {
        let fx = Fixture::new("enr-happy").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (owner_seed, _owner, owner_dk) = seat_owner(a, &w, "cloud").await;
        let invite = make_invite(
            a,
            &w,
            &owner_seed,
            &owner_dk,
            &op_id(1),
            "alice@acme.com",
            "s_deploy",
        )
        .await;

        let device_seed = [11u8; 32];
        let dpub = device_pub(&device_seed);
        let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;
        assert_eq!(grant.device_key_id, device_key_id_for(&dpub)); // server-derived

        let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
            panic!("expected a redeem");
        };
        assert_eq!(r.principal.as_str(), "alice@acme.com");
        assert_eq!(r.device_key_id, device_key_id_for(&dpub));
        assert_eq!(r.read_tokens.len(), 1);
        assert_eq!(r.read_tokens[0].skill_id.as_str(), "s_deploy");
        // The minted read token resolves to exactly the (ws, skill) scope.
        let scope = a
            .resolve_read_token(&r.read_tokens[0].token, NOW)
            .await
            .unwrap();
        assert_eq!(scope.ws().as_str(), "w_acme");
        assert_eq!(scope.skill().as_str(), "s_deploy");
    }

    #[tokio::test]
    async fn a_leaked_grant_redeemed_by_a_different_device_is_denied() {
        let fx = Fixture::new("enr-leak").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
        let invite = make_invite(
            a,
            &w,
            &owner_seed,
            &owner_dk,
            &op_id(1),
            "alice@acme.com",
            "s_deploy",
        )
        .await;
        let device_seed = [11u8; 32];
        let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;

        // An attacker who stole the grant token but holds a DIFFERENT key cannot redeem it.
        let attacker_seed = [99u8; 32];
        let attacker_pub = device_pub(&attacker_seed);
        let attacker_dk = device_key_id_for(&attacker_pub);
        let grant_hash = digest::sha256(grant.grant_token.as_bytes());
        let offered: Vec<&str> = grant.offered_skills.iter().map(SkillId::as_str).collect();
        let sig = sign_enroll(
            &attacker_seed,
            grant.workspace_id.as_str(),
            grant_hash,
            &grant.device_auth_id,
            &attacker_dk,
            attacker_pub,
            &offered,
        );
        let out = a
            .redeem_enrollment(&grant.grant_token, &sig, attacker_pub, NOW, "t0")
            .await
            .unwrap();
        assert!(matches!(out, RedeemOutcome::Denied(_)), "got {out:?}");
    }

    #[tokio::test]
    async fn redeem_replay_re_derives_identical_read_tokens() {
        let fx = Fixture::new("enr-replay").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
        let invite = make_invite(
            a,
            &w,
            &owner_seed,
            &owner_dk,
            &op_id(1),
            "alice@acme.com",
            "s_deploy",
        )
        .await;
        let device_seed = [11u8; 32];
        let dpub = device_pub(&device_seed);
        let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;

        let RedeemOutcome::Redeemed(r1) = redeem(a, &grant, &device_seed, dpub).await else {
            panic!("first redeem");
        };
        let RedeemOutcome::Redeemed(r2) = redeem(a, &grant, &device_seed, dpub).await else {
            panic!("replay redeem");
        };
        // Deterministic: the replay re-derives the IDENTICAL token (the same content-id PK row, no fresh mint).
        assert_eq!(r1.read_tokens.len(), 1);
        assert_eq!(r1.read_tokens[0].token, r2.read_tokens[0].token);
        // Both resolve (the row is the same one, REPLACED in place).
        assert!(
            a.resolve_read_token(&r2.read_tokens[0].token, NOW)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn cloud_redeem_of_a_non_rostered_principal_is_denied() {
        let fx = Fixture::new("enr-gate").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
        // The invite seeds ALICE onto the roster…
        let invite = make_invite(
            a,
            &w,
            &owner_seed,
            &owner_dk,
            &op_id(1),
            "alice@acme.com",
            "s_deploy",
        )
        .await;
        let device_seed = [11u8; 32];
        let dpub = device_pub(&device_seed);
        // …but the device proves BOB (not on the roster) on the verification page.
        let grant = cloud_flow_to_grant(a, &invite, &device_seed, "bob@acme.com").await;
        let out = redeem(a, &grant, &device_seed, dpub).await;
        assert!(matches!(out, RedeemOutcome::Denied(_)), "got {out:?}");
    }

    #[tokio::test]
    async fn self_host_redeem_grants_membership_without_smtp() {
        let fx = Fixture::new("enr-selfhost").await;
        let a = &fx.authority;
        let w = ws("w_local");
        let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "self_host").await;
        let invite = make_invite(
            a,
            &w,
            &owner_seed,
            &owner_dk,
            &op_id(1),
            "owner@acme.com",
            "s_deploy",
        )
        .await;

        let device_seed = [11u8; 32];
        let dpub = device_pub(&device_seed);
        // Self-host: the session is born confirmed (device-rooted principal); the first poll yields a grant.
        let start = a
            .start_device_auth(&invite, &dpub, "laptop", NOW, "t0")
            .await
            .unwrap();
        let grant = match a
            .poll_device_auth(&start.device_code, NOW, "t0")
            .await
            .unwrap()
        {
            DeviceAuthPoll::Granted(g) => g,
            other => panic!("expected Granted (no human step), got {other:?}"),
        };
        let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
            panic!("self-host redeem");
        };
        assert!(
            r.principal.as_str().starts_with("dev."),
            "device-rooted principal"
        );
        assert_eq!(r.read_tokens.len(), 1);
        assert!(
            a.resolve_read_token(&r.read_tokens[0].token, NOW)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn revoke_device_404s_read_tokens_and_refuses_later_device_ops() {
        let fx = Fixture::new("enr-revoke").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
        let invite = make_invite(
            a,
            &w,
            &owner_seed,
            &owner_dk,
            &op_id(1),
            "alice@acme.com",
            "s_deploy",
        )
        .await;
        let device_seed = [11u8; 32];
        let dpub = device_pub(&device_seed);
        let alice_dk = device_key_id_for(&dpub);
        let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;
        let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
            panic!("redeem");
        };
        let alice_token = r.read_tokens[0].token.clone();
        assert!(a.resolve_read_token(&alice_token, NOW).await.is_ok());

        // The owner revokes ALICE's device → her read token is purged (instant 404).
        let revoke = sign_governance(
            &owner_seed,
            w.as_str(),
            &op_id(2),
            &owner_dk,
            GovernanceOp::DeviceRevoke {
                target_device_key_id: alice_dk.clone(),
            },
        );
        assert_eq!(
            a.revoke_device(&w, &op_id(2), revoke, "t0").await.unwrap(),
            GovernanceOutcome::Ok
        );
        assert!(matches!(
            a.resolve_read_token(&alice_token, NOW).await,
            Err(AuthorityError::NotFound)
        ));

        // The owner self-revokes its OWN device → a subsequent device-signed governance op is refused.
        let self_revoke = sign_governance(
            &owner_seed,
            w.as_str(),
            &op_id(3),
            &owner_dk,
            GovernanceOp::DeviceRevoke {
                target_device_key_id: owner_dk.clone(),
            },
        );
        assert_eq!(
            a.revoke_device(&w, &op_id(3), self_revoke, "t0")
                .await
                .unwrap(),
            GovernanceOutcome::Ok
        );
        let after = sign_governance(
            &owner_seed,
            w.as_str(),
            &op_id(4),
            &owner_dk,
            GovernanceOp::Invite {
                role: Role::Member,
                expires_at: None,
                emails: vec![prin("carol@acme.com")],
                skills: vec![],
            },
        );
        let out = a.create_invite(&w, &op_id(4), after, "t0").await.unwrap();
        assert!(
            matches!(out, CreateInviteOutcome::Denied(_)),
            "revoked device refused: {out:?}"
        );
    }

    #[tokio::test]
    async fn a_members_governance_op_is_denied() {
        let fx = Fixture::new("enr-rolematrix").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (_owner_seed, _o, _owner_dk) = seat_owner(a, &w, "cloud").await;
        // A confirmed MEMBER (not owner) with a registered device.
        let member_seed = [22u8; 32];
        let member_pub = device_pub(&member_seed);
        let member_dk = device_key_id_for(&member_pub);
        let member = prin("mary@acme.com");
        a.db()
            .seed_workspace_member(&w, &member, "member", "confirmed")
            .await
            .unwrap();
        a.db()
            .seed_device(&w, &member_dk, &member_pub, &member, false)
            .await
            .unwrap();

        let signed = sign_governance(
            &member_seed,
            w.as_str(),
            &op_id(9),
            &member_dk,
            GovernanceOp::Invite {
                role: Role::Member,
                expires_at: None,
                emails: vec![prin("x@acme.com")],
                skills: vec![],
            },
        );
        let out = a.create_invite(&w, &op_id(9), signed, "t0").await.unwrap();
        assert!(
            matches!(out, CreateInviteOutcome::Denied(_)),
            "member denied: {out:?}"
        );
    }

    #[tokio::test]
    async fn create_invite_is_op_id_idempotent_with_an_identical_link() {
        let fx = Fixture::new("enr-idem").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
        let op = op_id(6);
        let mk = || {
            sign_governance(
                &owner_seed,
                w.as_str(),
                &op,
                &owner_dk,
                GovernanceOp::Invite {
                    role: Role::Member,
                    expires_at: None,
                    emails: vec![prin("alice@acme.com")],
                    skills: vec![(skill("s_deploy"), None)],
                },
            )
        };
        let CreateInviteOutcome::Created(c1) = a.create_invite(&w, &op, mk(), "t0").await.unwrap()
        else {
            panic!("first create");
        };
        let CreateInviteOutcome::Created(c2) = a.create_invite(&w, &op, mk(), "t0").await.unwrap()
        else {
            panic!("replay create");
        };
        assert_eq!(
            c1.link, c2.link,
            "the deterministic link replays identically"
        );
    }

    #[tokio::test]
    async fn passcode_locks_after_the_attempt_cap() {
        let fx = Fixture::new("enr-brute").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
        let invite = make_invite(
            a,
            &w,
            &owner_seed,
            &owner_dk,
            &op_id(1),
            "alice@acme.com",
            "s_deploy",
        )
        .await;
        let device_seed = [11u8; 32];
        let dpub = device_pub(&device_seed);
        let start = a
            .start_device_auth(&invite, &dpub, "laptop", NOW, "t0")
            .await
            .unwrap();
        let pc = a
            .start_passcode(&start.user_code, "alice@acme.com", NOW, "t0")
            .await
            .unwrap();
        // A guaranteed-wrong guess (differs from the real code).
        let wrong = if pc.passcode == "000000" {
            "000001"
        } else {
            "000000"
        };
        for _ in 0..5 {
            let r = a
                .complete_passcode(&start.user_code, "alice@acme.com", wrong, NOW)
                .await
                .unwrap();
            assert!(matches!(r, PasscodeComplete::WrongCode { .. }), "got {r:?}");
        }
        // The cap is now hit — further attempts (even the RIGHT code) are locked out.
        assert_eq!(
            a.complete_passcode(&start.user_code, "alice@acme.com", &pc.passcode, NOW)
                .await
                .unwrap(),
            PasscodeComplete::TooManyAttempts
        );
    }

    #[tokio::test]
    async fn device_key_id_is_server_derived_not_client_asserted() {
        let fx = Fixture::new("enr-dk").await;
        let a = &fx.authority;
        let w = ws("w_acme");
        let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
        let invite = make_invite(
            a,
            &w,
            &owner_seed,
            &owner_dk,
            &op_id(1),
            "alice@acme.com",
            "s_deploy",
        )
        .await;
        let device_seed = [11u8; 32];
        let dpub = device_pub(&device_seed);
        let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;
        // The id the server bound is purely a function of the public key.
        assert_eq!(grant.device_key_id, device_key_id_for(&dpub));
        let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
            panic!("redeem");
        };
        assert_eq!(r.device_key_id, device_key_id_for(&dpub));

        // Presenting a DIFFERENT key (whose server-derived id ≠ the grant's binding) is denied.
        let other_seed = [55u8; 32];
        let other_pub = device_pub(&other_seed);
        let grant_hash = digest::sha256(grant.grant_token.as_bytes());
        let offered: Vec<&str> = grant.offered_skills.iter().map(SkillId::as_str).collect();
        let sig = sign_enroll(
            &other_seed,
            grant.workspace_id.as_str(),
            grant_hash,
            &grant.device_auth_id,
            &device_key_id_for(&other_pub),
            other_pub,
            &offered,
        );
        let out = a
            .redeem_enrollment(&grant.grant_token, &sig, other_pub, NOW, "t0")
            .await
            .unwrap();
        assert!(matches!(out, RedeemOutcome::Denied(_)), "got {out:?}");
    }

    #[tokio::test]
    async fn admin_claim_stands_up_a_self_host_workspace_once() {
        let fx = Fixture::new("enr-admin").await;
        let a = &fx.authority;
        let w = ws("w_local");
        a.db().seed_admin_claim(&w, "claim-secret").await.unwrap();
        let device_seed = [33u8; 32];
        let dpub = device_pub(&device_seed);

        let RedeemOutcome::Redeemed(r) = a
            .admin_claim("claim-secret", dpub, "Local", NOW, "t0")
            .await
            .unwrap()
        else {
            panic!("admin claim");
        };
        assert_eq!(r.workspace_id.as_str(), "w_local");
        assert!(r.principal.as_str().starts_with("dev."));
        // The one-time token is now consumed — a second claim is denied.
        let again = a
            .admin_claim("claim-secret", dpub, "Local", NOW, "t0")
            .await
            .unwrap();
        assert!(matches!(again, RedeemOutcome::Denied(_)));
    }
}
