//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn propose_opens_a_proposal_without_moving_current(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-open").await;
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

#[sqlx::test]
async fn a_propose_against_an_absent_current_fails_typed_uploading_nothing(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-genesis").await;
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

#[sqlx::test]
async fn a_proposal_staled_by_a_publish_then_gc_reclaims_its_unique_object_and_reads_404(
    pool: PgPool,
) {
    // The keep-set == read-surface crux through the REAL write paths: propose roots a unique object (kept +
    // readable); a direct publish stales the proposal; GC reclaims the unique object; a read is 404, never Integrity.
    let fx = Fixture::new(pool, "pr-stale-gc").await;
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

#[sqlx::test]
async fn propose_then_approve_promotes_sideways_and_replays_idempotently(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-approve").await;
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

#[sqlx::test]
async fn interleaving_b_a_stale_approve_conflicts_then_rebase_and_approve_succeeds(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-interleave-b").await;
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

#[sqlx::test]
async fn interleaving_c_aba_a_stale_approve_conflicts_even_when_the_live_tree_matches_the_base(
    pool: PgPool,
) {
    // …X(beta)->Y(gamma); revert --to X makes current.tree == X.tree == the proposal's base tree, yet the
    // generation advanced. A late approve at the stale base must CONFLICT — a digest-only CAS would wrongly
    // accept (current.tree == base.tree); the whole-(epoch,seq) CAS catches it.
    let fx = Fixture::new(pool, "pr-interleave-c").await;
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

#[sqlx::test]
async fn approving_an_already_accepted_proposal_conflicts_and_never_promotes_twice(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-double-approve").await;
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

#[sqlx::test]
async fn four_eyes_blocks_self_approve_only_under_review_required(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-4eyes-on").await;
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

#[sqlx::test]
async fn a_solo_author_may_self_approve_when_review_required_is_off(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-4eyes-off").await;
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

#[sqlx::test]
async fn a_staled_then_gc_reclaimed_proposal_approve_conflicts_not_integrity(pool: PgPool) {
    // After a proposal stales AND GC reclaims its unique bytes, a late approve must be a clean CONFLICT — the
    // pre-transaction render fault is classified as stale (current moved), never surfaced as a corruption alarm.
    let fx = Fixture::new(pool, "pr-stale-approve").await;
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

#[sqlx::test]
async fn reject_flips_open_to_rejected_and_the_unique_object_reclaims(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-reject").await;
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

#[sqlx::test]
async fn rejecting_an_already_rejected_proposal_is_idempotent_and_approve_after_reject_is_typed(
    pool: PgPool,
) {
    let fx = Fixture::new(pool, "pr-reject-idem").await;
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

#[sqlx::test]
async fn an_unrostered_principal_cannot_reject(pool: PgPool) {
    let fx = Fixture::new(pool, "pr-reject-authz").await;
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

#[sqlx::test]
async fn the_review_required_loop_direct_is_approval_required_propose_needs_review_approve_ok(
    pool: PgPool,
) {
    // Under review_required a DIRECT publish is APPROVAL_REQUIRED (the dead-end), an explicit --propose is
    // NEEDS_REVIEW (the remedy), and a second-actor approve promotes — never confusing the two outcomes.
    let fx = Fixture::new(pool, "pr-rr-loop").await;
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

#[sqlx::test]
async fn the_proposals_table_rejects_out_of_range_generations(pool: PgPool) {
    // SF-4: the safe-integer CHECK pins every stored (epoch, seq) to the JCS ceiling a follower could verify.
    let fx = Fixture::new(pool, "pr-safeint").await;
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

#[sqlx::test]
async fn a_publish_by_an_unrostered_principal_is_denied_and_records_nothing_readable(pool: PgPool) {
    // The pointer-move's in-transaction authorization (the roster check) replaces the retired upload's
    // roster gate: a registered-but-unrostered device migrates its candidate but cannot promote it, and
    // records no commit_object — so the object is unreadable.
    let fx = Fixture::new(pool, "authz-unrostered").await;
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

#[sqlx::test]
async fn a_publish_cannot_adopt_another_skills_commit(pool: PgPool) {
    // The cross-skill adoption guard, in the SHARED write body (so it covers publish / propose / approve
    // alike): a content-addressed commit belongs to exactly one skill, so re-creating its identical bytes
    // under another skill is refused — even by a principal rostered for both.
    let fx = Fixture::new(pool, "authz-xskill").await;
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

#[sqlx::test]
async fn approve_after_reject_then_gc_is_denied_not_integrity(pool: PgPool) {
    // After a proposal is rejected AND a GC reclaims its now-unrooted unique bytes — while `current` is still
    // at the base (reject moves no pointer) — an approve's pre-transaction render faults over the missing
    // bytes. It must NOT surface as Integrity (a 500 / no receipt): the proposal is no longer open, so the
    // bytes were LEGITIMATELY reclaimed, and the transaction must produce a typed, receipted DENIED.
    let fx = Fixture::new(pool, "pr-reject-gc-approve").await;
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

#[sqlx::test]
async fn revert_to_a_proposal_commit_is_refused_so_it_cannot_bypass_review(pool: PgPool) {
    // A proposal commit carries a `skill_commit` provenance row (so its digest resolves) but NO `commit_object`
    // root — it is not an accepted version. Reverting to it would forward-promote its un-reviewed tree past the
    // review gate + four-eyes (revert bypasses both). The accepted-trunk gate must refuse it, leaving `current`
    // unmoved and never serving the proposal's bytes.
    let fx = Fixture::new(pool, "pr-revert-proposal").await;
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
