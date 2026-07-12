//! The verb surface's CUSTODY legs — propose-SUPERSEDE, the author WITHDRAW, the mandatory reject
//! REASON on the device lane, and the revert PURGE gate.
//!
//! These drive the contribute/revert write paths through `Authority` against a real Postgres + git
//! store: a fresh proposal closes the author's earlier drafts (and only theirs); a withdraw is
//! author-only, closes as `withdrawn` with no verdict notice, and replays idempotently; an empty
//! reject reason is a typed synthesized denial that never poisons the op-id slot, while a real one
//! lands on the row AND the author's verdict notice; and a purged version can never be a revert
//! target, on either lane.

use super::*;

use topos_types::TerminalOutcome as TO;

use crate::catalog::PurgeOutcome;

/// A proposal's `(status, resolved_by, resolved_reason)` for a (skill, commit) pair, newest-first
/// preference mirroring the detail read (raw `sqlx`, the test's own eye).
async fn proposal_state(
    pool: &PgPool,
    ws: &str,
    commit: &CommitId,
) -> Option<(String, Option<String>, Option<String>)> {
    sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
        "SELECT status, resolved_by, resolved_reason FROM proposals \
         WHERE workspace_id = $1 AND commit_id = $2 \
         ORDER BY CASE status WHEN 'open' THEN 0 ELSE 1 END, created_at DESC LIMIT 1",
    )
    .bind(ws)
    .bind(commit.0.as_slice())
    .fetch_optional(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn a_fresh_proposal_supersedes_only_the_authors_other_open_drafts(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsc-supersede").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let alice_key = dev_key(61);
    let bob_key = dev_key(62);
    register(&fx, &w, &s, "dk_alice", &alice_key, "alice@acme.com").await;
    register(&fx, &w, &s, "dk_bob", &bob_key, "bob@acme.com").await;
    publish(
        &fx,
        &alice_key,
        "dk_alice",
        &w,
        &s,
        "61000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v1")]),
        gn(0, 0),
    )
    .await;
    let base_commit = current_commit(&fx, &w, &s).await;

    // Alice's first draft + bob's draft, both open on the same base.
    let (r1, alice_c1, _) = do_propose(
        &fx,
        &alice_key,
        "dk_alice",
        &w,
        &s,
        "61000000-0000-4000-8000-000000000002",
        child(base_commit, vec![file("SKILL.md", b"alice draft 1")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(r1.outcome, TO::NeedsReview);
    let (rb, bob_c, _) = do_propose(
        &fx,
        &bob_key,
        "dk_bob",
        &w,
        &s,
        "62000000-0000-4000-8000-000000000001",
        child(base_commit, vec![file("SKILL.md", b"bob draft")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(rb.outcome, TO::NeedsReview);

    // Alice's SECOND draft supersedes her first — and ONLY hers: bob's stays open.
    let (r2, alice_c2, _) = do_propose(
        &fx,
        &alice_key,
        "dk_alice",
        &w,
        &s,
        "61000000-0000-4000-8000-000000000003",
        child(base_commit, vec![file("SKILL.md", b"alice draft 2")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(r2.outcome, TO::NeedsReview);
    assert_eq!(
        proposal_state(&pool, "w_acme", &alice_c1).await,
        Some((
            "closed".to_owned(),
            Some("alice@acme.com".to_owned()),
            Some("superseded".to_owned())
        )),
        "the author's earlier draft closed as superseded (no verdict notice — the author did it)"
    );
    assert_eq!(
        proposal_state(&pool, "w_acme", &bob_c).await,
        Some(("open".to_owned(), None, None)),
        "another author's draft is untouched"
    );
    assert_eq!(
        proposal_state(&pool, "w_acme", &alice_c2).await,
        Some(("open".to_owned(), None, None))
    );

    // The idempotent re-propose of the SAME (candidate, base) still converges: NEEDS_REVIEW, the open
    // row survives its own supersede pass (only rows whose commit DIFFERS close).
    let (r3, alice_c2_again, _) = do_propose(
        &fx,
        &alice_key,
        "dk_alice",
        &w,
        &s,
        "61000000-0000-4000-8000-000000000004",
        child(base_commit, vec![file("SKILL.md", b"alice draft 2")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(r3.outcome, TO::NeedsReview);
    assert_eq!(alice_c2_again, alice_c2);
    assert_eq!(
        proposal_state(&pool, "w_acme", &alice_c2).await,
        Some(("open".to_owned(), None, None))
    );
    // No notice was written for the superseded draft (the author acted on their own work).
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_alice"))
        .await
        .unwrap();
    assert!(
        d.notices.is_empty(),
        "supersede writes no notice: {:?}",
        d.notices
    );
}

#[sqlx::test]
async fn withdraw_is_author_only_closes_as_withdrawn_and_replays_idempotently(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsc-withdraw").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let alice_key = dev_key(63);
    let bob_key = dev_key(64);
    register(&fx, &w, &s, "dk_alice", &alice_key, "alice@acme.com").await;
    register(&fx, &w, &s, "dk_bob", &bob_key, "bob@acme.com").await;
    publish(
        &fx,
        &alice_key,
        "dk_alice",
        &w,
        &s,
        "63000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v1")]),
        gn(0, 0),
    )
    .await;
    let base_commit = current_commit(&fx, &w, &s).await;
    let (_, cand, _) = do_propose(
        &fx,
        &alice_key,
        "dk_alice",
        &w,
        &s,
        "63000000-0000-4000-8000-000000000002",
        child(base_commit, vec![file("SKILL.md", b"draft")]),
        gn(1, 1),
    )
    .await;

    // BOB (a confirmed member, not the author) cannot withdraw alice's draft — a typed DURABLE denial.
    let bob_attempt = fx
        .authority
        .review_withdraw(
            &w,
            &s,
            cand,
            DeviceOpAuth {
                credential: cred(&w, "dk_bob"),
                op: DeviceOp::ReviewWithdraw,
                expected: gn(1, 1),
            },
            &op("64000000-0000-4000-8000-000000000001"),
            CREATED_AT,
        )
        .await
        .unwrap();
    assert_eq!(bob_attempt.outcome, TO::Denied);
    assert_eq!(
        proposal_state(&pool, "w_acme", &cand).await,
        Some(("open".to_owned(), None, None)),
        "the denied withdraw closed nothing"
    );
    // The denial is DURABLE (bob is a verified member — he is owed a replayable answer).
    let bob_replay = fx
        .authority
        .review_withdraw(
            &w,
            &s,
            cand,
            DeviceOpAuth {
                credential: cred(&w, "dk_bob"),
                op: DeviceOp::ReviewWithdraw,
                expected: gn(1, 1),
            },
            &op("64000000-0000-4000-8000-000000000001"),
            "some-other-time",
        )
        .await
        .unwrap();
    assert_eq!(bob_replay, bob_attempt, "byte-identical replay");

    // ALICE withdraws her own draft: closed as `withdrawn`, no verdict notice.
    let withdrawn = fx
        .authority
        .review_withdraw(
            &w,
            &s,
            cand,
            DeviceOpAuth {
                credential: cred(&w, "dk_alice"),
                op: DeviceOp::ReviewWithdraw,
                expected: gn(1, 1),
            },
            &op("63000000-0000-4000-8000-000000000003"),
            CREATED_AT,
        )
        .await
        .unwrap();
    assert_eq!(withdrawn.outcome, TO::Ok);
    assert_eq!(
        withdrawn
            .details
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str()),
        Some("PROPOSAL_WITHDRAWN")
    );
    assert_eq!(
        proposal_state(&pool, "w_acme", &cand).await,
        Some((
            "closed".to_owned(),
            Some("alice@acme.com".to_owned()),
            Some("withdrawn".to_owned())
        ))
    );
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_alice"))
        .await
        .unwrap();
    assert!(
        d.notices.is_empty(),
        "a withdraw writes no verdict notice: {:?}",
        d.notices
    );

    // A lost-ack retry under a NEW op id replays idempotently (the author's own withdrawn row).
    let again = fx
        .authority
        .review_withdraw(
            &w,
            &s,
            cand,
            DeviceOpAuth {
                credential: cred(&w, "dk_alice"),
                op: DeviceOp::ReviewWithdraw,
                expected: gn(1, 1),
            },
            &op("63000000-0000-4000-8000-000000000004"),
            CREATED_AT,
        )
        .await
        .unwrap();
    assert_eq!(again.outcome, TO::Ok);
    assert_eq!(
        again
            .details
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str()),
        Some("PROPOSAL_ALREADY_WITHDRAWN")
    );

    // A late REJECT of the withdrawn proposal must not overwrite its story.
    let late = do_reject(
        &fx,
        &bob_key,
        "dk_bob",
        &w,
        &s,
        "64000000-0000-4000-8000-000000000002",
        cand,
        [0u8; 32],
        gn(1, 1),
    )
    .await;
    assert_eq!(late.outcome, TO::Denied);
    assert_eq!(
        proposal_state(&pool, "w_acme", &cand).await,
        Some((
            "closed".to_owned(),
            Some("alice@acme.com".to_owned()),
            Some("withdrawn".to_owned())
        ))
    );
}

#[sqlx::test]
async fn a_device_reject_requires_a_reason_and_the_notice_carries_it(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsc-reason").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let alice_key = dev_key(65);
    let bob_key = dev_key(66);
    register(&fx, &w, &s, "dk_alice", &alice_key, "alice@acme.com").await;
    register(&fx, &w, &s, "dk_bob", &bob_key, "bob@acme.com").await;
    publish(
        &fx,
        &alice_key,
        "dk_alice",
        &w,
        &s,
        "65000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v1")]),
        gn(0, 0),
    )
    .await;
    let base_commit = current_commit(&fx, &w, &s).await;
    let (_, cand, _) = do_propose(
        &fx,
        &alice_key,
        "dk_alice",
        &w,
        &s,
        "65000000-0000-4000-8000-000000000002",
        child(base_commit, vec![file("SKILL.md", b"draft")]),
        gn(1, 1),
    )
    .await;

    // An empty/whitespace reason is a typed Denied — SYNTHESIZED (the same op id, corrected, must not
    // replay this refusal: the reason is not part of the bound identity).
    let reject_op = op("66000000-0000-4000-8000-000000000001");
    let empty = fx
        .authority
        .review_reject(
            &w,
            &s,
            cand,
            DeviceOpAuth {
                credential: cred(&w, "dk_bob"),
                op: DeviceOp::ReviewReject,
                expected: gn(1, 1),
            },
            "   ",
            &reject_op,
            CREATED_AT,
        )
        .await
        .unwrap();
    assert_eq!(empty.outcome, TO::Denied);
    assert_eq!(
        empty
            .details
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str()),
        Some(crate::REASON_REQUIRED_CODE)
    );
    assert_eq!(
        proposal_state(&pool, "w_acme", &cand).await,
        Some(("open".to_owned(), None, None)),
        "the refused reject flipped nothing"
    );

    // The SAME op id with a real reason now LANDS (nothing durable poisoned the slot), the reason on
    // the row…
    let rejected = fx
        .authority
        .review_reject(
            &w,
            &s,
            cand,
            DeviceOpAuth {
                credential: cred(&w, "dk_bob"),
                op: DeviceOp::ReviewReject,
                expected: gn(1, 1),
            },
            "too broad — split it",
            &reject_op,
            CREATED_AT,
        )
        .await
        .unwrap();
    assert_eq!(rejected.outcome, TO::Ok);
    assert_eq!(
        proposal_state(&pool, "w_acme", &cand).await,
        Some((
            "rejected".to_owned(),
            Some("bob@acme.com".to_owned()),
            Some("too broad — split it".to_owned())
        ))
    );
    // …and on the AUTHOR's verdict notice.
    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_alice"))
        .await
        .unwrap();
    let verdict = d
        .notices
        .iter()
        .find(|n| n.kind == "verdict")
        .expect("the author gets a verdict notice");
    assert_eq!(verdict.outcome.as_deref(), Some("rejected"));
    assert_eq!(verdict.reason.as_deref(), Some("too broad — split it"));
}

#[sqlx::test]
async fn a_purged_version_is_never_a_revert_target_on_either_lane(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "vsc-purge").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(67);
    register(&fx, &w, &s, "dk_owner", &key, "alice@acme.com").await;
    // The purge ceremony is an OWNER session op; raise alice's seat.
    fx.authority
        .db()
        .seed_workspace_member(&w, &prin("alice@acme.com"), "owner", "confirmed")
        .await
        .unwrap();

    // Genesis (v1) named "Deploy" (the purge keys on the immutable skill id), then v2 on top.
    let (staged, device) = prepare(
        &fx,
        &key,
        "dk_owner",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "67000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v1")]),
        gn(0, 0),
    )
    .await;
    crate::set_current::publish(
        &fx.authority,
        &w,
        &s,
        &staged,
        &device,
        Some("Deploy"),
        None,
        CREATED_AT,
        NOW,
    )
    .await
    .unwrap();
    let v1 = current_commit(&fx, &w, &s).await;
    publish(
        &fx,
        &key,
        "dk_owner",
        &w,
        &s,
        "67000000-0000-4000-8000-000000000002",
        child(v1, vec![file("SKILL.md", b"v2")]),
        gn(1, 1),
    )
    .await;

    // Purge v1 (not current — allowed).
    assert_eq!(
        fx.authority
            .purge_version_session(
                &w,
                "alice@acme.com",
                "s_deploy",
                v1,
                DeploymentMode::Cloud,
                CREATED_AT,
                NOW,
            )
            .await
            .unwrap(),
        PurgeOutcome::Purged
    );

    // The DEVICE-lane revert to the purged v1 is a typed refusal BEFORE any staging.
    let refused = fx
        .authority
        .revert(
            &w,
            &s,
            v1,
            revert_request(&w, "dk_owner", gn(1, 2)),
            "alice@acme.com",
            "topos: revert",
            &op("67000000-0000-4000-8000-000000000003"),
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(refused.outcome, TO::Denied);
    assert_eq!(
        refused
            .details
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str()),
        Some("TARGET_PURGED")
    );
    // Nothing moved.
    assert_ne!(current_commit(&fx, &w, &s).await, v1);

    // The SESSION revert hits the SAME gate (alice's owner seat authorizes; the target is refused).
    let session_refused = fx
        .authority
        .revert_session(
            &w,
            &s,
            v1,
            gn(1, 2),
            "67000000-0000-4000-8000-000000000004",
            "alice@acme.com",
            DeploymentMode::Cloud,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(session_refused.outcome, TO::Denied);
    assert_eq!(
        session_refused
            .details
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(|c| c.as_str()),
        Some("TARGET_PURGED")
    );
}

