//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn genesis_creates_a_pointer_at_1_1_with_the_expected_record(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-genesis").await;
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

    // Read the stored record back — it is unsigned: authority is the row behind the pointer, and
    // integrity is the content-addressed version id re-verified byte-for-byte on apply. It names THIS
    // skill/ws at (1,1) and pins the promoted version id; the scope is what stops the record from being
    // replayed into another skill/ws (a follower checks it), so we pin exactly that scope.
    let record = fx
        .authority
        .db()
        .read_current_record(&w, &s)
        .await
        .unwrap()
        .expect("a current record");
    let wire = wire_record(&record);
    assert_eq!(wire.scope.workspace_id, "w_acme");
    assert_eq!(wire.scope.skill_id, "s_deploy");
    assert_eq!(wire.record.generation, gn(1, 1));
    assert_eq!(
        wire.record.version_id,
        digest::to_hex(&r.version_id.unwrap().0),
        "the record pins the promoted version id"
    );
}

#[sqlx::test]
async fn publish_advances_seq_within_the_epoch(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-advance").await;
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
#[sqlx::test]
async fn concurrent_publishes_one_ok_one_conflict(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-concurrent").await;
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
        crate::set_current::publish(&fx.authority, &w, &s, &sa, &da, None, None, CREATED_AT, NOW),
        crate::set_current::publish(&fx.authority, &w, &s, &sb, &db, None, None, CREATED_AT, NOW),
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
    // (That the CONFLICT above can arise via a serialization-failure retry under real MVCC — not only a
    // serialized schedule — is proven deterministically by `the_serializable_runner_retries_a_forced_
    // serialization_failure`; here we assert only the outcome, which every valid schedule satisfies.)
}

/// The teeth of the MVCC re-proof, made deterministic: force exactly one Postgres serialization failure
/// (SQLSTATE 40001) inside the `run_serializable!` macro and assert it rolled back and retried. A
/// live-concurrency assertion on `retry_count` is scheduler-dependent (an accidentally-serialized `join!`
/// reaches the same outcome without ever raising 40001); this pins the retry *mechanism* — the thing that
/// re-establishes SQLite's old single-writer safety on Postgres — to a single, repeatable observation.
#[sqlx::test]
async fn the_serializable_runner_retries_a_forced_serialization_failure(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-retry-proof").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(21);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;
    // A genesis publish creates the `current` row the forcing method contends on.
    publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "aaaaaaaa-0000-4000-8000-00000000000a",
        genesis(vec![file("a", b"0")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(fx.authority.db().retry_count(), 0);
    fx.authority
        .db()
        .test_force_one_serialization_retry(&w, &s)
        .await
        .expect("the runner re-runs the closure and the second attempt commits");
    assert_eq!(
        fx.authority.db().retry_count(),
        1,
        "the runner must roll back and retry exactly one forced serialization failure"
    );
}

/// Interleaving C — a revert advances `seq` across a byte round-trip, so a stale move at the pre-revert
/// generation CONFLICTs (a digest-only CAS would wrongly accept it; the whole-(epoch,seq) CAS catches it).
#[sqlx::test]
async fn revert_advances_seq_and_a_stale_publish_conflicts(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-revert").await;
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
    let rdev = revert_request(&w, "dk_a", gn(1, 2));
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
    let stale =
        crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, None, CREATED_AT, NOW)
            .await
            .unwrap();
    assert_eq!(stale.outcome, TerminalOutcome::Conflict);
    assert_eq!(stale.current, Some(gn(1, 3)));
}

/// The restore-ABA: a backup/restore that bumps `epoch` while reusing `seq`. A stale op at the OLD
/// generation (matching `seq`, lower `epoch`) CONFLICTs — a seq-only CAS would wrongly accept it.
#[sqlx::test]
async fn restore_aba_matching_seq_bumped_epoch_conflicts(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-aba").await;
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
    let stale =
        crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, None, CREATED_AT, NOW)
            .await
            .unwrap();
    assert_eq!(stale.outcome, TerminalOutcome::Conflict);
    assert_eq!(stale.current, Some(gn(2, 2)));
}

/// Interleaving E — a lost-ack retry: the original op committed (seq=2), the team moved on (seq=3), and the
/// retry returns the BYTE-IDENTICAL original receipt (the original stored record), not a spurious conflict.
#[sqlx::test]
async fn lost_ack_retry_replays_the_identical_receipt(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-lostack").await;
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
    let first =
        crate::set_current::publish(&fx.authority, &w, &s, &sk, &dk, None, None, CREATED_AT, NOW)
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

    // Retry op K (its ack was lost): the replay returns the ORIGINAL receipt byte-for-byte (the (1,2)
    // record), even though current is now (1,3).
    let retry =
        crate::set_current::publish(&fx.authority, &w, &s, &sk, &dk, None, None, CREATED_AT, NOW)
            .await
            .unwrap();
    assert_eq!(retry, first);
    assert_eq!(retry.current, Some(gn(1, 2)));
}

/// A device revoked BEFORE the promotion (committed ahead of the pointer-move txn) blocks the move.
#[sqlx::test]
async fn a_revoke_before_promotion_blocks_the_move(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-revoke").await;
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
    let r =
        crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, None, CREATED_AT, NOW)
            .await
            .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    // The pointer did NOT move.
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
    // A revoked credential is a pre-auth cause: the DENIED is synthesized, never a durable receipt row
    // (the credential-model counterpart of the retired forged-credential pre-auth case).
    assert_eq!(
        receipt_rows(&pool, "f0000000-0000-4000-8000-000000000001").await,
        0,
        "a revoked device's DENIED must not mint a durable receipt row"
    );
}

