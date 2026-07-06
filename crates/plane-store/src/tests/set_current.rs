//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn genesis_creates_a_signed_pointer_at_1_1_and_verifies(pool: PgPool) {
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
        crate::set_current::publish(&fx.authority, &w, &s, &sa, &da, None, CREATED_AT, NOW),
        crate::set_current::publish(&fx.authority, &w, &s, &sb, &db, None, CREATED_AT, NOW),
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
    let stale = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, CREATED_AT, NOW)
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
    let stale = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(stale.outcome, TerminalOutcome::Conflict);
    assert_eq!(stale.current, Some(gn(2, 2)));
}

/// Interleaving E — a lost-ack retry: the original op committed (seq=2), the team moved on (seq=3), and the
/// retry returns the BYTE-IDENTICAL original receipt (the original signed record), not a spurious conflict.
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
    let first = crate::set_current::publish(&fx.authority, &w, &s, &sk, &dk, None, CREATED_AT, NOW)
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
    let retry = crate::set_current::publish(&fx.authority, &w, &s, &sk, &dk, None, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(retry, first);
    assert_eq!(retry.current, Some(gn(1, 2)));
}

/// A device revoked BEFORE the promotion (committed ahead of the pointer-move txn) blocks the move.
#[sqlx::test]
async fn a_revoke_before_promotion_blocks_the_move(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-revoke").await;
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
    let r = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, CREATED_AT, NOW)
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
    let r = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, CREATED_AT, NOW)
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
    let r = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
}

/// The review-required gate: a direct publish preflight short-circuits to APPROVAL_REQUIRED having ingested
/// nothing; and the in-transaction read is authoritative if a migrate somehow happened first.
#[sqlx::test]
async fn review_required_gates_a_direct_publish(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-gate").await;
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
    let r = crate::set_current::publish(&fx.authority, &w, &s, &ss, &ds, None, CREATED_AT, NOW)
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::ApprovalRequired);
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
}

