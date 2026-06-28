//! In-crate authority tests (the `pub(crate)` seed helper is only visible here, never to an external
//! integration crate). They exercise the access rule, cross-workspace and cross-skill isolation, the
//! upload/rehash guard, dedup-obliviousness, and the transaction discipline against a real SQLite
//! database + a real per-workspace git store.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest;

use crate::sqlite::{ClaimOutcome, InstallOutcome, ObjectStatus, RecordOutcome};
use crate::{
    Authority, AuthorityError, CandidateUpload, CommitId, FileMode, ObjectId, OpId, Principal,
    SkillId, UploadedFile, WorkspaceId, gc, lifecycle,
};

// ── fixtures + helpers ───────────────────────────────────────────────────────────────────────────

/// A temp dir + an open authority, cleaned up on drop (RAII, so a failing test still tidies).
struct Fixture {
    dir: PathBuf,
    authority: Authority,
}

impl Fixture {
    async fn new(tag: &str) -> Self {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("topos-ps-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let authority = Authority::open_sqlite(&dir.join("plane.db"), &dir.join("stores"))
            .await
            .expect("open authority");
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

// ── the access rule: a rostered reader gets the bytes (even of an unpromoted version) ──────────────

#[tokio::test]
async fn rostered_member_reads_bytes_of_an_unpromoted_version() {
    let fx = Fixture::new("read-ok").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let uploader = prin("dev_up");
    let reader = prin("dev_read");

    // Two distinct readers/uploaders are rostered for the skill.
    a.db().seed_roster(&w, &s, &uploader).await.unwrap();
    a.db().seed_roster(&w, &s, &reader).await.unwrap();

    let body = b"# PR describe\nrun the thing\n";
    let script = b"#!/bin/sh\necho hi\n";
    let receipt = a
        .upload_candidate(
            &uploader,
            &w,
            &s,
            genesis(vec![file("SKILL.md", body), file("run.sh", script)]),
        )
        .await
        .expect("upload");

    // `current` was never moved, yet a rostered member reads the candidate's bytes (the read path does
    // not consult the pointer).
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
    // The receipt's recomputed version id is exactly the kernel commit id over the uploaded bytes.
    assert_eq!(receipt.logical_bytes, (body.len() + script.len()) as u64);
}

#[tokio::test]
async fn unrostered_reader_gets_notfound_for_a_real_object() {
    let fx = Fixture::new("read-unrostered").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let uploader = prin("dev_up");
    a.db().seed_roster(&w, &s, &uploader).await.unwrap();
    let body = b"secret bytes";
    a.upload_candidate(&uploader, &w, &s, genesis(vec![file("SKILL.md", body)]))
        .await
        .unwrap();

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
    a.upload_candidate(&p, &w, &s, genesis(vec![file("SKILL.md", body)]))
        .await
        .unwrap();
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

    // Upload a real object into workspace B.
    a.db().seed_roster(&wb, &s, &p).await.unwrap();
    let secret = b"workspace B private bytes";
    a.upload_candidate(&p, &wb, &s, genesis(vec![file("SKILL.md", secret)]))
        .await
        .unwrap();

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
    a.upload_candidate(&p, &w, &y, genesis(vec![file("SKILL.md", y_bytes)]))
        .await
        .unwrap();

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

// ── upload + server rehash (the confused-deputy guard) ────────────────────────────────────────────

#[tokio::test]
async fn server_recomputes_the_version_id_so_a_one_byte_change_changes_it() {
    let fx = Fixture::new("rehash").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();

    let r1 = a
        .upload_candidate(&p, &w, &s, genesis(vec![file("SKILL.md", b"alpha")]))
        .await
        .unwrap();
    let r2 = a
        .upload_candidate(&p, &w, &s, genesis(vec![file("SKILL.md", b"alphb")]))
        .await
        .unwrap();
    assert_ne!(
        r1.version_id, r2.version_id,
        "a 1-byte change must change the version id"
    );
    assert_ne!(r1.bundle_digest, r2.bundle_digest);
}

#[tokio::test]
async fn upload_rejects_a_forbidden_path_and_records_nothing() {
    let fx = Fixture::new("reject-path").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();

    let bad = a
        .upload_candidate(&p, &w, &s, genesis(vec![file("/abs/forbidden", b"x")]))
        .await;
    assert!(matches!(bad, Err(AuthorityError::RejectedUpload(_))));
    // Nothing was recorded: a read of the object's id is not-found.
    assert!(matches!(
        a.read_object(&p, &w, &s, object_id(b"x")).await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn empty_upload_is_rejected() {
    let fx = Fixture::new("empty-upload").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    // The git store would happily snapshot a zero-entry tree; the authority must reject an empty bundle
    // itself (it cannot trust the client scanner to have done so).
    let res = a.upload_candidate(&p, &w, &s, genesis(vec![])).await;
    assert!(matches!(res, Err(AuthorityError::RejectedUpload(_))));
}

#[tokio::test]
async fn unrostered_upload_is_denied_and_records_nothing_readable() {
    let fx = Fixture::new("upload-denied").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let stranger = prin("dev_stranger");

    let body = b"injected";
    let res = a
        .upload_candidate(&stranger, &w, &s, genesis(vec![file("SKILL.md", body)]))
        .await;
    assert!(matches!(res, Err(AuthorityError::Denied)));

    // Even a (later) rostered reader cannot read it — no provenance was recorded, so the access join
    // finds nothing (the orphan git object is unreachable through the only public surface).
    a.db()
        .seed_roster(&w, &s, &prin("dev_reader"))
        .await
        .unwrap();
    assert!(matches!(
        a.read_object(&prin("dev_reader"), &w, &s, object_id(body))
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[tokio::test]
async fn dedup_hit_returns_an_identical_receipt() {
    let fx = Fixture::new("dedup").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();

    let files = || {
        vec![
            file("SKILL.md", b"same bytes"),
            file("run.sh", b"#!/bin/sh\n"),
        ]
    };
    let first = a
        .upload_candidate(&p, &w, &s, genesis(files()))
        .await
        .unwrap();
    let second = a
        .upload_candidate(&p, &w, &s, genesis(files()))
        .await
        .unwrap();
    // A re-upload of identical bytes yields a byte-identical receipt — no "already present" signal.
    assert_eq!(first, second);
}

#[tokio::test]
async fn exact_cross_skill_re_upload_is_denied() {
    let fx = Fixture::new("xskill-adopt").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (x, y) = (skill("s_x"), skill("s_y"));
    let p = prin("dev_p"); // rostered for both skills, but still can't move a commit across them.
    a.db().seed_roster(&w, &x, &p).await.unwrap();
    a.db().seed_roster(&w, &y, &p).await.unwrap();

    let cand = || genesis(vec![file("SKILL.md", b"shared content")]);
    a.upload_candidate(&p, &w, &x, cand())
        .await
        .expect("first upload to X");
    // The identical bundle yields the identical commit id; recording it under Y is refused (the
    // skill_commit primary key makes a commit belong to exactly one skill).
    let adopt = a.upload_candidate(&p, &w, &y, cand()).await;
    assert!(matches!(adopt, Err(AuthorityError::Denied)));
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

#[tokio::test]
async fn two_writers_serialize_without_a_busy_error() {
    let fx = Fixture::new("concurrent").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();

    // Two immediate-write transactions for different commits, driven concurrently. The IMMEDIATE write
    // lock + busy timeout serialize them; both must succeed (no SQLITE_BUSY).
    let c1 = CommitId([0xA1; 32]);
    let c2 = CommitId([0xB2; 32]);
    let os1 = [ObjectId([0x01; 32])];
    let os2 = [ObjectId([0x02; 32])];
    let (r1, r2) = tokio::join!(
        a.db().record_authorized_commit(&w, &s, &p, c1, &os1),
        a.db().record_authorized_commit(&w, &s, &p, c2, &os2),
    );
    assert_eq!(r1.unwrap(), RecordOutcome::Recorded);
    assert_eq!(r2.unwrap(), RecordOutcome::Recorded);
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
            .install_object(&w, o, &goid(7), 3, 100)
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
            .install_object(&w, o, &goid(7), 3, 101)
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
        .install_object(&w, o, &goid(9), 1, 100)
        .await
        .unwrap();
    // No commit_object, no lease → the guarded claim succeeds and yields the git locator.
    match a.db().claim_for_delete(&w, o, 200).await.unwrap() {
        ClaimOutcome::Claimed { git_oid } => assert_eq!(git_oid, goid(9)),
        ClaimOutcome::Spared => panic!("expected claimed"),
    }
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Deleting
    );
    a.db().finalize_delete(&w, o, 300).await.unwrap();
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
        .install_object(&w, o, &goid(1), 1, 100)
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
        .install_object(&w, o, &goid(2), 1, 100)
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
        .install_object(&w, o1, &goid(3), 1, 100)
        .await
        .unwrap();
    a.db()
        .install_object(&w, o2, &goid(4), 1, 100)
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
            .install_object(&w, o, &goid(5), 1, 200)
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
            .install_object(&w, fresh, &goid(6), 1, 110)
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
        .install_object(&w, existing, &goid(6), 1, 100)
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
    assert_eq!(first, Some(goid(8)));
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
        .install_object(&w, x, &goid(1), 1, 100)
        .await
        .unwrap();
    a.db()
        .install_object(&w, y, &goid(2), 1, 100)
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
        .install_object(&w, x, &goid(1), 1, 100)
        .await
        .unwrap();
    a.db()
        .install_object(&w, y, &goid(2), 1, 100)
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
async fn gc_acts_only_on_fenced_objects_legacy_upload_stays_readable() {
    let fx = Fixture::new("e-legacy").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    // The pre-existing straight-to-git upload path: writes a blob with NO object_presence row.
    let body = b"legacy bytes";
    a.upload_candidate(&p, &w, &s, genesis(vec![file("L.md", body)]))
        .await
        .unwrap();
    assert_eq!(
        a.read_object(&p, &w, &s, object_id(body)).await.unwrap(),
        body
    );
    // A full GC pass must not touch it (no presence row → invisible to GC).
    gc::run_gc(a, &w, 200).await.unwrap();
    assert_eq!(
        a.read_object(&p, &w, &s, object_id(body)).await.unwrap(),
        body,
        "a legacy blob with no presence row stays readable after GC"
    );
}

#[tokio::test]
async fn gc_spares_an_object_a_legacy_commit_still_references_even_if_fenced() {
    // The keep-set regression: read_object authorizes via ANY commit_object, so the fence must spare a
    // commit-referenced object even when it also carries a (now-abandoned) presence row.
    let fx = Fixture::new("e-overlap").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    let body = b"shared between legacy and fenced";
    // Legacy upload records commit_object + writes the blob (readable, no presence row).
    a.upload_candidate(&p, &w, &s, genesis(vec![file("B.md", body)]))
        .await
        .unwrap();
    // A later fenced migrate of the SAME bytes adds a presence row; then it is abandoned.
    ingest_migrate(a, &w, "op", vec![file("B.md", body)], 100).await;
    a.db().release_lease(&w, &op("op")).await.unwrap();

    // GC must SPARE it (a commit_object edge = readable); the read keeps working.
    gc::run_gc(a, &w, 200).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    assert_eq!(
        a.read_object(&p, &w, &s, object_id(body)).await.unwrap(),
        body
    );
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
    // `recovery_sweep_spares_a_deleting_object_re_rooted_by_a_legacy_edge`).
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
async fn recovery_sweep_spares_a_deleting_object_re_rooted_by_a_legacy_edge() {
    // The recovery byte-loss guard (the companion to
    // `gc_spares_an_object_a_legacy_commit_still_references_even_if_fenced`, but for the RECOVERY path, where
    // the edge arrives AFTER the claim). A crashed GC leaves a stale `deleting` row; before recovery runs, a
    // legacy `upload_candidate` of identical bytes records a `commit_object` edge with no `object_presence`
    // consult, so the object becomes read-authorized. recovery_sweep must re-verify the keep-set at delete
    // time and SPARE it, never unlink a now-readable, committed object's bytes. Fails (Integrity on the final
    // read) if `claim_stale_for_recovery` drops its keep-set guard.
    let fx = Fixture::new("e-recover-reroot").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    let body = b"shared content the recovery must not reclaim";

    // (1) Fenced migrate of `body`, then abandon -> present, unrooted (a normal GC candidate).
    ingest_migrate(a, &w, "op", vec![file("B.md", body)], 100).await;
    a.db().release_lease(&w, &op("op")).await.unwrap();
    let oid = object_id(body);

    // (2) A GC claims it (present -> deleting) then "crashes" before unlink/finalize: the row is `deleting`
    // with an old status_updated_at and the bytes are still on disk.
    assert!(matches!(
        a.db().claim_for_delete(&w, oid, 200).await.unwrap(),
        ClaimOutcome::Claimed { .. }
    ));

    // (3) A legacy upload of the SAME bytes records a `commit_object` edge — the object is now readable even
    // though its row is `deleting`.
    a.upload_candidate(&p, &w, &s, genesis(vec![file("B.md", body)]))
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
    // `commit_object` edge spares; see `recovery_sweep_spares_a_deleting_object_re_rooted_by_a_legacy_edge`).
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
        a.db().finalize_delete(&w, oid, 200).await.unwrap();
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
    // The unlink still finalizes normally.
    a.db().finalize_delete(&w, o, 200).await.unwrap();
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
