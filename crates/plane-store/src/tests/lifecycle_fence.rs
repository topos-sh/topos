//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

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
