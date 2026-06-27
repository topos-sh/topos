//! In-crate authority tests (the `pub(crate)` seed helper is only visible here, never to an external
//! integration crate). They exercise the access rule, cross-workspace and cross-skill isolation, the
//! upload/rehash guard, dedup-obliviousness, and the transaction discipline against a real SQLite
//! database + a real per-workspace git store.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use topos_core::digest;

use crate::sqlite::RecordOutcome;
use crate::{
    Authority, AuthorityError, CandidateUpload, CommitId, FileMode, ObjectId, Principal, SkillId,
    UploadedFile, WorkspaceId,
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