/// A version's purge tombstone (`skill_commit.purged_at`).
async fn purge_tombstone(pool: &PgPool, ws: &str, commit: &CommitId) -> Option<i64> {
    sqlx::query_scalar::<_, Option<i64>>(
        "SELECT purged_at FROM skill_commit WHERE workspace_id = $1 AND commit_id = $2",
    )
    .bind(ws)
    .bind(commit.0.as_slice())
    .fetch_one(pool)
    .await
    .unwrap()
}

#[sqlx::test]
async fn re_introducing_a_purged_version_clears_its_tombstone(pool: PgPool) {
    // A purged version's bytes can legitimately reappear (the publisher re-publishes the identical
    // content — content addressing re-derives the SAME commit id). Re-rooting the bytes makes the
    // version LIVE again, so its purge tombstone must clear: a stale `purged_at` would keep refusing a
    // revert to now-present bytes and make a fresh purge see `already_purged`.
    let fx = Fixture::new(pool.clone(), "vsc-repurge").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(69);
    register(&fx, &w, &s, "dk_owner", &key, "alice@acme.com").await;
    fx.authority
        .db()
        .seed_workspace_member(&w, &prin("alice@acme.com"), "owner", "confirmed")
        .await
        .unwrap();

    // Genesis v1 (current) named "Deploy" (ops key on the id); a proposal V rides on it (not current — purgeable).
    let (staged, device) = prepare(
        &fx,
        &key,
        "dk_owner",
        &w,
        &s,
        DeviceOp::PublishDirect,
        "69000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v1")]),
        gn(0, 0),
    )
    .await;
    crate::set_current::publish(
        &fx.authority,
        &w,
        &s,
        &staged,
        &device,
        Some("Deploy"),
        None,
        CREATED_AT,
        NOW,
    )
    .await
    .unwrap();
    let v1 = current_commit(&fx, &w, &s).await;
    let (rp, vcommit, _) = do_propose(
        &fx,
        &key,
        "dk_owner",
        &w,
        &s,
        "69000000-0000-4000-8000-000000000002",
        child(v1, vec![file("SKILL.md", b"secret leaked")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(rp.outcome, TO::NeedsReview);

    // Purge V (a proposal commit, never current — allowed): the tombstone lands, the proposal closes.
    assert_eq!(
        fx.authority
            .purge_version_session(
                &w,
                "alice@acme.com",
                "s_deploy",
                vcommit,
                DeploymentMode::Cloud,
                CREATED_AT,
                NOW,
            )
            .await
            .unwrap(),
        PurgeOutcome::Purged
    );
    assert!(
        purge_tombstone(&pool, "w_acme", &vcommit).await.is_some(),
        "the purge tombstones V"
    );

    // Re-propose the IDENTICAL draft over the same base → the SAME commit id V, re-ingesting its bytes.
    let (rp2, vcommit2, _) = do_propose(
        &fx,
        &key,
        "dk_owner",
        &w,
        &s,
        "69000000-0000-4000-8000-000000000003",
        child(v1, vec![file("SKILL.md", b"secret leaked")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(rp2.outcome, TO::NeedsReview);
    assert_eq!(
        vcommit2, vcommit,
        "content addressing re-derives the same id"
    );
    assert_eq!(
        purge_tombstone(&pool, "w_acme", &vcommit).await,
        None,
        "re-introducing the bytes clears the tombstone — the version is live again"
    );
}
