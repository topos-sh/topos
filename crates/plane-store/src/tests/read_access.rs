//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn a_member_reads_the_bytes_of_a_version(pool: PgPool) {
    let fx = Fixture::new(pool, "read-ok").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let reader = prin("dev_read");
    // The read gate is now a CONFIRMED workspace member (the per-skill roster no longer scopes reads).
    a.db()
        .seed_workspace_member(&w, &reader, "member", "confirmed")
        .await
        .unwrap();

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

    // A confirmed member reads each of the version's objects (the read path resolves via the access join).
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

#[sqlx::test]
async fn non_member_reader_gets_notfound_for_a_real_object(pool: PgPool) {
    let fx = Fixture::new(pool, "read-nonmember").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let body = b"secret bytes";
    stage_committed(a, &w, &s, "nonmember", vec![file("SKILL.md", body)]).await;

    // A principal who is not a confirmed workspace member gets the uniform not-found (never the bytes,
    // never a 403) — membership is the read gate now.
    let outsider = prin("dev_outsider");
    assert!(matches!(
        a.read_object(&outsider, &w, &s, object_id(body)).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn revocation_of_membership_stops_reads(pool: PgPool) {
    let fx = Fixture::new(pool, "revoke").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_x");
    a.db()
        .seed_workspace_member(&w, &p, "member", "confirmed")
        .await
        .unwrap();
    let body = b"body";
    stage_committed(a, &w, &s, "revoke", vec![file("SKILL.md", body)]).await;
    assert_eq!(
        a.read_object(&p, &w, &s, object_id(body)).await.unwrap(),
        body
    );

    // The read gate is a CONFIRMED workspace_member row; revoking confirmation (a remove/downgrade)
    // stops the read immediately — the same row-write-is-effective-immediately property the deleted
    // per-skill roster had.
    a.db()
        .seed_workspace_member(&w, &p, "member", "invited")
        .await
        .unwrap();
    assert!(matches!(
        a.read_object(&p, &w, &s, object_id(body)).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn cross_workspace_object_is_unreadable_under_another_scope(pool: PgPool) {
    let fx = Fixture::new(pool, "xws").await;
    let a = &fx.authority;
    let (wa, wb, s) = (ws("w_a"), ws("w_b"), skill("s_pr"));
    let p = prin("dev_p");

    // Stage a real object into workspace B.
    let secret = b"workspace B private bytes";
    stage_committed(a, &wb, &s, "xws", vec![file("SKILL.md", secret)]).await;

    // The same principal, a confirmed member of workspace A, cannot read B's object by supplying B's
    // object id under A's scope — the workspace_id binding makes it a uniform not-found (reachability
    // isolation, distinct from the membership gate, which A's seat passes).
    a.db()
        .seed_workspace_member(&wa, &p, "member", "confirmed")
        .await
        .unwrap();
    assert!(matches!(
        a.read_object(&p, &wa, &s, object_id(secret)).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn cross_skill_object_is_unreadable_and_indistinguishable_from_absent(pool: PgPool) {
    let fx = Fixture::new(pool, "xskill").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (x, y) = (skill("s_x"), skill("s_y"));
    let p = prin("dev_p");

    // An object reachable only via skill Y, in the shared per-workspace store.
    let y_bytes = b"skill Y only";
    stage_committed(a, &w, &y, "xskill", vec![file("SKILL.md", y_bytes)]).await;

    // A confirmed member (the gate passes) reading under skill X's scope gets not-found for Y's object —
    // per-skill REACHABILITY still isolates: skill X does not reach Y's bytes, so it is byte-for-byte
    // identical to asking for an object that exists in no skill at all. (Membership gates WHO may ask; the
    // skill-scoped witness still gates WHAT that skill reaches.)
    a.db()
        .seed_workspace_member(&w, &p, "member", "confirmed")
        .await
        .unwrap();
    let cross = a.read_object(&p, &w, &x, object_id(y_bytes)).await;
    let absent = a
        .read_object(&p, &w, &x, object_id(b"never uploaded"))
        .await;
    assert!(matches!(cross, Err(AuthorityError::NotFound)));
    assert!(matches!(absent, Err(AuthorityError::NotFound)));
}

/// Exercise the access join + the pointer table directly from staged rows (no upload), isolating the
/// authorization logic. The witness resolves only on the full member ∧ reachable match; every
/// mismatch — wrong principal (non-member), skill, workspace, or object — collapses to no witness.
#[sqlx::test]
async fn seeded_access_join_resolves_a_witness_and_isolates_every_axis(pool: PgPool) {
    let fx = Fixture::new(pool, "seed-join").await;
    let a = &fx.authority;
    let (w, s, p) = (ws("w_acme"), skill("s_pr"), prin("dev_p"));
    let commit = CommitId([0x33; 32]);
    let obj = ObjectId([0x44; 32]);

    // The gate is now a CONFIRMED workspace member (workspace-scoped); the reachability half stays
    // skill-scoped.
    a.db()
        .seed_workspace_member(&w, &p, "member", "confirmed")
        .await
        .unwrap();
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
    // A non-member principal → gate denies → no witness.
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
#[sqlx::test]
async fn check_lineage_uses_seeded_provenance(pool: PgPool) {
    use crate::{CandidateCommit, LineageDecision};
    let fx = Fixture::new(pool, "lineage-db").await;
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
