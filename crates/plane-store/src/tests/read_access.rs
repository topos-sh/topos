//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

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