/// Once `review_required` is MUTABLE (the public set-policy op exists), a gated direct publish must REPLAY
/// its original `APPROVAL_REQUIRED` even when the policy is turned OFF between a lost-ack and a same-`op_id`
/// retry. The preflight gate binds `commit = None`, so without the pre-txn replay the retry would fall
/// through to the promote path, whose commit-comparison replay would burn it as `OP_ID_REUSED`. The pointer
/// never moves, and a FRESH op under the now-off policy is NOT gated (the replay is op-scoped).
#[sqlx::test]
async fn a_gated_publish_replays_approval_required_across_a_policy_flip(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-gate-flip").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(23);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    // Genesis (bypasses the gate) so a child publish has a base.
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

    // Review ON (via the new PUBLIC op): the preflight gates op_id X → APPROVAL_REQUIRED (commit/digest None).
    let op_id = op("41000000-0000-4000-8000-000000000001");
    fx.authority.set_review_required(&w, true).await.unwrap();
    let pre1 = crate::set_current::publish_preflight(
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
    assert_eq!(pre1.unwrap().outcome, TerminalOutcome::ApprovalRequired);

    // Flip the policy OFF, then RETRY the SAME op_id: the gated outcome is REPLAYED (without the fix this
    // returns `None`, the promote runs, and the bound-identity mismatch burns it as OP_ID_REUSED).
    fx.authority.set_review_required(&w, false).await.unwrap();
    let pre2 = crate::set_current::publish_preflight(
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
    assert_eq!(
        pre2.expect("the gated outcome replays across the policy flip")
            .outcome,
        TerminalOutcome::ApprovalRequired,
    );

    // A DIFFERENT op id under the now-off policy is NOT gated — the replay is op-scoped, not a sticky gate.
    let fresh = op("41000000-0000-4000-8000-000000000002");
    let pre3 = crate::set_current::publish_preflight(
        &fx.authority,
        &w,
        &s,
        DeviceOp::PublishDirect,
        "dk_a",
        &fresh,
        None,
        None,
        gn(1, 1),
        CREATED_AT,
    )
    .await
    .unwrap();
    assert!(pre3.is_none(), "a fresh op under review-off is not gated");

    // The pointer never moved across the whole flip.
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
}

/// The MIRROR direction: a SUCCESSFUL direct publish (review OFF), then the policy flips ON, then a
/// same-`op_id` retry. The stored OK receipt binds `commit = Some`; the preflight must NOT re-gate it — it
/// skips the gate so the promote path's in-txn replay (which runs BEFORE the in-txn gate) returns the
/// original OK, rather than recording a `commit = None` gate receipt that mismatches the stored one and burns
/// the op as `OP_ID_REUSED`. A FRESH op is still gated under the now-on policy.
#[sqlx::test]
async fn a_successful_publish_replays_ok_even_after_review_required_flips_on(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-ok-flip-on").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(24);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    // Genesis, then a successful CHILD publish under review-OFF (records an OK receipt for op_id Y).
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
    let op_y = "42000000-0000-4000-8000-000000000001";
    let ok = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        op_y,
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(ok.outcome, TerminalOutcome::Ok);
    let c1 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // Flip review ON. The preflight for the SAME op id must NOT re-gate the stored OK (skip the gate ⇒ None).
    fx.authority.set_review_required(&w, true).await.unwrap();
    let pre = crate::set_current::publish_preflight(
        &fx.authority,
        &w,
        &s,
        DeviceOp::PublishDirect,
        "dk_a",
        &op(op_y),
        None,
        None,
        gn(1, 1),
        CREATED_AT,
    )
    .await
    .unwrap();
    assert!(
        pre.is_none(),
        "a stored OK op is not re-gated when review flips on (the gate is skipped)"
    );

    // A FRESH op IS gated under the now-on policy (the gate still fires for new ops).
    let fresh = op("42000000-0000-4000-8000-000000000002");
    let pre_fresh = crate::set_current::publish_preflight(
        &fx.authority,
        &w,
        &s,
        DeviceOp::PublishDirect,
        "dk_a",
        &fresh,
        None,
        None,
        gn(1, 2),
        CREATED_AT,
    )
    .await
    .unwrap();
    assert_eq!(
        pre_fresh.unwrap().outcome,
        TerminalOutcome::ApprovalRequired
    );

    // The full promote retry of op Y replays the original OK (the in-txn replay precedes the in-txn gate),
    // not OP_ID_REUSED — and the pointer does not double-advance.
    let retry = publish(
        &fx,
        &key,
        "dk_a",
        &w,
        &s,
        op_y,
        child(c0, vec![file("f", b"1")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(
        retry.outcome,
        TerminalOutcome::Ok,
        "the retry replays the original OK"
    );
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c1)
    );
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
#[sqlx::test]
async fn publish_signed_as_a_non_direct_op_is_rejected_before_ingest(pool: PgPool) {
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
        crate::set_current::publish(&fx.authority, &w, &s, &sa, &da, None, CREATED_AT, NOW)
            .await
            .unwrap()
            .is_ok()
    );
    let rb = crate::set_current::publish(&fx.authority, &w, &s, &sb, &db, None, CREATED_AT, NOW)
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
/// brand-new skill — the publish succeeds at (1,1) and self-rosters the author in the same transaction.
/// The roster row is proven behaviorally: a follow-up NON-genesis publish (which passes the roster gate
/// only when the row exists) succeeds.
#[sqlx::test]
async fn genesis_by_a_confirmed_member_stands_up_the_roster(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-standup").await;
    let (w, s) = (ws("w_acme"), skill("s_new"));
    let key = dev_key(31);
    let p = prin("p_author");
    fx.authority
        .db()
        .seed_device(&w, "dk_a", &key.verifying_key().to_bytes(), &p, false)
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(&w, &p, "member", "confirmed")
        .await
        .unwrap();
    // Deliberately NO seed_roster — the standup must create the row itself.

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
/// `a_publish_by_an_unrostered_principal_is_denied_and_records_nothing_readable`.)
#[sqlx::test]
async fn genesis_by_an_invited_unconfirmed_member_is_denied(pool: PgPool) {
    let fx = Fixture::new(pool, "sc-standup-invited").await;
    let (w, s) = (ws("w_acme"), skill("s_new"));
    let key = dev_key(32);
    let p = prin("p_invitee");
    fx.authority
        .db()
        .seed_device(&w, "dk_a", &key.verifying_key().to_bytes(), &p, false)
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
        .seed_device(&w, "dk_a", &key.verifying_key().to_bytes(), &p, false)
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
        crate::set_current::publish(&fx.authority, &w, &s, &sa, &da, None, CREATED_AT, NOW),
        crate::set_current::publish(&fx.authority, &w, &s, &sb, &db, None, CREATED_AT, NOW),
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

/// The server-trusted genesis ids for a candidate — the same kernel derivation ingest re-runs — so a test
/// can sign a device op (or tamper its signature) BEFORE driving the full `Authority::publish`.
fn genesis_ids(files: &[UploadedFile]) -> ([u8; 32], [u8; 32]) {
    let manifest: Vec<digest::ManifestEntry> = files
        .iter()
        .map(|f| digest::ManifestEntry {
            path: f.path.clone(),
            mode: f.mode,
            content_sha256: digest::sha256(&f.bytes),
        })
        .collect();
    let bd = digest::bundle_digest(&manifest).unwrap();
    let commit = topos_core::sign::commit_id(&topos_core::sign::Commit {
        parents: &[],
        tree: bd,
        author: "d_test",
        message: "topos publish",
    })
    .unwrap();
    (commit, bd)
}

/// An invalid device signature is DENIED with NO durable `op_receipts` row (mirroring the governance
/// preamble's pre-auth posture), the abandoned candidate's lease is still released (its bytes stay
/// GC-reclaimable), and a CORRECTED retry of the SAME op id proceeds fresh — the unauthenticated attempt
/// never burned the idempotency slot.
#[sqlx::test]
async fn a_bad_signature_denied_is_never_persisted_and_a_corrected_retry_succeeds(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-preauth-sig").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(41);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    let files = vec![file("SKILL.md", b"pre-auth denied bytes")];
    let (commit, bd) = genesis_ids(&files);
    let op_id = op("42424242-0000-4000-8000-000000000000");
    let good = sign_op(
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        &op_id,
        gn(0, 0),
        commit,
        bd,
    );
    let mut bad = good;
    bad[0] ^= 1;

    let denied = fx
        .authority
        .publish(
            &w,
            &s,
            &op_id,
            genesis(files.clone()),
            DeviceSignedOp {
                device_key_id: "dk_a".to_owned(),
                op: DeviceOp::PublishDirect,
                signature: bad,
                expected: gn(0, 0),
            },
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(denied.outcome, TerminalOutcome::Denied);
    assert_eq!(
        receipt_rows(&pool, op_id.as_str()).await,
        0,
        "a pre-auth DENIED must not mint a durable attacker-keyed receipt row"
    );
    // The lease was still released, so the unauthenticated upload's bytes are ordinary garbage.
    assert_eq!(gc::run_gc(&fx.authority, &w, NOW).await.unwrap(), 1);

    // The corrected retry (same op id, valid signature) proceeds fresh and succeeds.
    let ok = fx
        .authority
        .publish(
            &w,
            &s,
            &op_id,
            genesis(files),
            DeviceSignedOp {
                device_key_id: "dk_a".to_owned(),
                op: DeviceOp::PublishDirect,
                signature: good,
                expected: gn(0, 0),
            },
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert!(ok.is_ok(), "the corrected retry must succeed: {ok:?}");
    assert_eq!(receipt_rows(&pool, op_id.as_str()).await, 1);
}

/// An UNKNOWN device id is the same synthesized, never-persisted DENIED — for both the pointer-move
/// transaction and the standalone reject transaction.
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

    // Pointer-move path: a publish signed by a never-registered device.
    let ghost_op = op("43000000-0000-4000-8000-000000000001");
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &ghost_op,
            child(c0, vec![file("f", b"v1")]),
            DeviceSignedOp {
                device_key_id: "dk_ghost".to_owned(),
                op: DeviceOp::PublishDirect,
                signature: [0u8; 64],
                expected: gn(1, 1),
            },
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::Denied);
    assert_eq!(receipt_rows(&pool, ghost_op.as_str()).await, 0);

    // Reject path: a review --reject signed by a never-registered device.
    let ghost_reject = op("43000000-0000-4000-8000-000000000002");
    let r = fx
        .authority
        .review_reject(
            &w,
            &s,
            c0,
            DeviceSignedOp {
                device_key_id: "dk_ghost".to_owned(),
                op: DeviceOp::ReviewReject,
                signature: [0u8; 64],
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

/// An AUTHENTICATED denial (a verified device whose principal fails an authorization gate) stays durable
/// and replays byte-identically — the pre-auth carve-out narrows exactly to unverifiable actors.
#[sqlx::test]
async fn an_authenticated_denial_stays_durable_and_replays(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-authz-denied").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(44);
    // Registered device (the signature WILL verify) — but no roster row and no confirmed workspace
    // membership, so the genesis-shaped publish fails the authenticated member gate.
    let p = prin("p_out");
    fx.authority
        .db()
        .seed_device(&w, "dk_out", &key.verifying_key().to_bytes(), &p, false)
        .await
        .unwrap();

    let files = vec![file("SKILL.md", b"outsider genesis")];
    let (commit, bd) = genesis_ids(&files);
    let op_id = op("44000000-0000-4000-8000-000000000000");
    let sig = sign_op(
        &key,
        "dk_out",
        &w,
        &s,
        DeviceOp::PublishDirect,
        &op_id,
        gn(0, 0),
        commit,
        bd,
    );
    let device = DeviceSignedOp {
        device_key_id: "dk_out".to_owned(),
        op: DeviceOp::PublishDirect,
        signature: sig,
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
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(retry, first);
}

// ── a corrupt stored receipt alarms instead of replaying altered bytes ──────────────────────────────

/// A stored receipt whose `details` column no longer parses is an Integrity alarm on replay — never a
/// silent replay with the details dropped (which would violate byte-identical replay without a sound).
#[sqlx::test]
async fn a_corrupt_stored_receipt_details_alarms_instead_of_replaying(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sc-baddetails").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(45);
    register(&fx, &w, &s, "dk_a", &key, "p_dev").await;

    let files = vec![file("SKILL.md", b"replay me")];
    let (commit, bd) = genesis_ids(&files);
    let op_id = op("45000000-0000-4000-8000-000000000000");
    let sig = sign_op(
        &key,
        "dk_a",
        &w,
        &s,
        DeviceOp::PublishDirect,
        &op_id,
        gn(0, 0),
        commit,
        bd,
    );
    let device = DeviceSignedOp {
        device_key_id: "dk_a".to_owned(),
        op: DeviceOp::PublishDirect,
        signature: sig,
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

    // The same-op retry must alarm, never replay an altered receipt.
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &op_id,
            genesis(files),
            device,
            None,
            CREATED_AT,
            NOW,
        )
        .await;
    assert!(
        matches!(r, Err(AuthorityError::Integrity(_))),
        "a corrupt receipt row must alarm as Integrity: {r:?}"
    );
}