/// After a successful promote + lease-release, a GC pass does NOT reclaim the new `current`'s objects (the
/// `skill_commit` + `commit_object` edges root them) — the re-rooting handoff has no reclaim window.
#[sqlx::test]
async fn post_promote_gc_does_not_reclaim_current_objects(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-gcreach").await;
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
#[sqlx::test]
async fn first_parent_mismatch_is_denied_even_when_the_cas_matches(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-firstparent").await;
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
    let r =
        crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, None, CREATED_AT, NOW)
            .await
            .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c1)
    ); // unmoved
    // The receipt carries the live commit id for a clock-anomaly diagnostic.
    let detail = r.details.unwrap();
    assert_eq!(detail["code"], "FIRST_PARENT_MISMATCH");
}

/// A two-parent author-merge candidate is rejected wholesale in the backbone (merges are a later increment).
#[sqlx::test]
async fn a_two_parent_merge_is_denied(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-merge").await;
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
    let r =
        crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, None, CREATED_AT, NOW)
            .await
            .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
}

/// The protection gate REROUTES, it does not refuse: a plain MEMBER's direct publish on an
/// effectively-REVIEWED bundle (here the workspace `review_required` default, no per-skill pin) is
/// DOWNGRADED in-transaction to a proposal — NEEDS_REVIEW carrying a `downgraded` detail, with `current`
/// frozen, an open `proposals` row opened, and the migrate lease released. Genesis always LANDS directly
/// (a proposal against a base that does not exist is meaningless, and the role matrix gives members
/// brand-new skills), and a same-`op_id` retry replays the downgraded receipt byte-for-byte.
#[sqlx::test]
async fn a_member_direct_publish_on_a_reviewed_bundle_downgrades_to_a_proposal(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-downgrade").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(21);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();

    // Genesis BYPASSES the gate (someone must create the first version; it cannot be proposed against a
    // base that does not exist) — even under review-required, it LANDS directly at (1,1).
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
    assert_eq!(g.current, Some(gn(1, 1)));
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // A NON-genesis direct publish by a plain MEMBER on the reviewed bundle DOWNGRADES to a proposal.
    let child_op = "40000000-0000-4000-8000-000000000001";
    let r = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        child_op,
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::NeedsReview);
    assert_eq!(
        r.details
            .as_ref()
            .and_then(|d| d.get("downgraded"))
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "the receipt marks the publish downgraded to a proposal"
    );
    // `current` never moved — the pointer is frozen at the genesis version, generation unchanged.
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &s)
            .await
            .unwrap(),
        Some(gn(1, 1))
    );
    // An open proposal was opened for the downgraded candidate, and its migrate lease is released.
    assert_eq!(
        open_proposal_count(&pool, w.as_str(), s.as_str()).await,
        1,
        "the downgrade opens exactly one open proposal"
    );
    assert_eq!(
        lease_count(&pool, w.as_str(), child_op).await,
        0,
        "the migrate lease is released after the downgrade"
    );

    // A same-op_id retry replays the downgraded receipt byte-for-byte (and opens no second proposal).
    let retry = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        child_op,
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(retry, r, "the downgraded receipt replays byte-identically");
    assert_eq!(
        open_proposal_count(&pool, w.as_str(), s.as_str()).await,
        1,
        "the replay opens no second proposal"
    );
}

