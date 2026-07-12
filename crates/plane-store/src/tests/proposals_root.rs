//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn an_open_non_stale_proposal_roots_and_reads_then_a_stale_one_reclaims_and_404s(
    pool: PgPool,
) {
    // The keep-set == read-surface crux: an open, non-stale proposal's unique object is kept + readable; the
    // instant a publish stales the proposal the SAME object drops out of read AND retention together (no
    // event, no reaper), and
    // a read of the reclaimed object is 404 — never an Integrity corruption fault.
    let fx = Fixture::new(pool, "prop-crux").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    // The read gate is confirmed membership now; the per-skill roster row is follow-state only.
    a.db()
        .seed_workspace_member(&w, &reader, "member", "confirmed")
        .await
        .unwrap();

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
    // A read of the reclaimed object is 404 — NEVER an Integrity fault (keep-set == read surface).
    assert!(matches!(
        a.read_object(&reader, &w, &s, x).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn recovery_claim_spares_an_open_proposals_object_and_reclaims_a_staled_one(pool: PgPool) {
    // The third copy of the predicate: `claim_stale_for_recovery` must spare a stale `deleting` row an open,
    // non-stale proposal roots, then reclaim it once the proposal goes stale — tracking the read gate exactly.
    let fx = Fixture::new(pool, "prop-recovery").await;
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

#[sqlx::test]
async fn a_rejected_proposals_unique_object_reclaims_and_reads_404(pool: PgPool) {
    // A non-`open` proposal never roots or authorizes — even at a matching base — so its unique bytes reclaim.
    let fx = Fixture::new(pool, "prop-reject").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    // The read gate is confirmed membership now; the per-skill roster row is follow-state only.
    a.db()
        .seed_workspace_member(&w, &reader, "member", "confirmed")
        .await
        .unwrap();
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

#[sqlx::test]
async fn a_trunk_shared_object_stays_kept_and_readable_after_its_proposal_stales(pool: PgPool) {
    // An object reachable from BOTH the trunk (a `commit_object` edge) and a proposal stays kept + readable
    // when the proposal stales — the trunk arm is untouched; only the proposal's UNIQUE objects reclaim.
    let fx = Fixture::new(pool, "prop-shared").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    // The read gate is confirmed membership now; the per-skill roster row is follow-state only.
    a.db()
        .seed_workspace_member(&w, &reader, "member", "confirmed")
        .await
        .unwrap();
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

#[sqlx::test]
async fn genuine_corruption_under_an_open_proposal_is_integrity_not_masked_as_404(pool: PgPool) {
    // The read-time TOCTOU guard re-authorizes on a fetch miss and downgrades to 404 ONLY when the object is
    // no longer authorized (a legitimately reclaimed proposal object). An object STILL rooted by an open,
    // non-stale proposal whose bytes are gone is genuine corruption — the guard's re-authorize returns Some,
    // so the Integrity fault must STAND, never be masked. (The guard's converse — the concurrent
    // authorize→stale→reclaim→fetch race that downgrades to 404 — is a window the single-threaded harness
    // cannot interleave; its outcome equals the reclaimed-object 404 the crux test asserts.)
    let fx = Fixture::new(pool, "prop-corrupt").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    // The read gate is confirmed membership now; the per-skill roster row is follow-state only.
    a.db()
        .seed_workspace_member(&w, &reader, "member", "confirmed")
        .await
        .unwrap();
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