/// A REVOKED device's genuinely FRESH write is DENIED at the shared `resolve_device_op` front door —
/// SYNTHESIZED, never a durable receipt — uniformly across every pre-transaction path. The membership
/// front-door check runs BEFORE the in-transaction protection gate could ever downgrade the publish to a
/// proposal, so a de-authorized principal grows no `op_receipts` (and mints no `proposals`) row at all.
/// The revoked device is admitted only to REPLAY a receipt it minted while authorized — pinned by the
/// sibling OK-replay assertion below.
#[sqlx::test]
async fn a_revoked_device_fresh_write_is_denied_but_still_replays_a_stored_receipt(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-gate-revoked").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(52);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    // A genesis publish (while authorized) leaves a stored OK receipt to replay later, and a `current` base.
    let genesis_op = "52000000-0000-4000-8000-000000000000";
    let g = fx
        .authority
        .publish(
            &w,
            &s,
            &op(genesis_op),
            genesis(vec![file("f", b"0")]),
            DeviceOpAuth {
                credential: cred(&w, "dk_a"),
                op: DeviceOp::PublishDirect,
                expected: gn(0, 0),
            },
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(g.current, Some(gn(1, 1)));
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    // Turn review-required ON so a live fresh child publish by a MEMBER WOULD downgrade to a proposal
    // (a durable NEEDS_REVIEW receipt + a proposals row) — the exact rows a de-authorized device must not grow.
    fx.authority.set_review_required(&w, true).await.unwrap();

    // Revoke the device, then drive a FRESH child publish. It is DENIED at the front door (never downgraded),
    // the pointer does not move, and NOTHING durable is minted — the revoked device gains no audit-row vector.
    fx.authority.db().revoke_device(&w, "dk_a").await.unwrap();
    let fresh_op = "52000000-0000-4000-8000-000000000001";
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &op(fresh_op),
            child(c0, vec![file("f", b"1")]),
            DeviceOpAuth {
                credential: cred(&w, "dk_a"),
                op: DeviceOp::PublishDirect,
                expected: gn(1, 1),
            },
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
    assert_eq!(
        receipt_rows(&pool, fresh_op).await,
        0,
        "a revoked device's fresh write must not mint a durable receipt row"
    );

    // The replay property survives revocation: retrying the genesis op_id (submitted while authorized)
    // still replays its stored OK byte-identically — the revoked device is admitted for the replay only.
    let replay = fx
        .authority
        .publish(
            &w,
            &s,
            &op(genesis_op),
            genesis(vec![file("f", b"0")]),
            DeviceOpAuth {
                credential: cred(&w, "dk_a"),
                op: DeviceOp::PublishDirect,
                expected: gn(0, 0),
            },
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(replay.outcome, TerminalOutcome::Ok);
    assert_eq!(replay.current, g.current);
}

/// A reviewer|owner direct publish on an effectively-REVIEWED bundle LANDS directly — the protected-branch
/// model: the protection gate downgrades only a plain MEMBER, and lands reviewer+ writes outright. The
/// pointer advances (no proposal is opened).
#[sqlx::test]
async fn a_reviewer_direct_publish_on_a_reviewed_bundle_lands_directly(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-reviewer-lands").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(23);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;
    // `register` seats a plain member; upgrade the SAME seat to reviewer, then turn review-required ON.
    fx.authority
        .db()
        .seed_workspace_member(&w, &prin("p_dev"), "reviewer", "confirmed")
        .await
        .unwrap();
    fx.authority.set_review_required(&w, true).await.unwrap();

    // Genesis (lands), then a reviewer's direct CHILD publish LANDS directly — no downgrade.
    let g = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000000",
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
    let r = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000001",
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(
        r.outcome,
        TerminalOutcome::Ok,
        "a reviewer lands directly on a reviewed bundle"
    );
    assert_eq!(r.current, Some(gn(1, 2)), "the pointer advances");
    // The pointer moved off the genesis commit, and NO proposal was opened.
    assert_ne!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
    assert_eq!(open_proposal_count(&pool, w.as_str(), s.as_str()).await, 0);
}

/// A per-skill `open` pin OVERRIDES the workspace review-required DEFAULT (the cascade reads the per-bundle
/// pin first): with the bundle pinned open, a plain MEMBER's direct publish LANDS directly rather than
/// downgrading.
#[sqlx::test]
async fn a_per_skill_open_pin_overrides_the_review_required_default(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-open-pin").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(24);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;
    fx.authority.set_review_required(&w, true).await.unwrap();

    // Genesis registers the catalog row (protection NULL ⇒ follows the workspace default = reviewed).
    let g = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "42000000-0000-4000-8000-000000000000",
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

    // Pin THIS skill explicitly `open` — the per-bundle pin the cascade reads before the workspace default.
    sqlx::query("UPDATE catalog SET protection = 'open' WHERE workspace_id = $1 AND skill_id = $2")
        .bind(w.as_str())
        .bind(s.as_str())
        .execute(&pool)
        .await
        .unwrap();

    // A plain member's direct child publish now LANDS (the pin beats the workspace review-required default).
    let r = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "42000000-0000-4000-8000-000000000001",
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(
        r.outcome,
        TerminalOutcome::Ok,
        "an open pin lets a member's direct publish land under a reviewed default"
    );
    assert_eq!(r.current, Some(gn(1, 2)));
    assert_eq!(open_proposal_count(&pool, w.as_str(), s.as_str()).await, 0);
}

/// A revert may only target a version of the SAME skill — reverting to another skill's commit (same
/// workspace) is refused, so the forward commit can never graft a foreign tree under this skill's edges.
#[sqlx::test]
async fn revert_to_another_skills_commit_is_refused(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-xskill-revert").await;
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
    let rdev = revert_request(&w, "dk_a", gn(1, 1));
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

/// A candidate of new bytes submitted to the PUBLISH entry LABELLED as a non-direct op (e.g. `Revert`) is
/// rejected before ingest — otherwise it would skip the review gate while reaching the promote path.
#[sqlx::test]
async fn publish_labelled_as_a_non_direct_op_is_rejected_before_ingest(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-opbypass").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(31);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;
    fx.authority
        .db()
        .set_review_required(&w, true)
        .await
        .unwrap();

    let op_id = op("31000000-0000-4000-8000-000000000000");
    let dev = DeviceOpAuth {
        credential: cred(&w, "dk_a"),
        op: DeviceOp::Revert,
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
            None,
            None,
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
#[sqlx::test]
async fn a_conflict_releases_the_lease_so_abandoned_objects_are_reclaimable(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-conflict-lease").await;
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
        crate::set_current::publish(&fx.authority, &w, &s, &sa, &da, None, None, CREATED_AT, NOW)
            .await
            .unwrap()
            .is_ok()
    );
    let rb =
        crate::set_current::publish(&fx.authority, &w, &s, &sb, &db, None, None, CREATED_AT, NOW)
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
#[sqlx::test]
async fn a_revert_lost_ack_retry_replays_the_original_ok(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-revert-replay").await;
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
    let rdev = revert_request(&w, "dk_a", gn(1, 2));
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
/// distinct receipt key that decodes to the SAME 16 bytes, so accepting it would split the idempotency
/// slot. Requiring the canonical hyphenated form keeps the key 1:1 with the op identity.
#[sqlx::test]
async fn a_non_canonical_uuid_op_id_is_rejected(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-opid-canon").await;
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

/// The genesis standup: a CONFIRMED workspace member with NO per-skill roster row genesis-publishes a
/// brand-new skill — the publish succeeds at (1,1) and self-seats the author's roster follow-state in the
/// same transaction. A follow-up NON-genesis publish then succeeds too (both writes gate on the confirmed
/// membership now; the self-seated roster row is follow-state that no longer gates any write).
#[sqlx::test]
async fn genesis_by_a_confirmed_member_stands_up_the_skill(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-standup").await;
    let (w, s) = (ws("w_acme"), skill("s_new"));
    let key = dev_key(31);
    let p = prin("p_author");
    fx.authority
        .db()
        .seed_device(&w, "dk_a", &key, &p, false, &cred(&w, "dk_a"))
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(&w, &p, "member", "confirmed")
        .await
        .unwrap();
    // Deliberately NO catalog / follow seeding — the genesis publish registers the skill and
    // self-follows the author itself.

    let g = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "eeeeeeee-0000-4000-8000-000000000000",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    assert!(g.is_ok());
    assert_eq!(g.current, Some(gn(1, 1)));

    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();
    let n = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "eeeeeeee-0000-4000-8000-000000000001",
        child(c0, vec![file("SKILL.md", b"v1")]),
        gn(1, 1),
    )
    .await;
    assert!(n.is_ok());
    assert_eq!(n.current, Some(gn(1, 2)));
}

/// An INVITED-but-unconfirmed member cannot stand up a skill: the genesis-eligible shape alone is not
/// enough — the standup requires a CONFIRMED workspace membership, and nothing is created on the DENIED.
/// (No member row at all is the same DENIED, proven by
/// `a_publish_by_a_non_member_principal_is_denied_and_records_nothing_readable`.)
#[sqlx::test]
async fn genesis_by_an_invited_unconfirmed_member_is_denied(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-standup-invited").await;
    let (w, s) = (ws("w_acme"), skill("s_new"));
    let key = dev_key(32);
    let p = prin("p_invitee");
    fx.authority
        .db()
        .seed_device(&w, "dk_a", &key, &p, false, &cred(&w, "dk_a"))
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(&w, &p, "member", "invited")
        .await
        .unwrap();

    let r = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "eeeeeeee-0000-4000-8000-000000000002",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert!(
        fx.authority
            .db()
            .read_current_commit(&w, &s)
            .await
            .unwrap()
            .is_none()
    );
}

/// Two concurrent GENESIS publishes of the same brand-new skill (a confirmed member, no roster row):
/// exactly one creates (1,1) and self-rosters the author; the loser's serialization retry re-reads the
/// now-present pointer and returns a clean CONFLICT carrying the live generation — never a double standup.
#[sqlx::test]
async fn concurrent_genesis_standups_one_ok_one_conflict(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-standup-concurrent").await;
    let (w, s) = (ws("w_acme"), skill("s_new"));
    let key = dev_key(33);
    let p = prin("p_author");
    fx.authority
        .db()
        .seed_device(&w, "dk_a", &key, &p, false, &cred(&w, "dk_a"))
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(&w, &p, "member", "confirmed")
        .await
        .unwrap();

    let (sa, da) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "eeeeeeee-0000-4000-8000-000000000003",
        genesis(vec![file("a", b"A")]),
        gn(0, 0),
    )
    .await;
    let (sb, db) = prepare(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "eeeeeeee-0000-4000-8000-000000000004",
        genesis(vec![file("a", b"B")]),
        gn(0, 0),
    )
    .await;
    let (ra, rb) = tokio::join!(
        crate::set_current::publish(&fx.authority, &w, &s, &sa, &da, None, None, CREATED_AT, NOW),
        crate::set_current::publish(&fx.authority, &w, &s, &sb, &db, None, None, CREATED_AT, NOW),
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
    let conflict = if ra.outcome == TerminalOutcome::Conflict {
        &ra
    } else {
        &rb
    };
    assert_eq!(conflict.current, Some(gn(1, 1)));
}

// ── pre-authentication DENIED: synthesized, never persisted (the anti-forgery/unbounded-growth guard) ──

/// Count the durable `op_receipts` rows for an op id (raw `sqlx::query`, so it adds nothing to the
/// committed `.sqlx` drift surface).
async fn receipt_rows(pool: &PgPool, op_id: &str) -> i64 {
    use sqlx::Row as _;
    sqlx::query("SELECT COUNT(*)::int8 AS n FROM op_receipts WHERE op_id = $1")
        .bind(op_id)
        .fetch_one(pool)
        .await
        .unwrap()
        .get::<i64, _>("n")
}

/// Count the OPEN `proposals` rows for a skill (raw `sqlx`, off the `.sqlx` drift surface) — the downgrade
/// tests assert a member's gated publish opens exactly one, and a replay opens no second.
async fn open_proposal_count(pool: &PgPool, ws: &str, skill: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::int8 FROM proposals \
         WHERE workspace_id = $1 AND skill_id = $2 AND status = 'open'",
    )
    .bind(ws)
    .bind(skill)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// Count the `promotion_lease` rows for an op id — a downgraded publish must have released its migrate lease.
async fn lease_count(pool: &PgPool, ws: &str, op_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::int8 FROM promotion_lease WHERE workspace_id = $1 AND op_id = $2",
    )
    .bind(ws)
    .bind(op_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// An UNKNOWN workspace credential is the same synthesized, never-persisted DENIED — for both the
/// pointer-move transaction and the standalone reject transaction. (Nothing signs, so the credential-model
/// pre-auth causes are exactly an unknown or a revoked credential — a credential that resolves to no
/// non-revoked registry row; the revoked case is pinned by `a_revoke_before_promotion_blocks_the_move`.)
#[sqlx::test]
async fn an_unknown_device_denied_is_never_persisted(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-preauth-ghost").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(42);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    // A real current (so the reject path below resolves a recorded digest for its commit).
    let g = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        "43000000-0000-4000-8000-000000000000",
        genesis(vec![file("f", b"v0")]),
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

    // Pointer-move path: a publish presenting a never-registered device key.
    let ghost_op = op("43000000-0000-4000-8000-000000000001");
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &ghost_op,
            child(c0, vec![file("f", b"v1")]),
            DeviceOpAuth {
                credential: cred(&w, "dk_ghost"),
                op: DeviceOp::PublishDirect,
                expected: gn(1, 1),
            },
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(receipt_rows(&pool, ghost_op.as_str()).await, 0);

    // Reject path: a review --reject presenting a never-registered device key.
    let ghost_reject = op("43000000-0000-4000-8000-000000000002");
    let r = fx
        .authority
        .review_reject(
            &w,
            &s,
            c0,
            DeviceOpAuth {
                credential: cred(&w, "dk_ghost"),
                op: DeviceOp::ReviewReject,
                expected: gn(1, 1),
            },
            &ghost_reject,
            CREATED_AT,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(receipt_rows(&pool, ghost_reject.as_str()).await, 0);
}

/// An AUTHENTICATED denial (a resolved, non-revoked device whose principal fails an authorization gate)
/// stays durable and replays byte-identically — the pre-auth carve-out narrows exactly to unresolvable
/// actors (an unknown or revoked device key).
#[sqlx::test]
async fn an_authenticated_denial_stays_durable_and_replays(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-authz-denied").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(44);
    // Registered device (the credential lookup WILL resolve) — but no roster row and no confirmed
    // workspace membership, so the genesis-shaped publish fails the authenticated member gate.
    let p = prin("p_out");
    fx.authority
        .db()
        .seed_device(&w, "dk_out", &key, &p, false, &cred(&w, "dk_out"))
        .await
        .unwrap();

    let op_id = op("44000000-0000-4000-8000-000000000000");
    let device = DeviceOpAuth {
        credential: cred(&w, "dk_out"),
        op: DeviceOp::PublishDirect,
        expected: gn(0, 0),
    };
    let files = vec![file("SKILL.md", b"outsider genesis")];
    let first = fx
        .authority
        .publish(
            &w,
            &s,
            &op_id,
            genesis(files.clone()),
            device.clone(),
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(first.outcome, TerminalOutcome::Denied);
    assert_eq!(
        receipt_rows(&pool, op_id.as_str()).await,
        1,
        "an authenticated denial names a verified device and stays receipted"
    );
    // A same-op_id retry replays the stored receipt byte-identically.
    let retry = fx
        .authority
        .publish(
            &w,
            &s,
            &op_id,
            genesis(files),
            device,
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(retry, first);
}

// ── a corrupt stored receipt faults instead of replaying altered bytes ──────────────────────────────

/// A stored receipt whose `details` column no longer parses is an Integrity fault on replay — never a
/// silent replay with the details dropped (which would violate byte-identical replay without a sound).
#[sqlx::test]
async fn a_corrupt_stored_receipt_details_is_integrity_instead_of_replaying(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-baddetails").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(45);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    let files = vec![file("SKILL.md", b"replay me")];
    let op_id = op("45000000-0000-4000-8000-000000000000");
    let device = DeviceOpAuth {
        credential: cred(&w, "dk_a"),
        op: DeviceOp::PublishDirect,
        expected: gn(0, 0),
    };
    let first = fx
        .authority
        .publish(
            &w,
            &s,
            &op_id,
            genesis(files.clone()),
            device.clone(),
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert!(first.is_ok());

    // Corrupt the stored details out-of-band (raw query — nothing added to the .sqlx surface).
    sqlx::query("UPDATE op_receipts SET details = 'not json' WHERE op_id = $1")
        .bind(op_id.as_str())
        .execute(&pool)
        .await
        .unwrap();

    // The same-op retry must fault, never replay an altered receipt.
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &op_id,
            genesis(files),
            device,
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await;
    assert!(
        matches!(r, Err(AuthorityError::Integrity(_))),
        "a corrupt receipt row must fault as Integrity: {r:?}"
    );
}
