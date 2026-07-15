//! The write surface: ingest/dedup, the generation-fenced CAS (+ the idempotent-replay carve-out),
//! the same-bundle lineage fence, the approve-path pointer move, and the forward revert.

use sqlx::PgPool;

use super::support::{Fixture, NOW, bundle, candidate, ws};
use crate::{AuthorityError, CandidateUpload, UploadedFile};

#[sqlx::test]
async fn genesis_publish_creates_the_pointer_and_serves_reads(pool: PgPool) {
    let fx = Fixture::new(pool, "genesis");
    let (w, b) = (ws("w1"), bundle("b1"));

    let (version, pointer) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"hello", None), None, NOW)
        .await
        .expect("genesis publish");
    assert!(!version.deduped);
    assert_eq!(pointer.generation, 1);
    assert!(!pointer.replayed);
    assert_eq!(pointer.version_id, version.version_id);
    assert_eq!(pointer.moved_by, "Alice (test)");

    // The identity is the KERNEL commit id over the candidate frame — placement-independent and
    // reproducible by any component.
    let expected = topos_core::identity::commit_id(&topos_core::identity::Commit {
        parents: &[],
        tree: version.bundle_digest,
        author: "Alice (test)",
        message: "test: candidate",
    })
    .expect("kernel id");
    assert_eq!(version.version_id.0, expected);

    let current = fx
        .authority
        .read_current(&w, &b)
        .await
        .expect("read current")
        .expect("pointer exists");
    assert_eq!(current.generation, 1);
    assert_eq!(current.version_id, version.version_id);
    assert_eq!(current.bundle_digest, version.bundle_digest);
    assert_eq!(current.moved_at_ms, NOW);

    let meta = fx
        .authority
        .read_version(&w, &b, version.version_id)
        .await
        .expect("read version");
    assert_eq!(meta.files.len(), 1);
    assert_eq!(meta.files[0].path, "GUIDE.md");
    assert_eq!(meta.author, "Alice (test)");
    assert!(meta.parents.is_empty());

    let bytes = fx
        .authority
        .read_object(&w, &b, super::support::object_id(b"hello"))
        .await
        .expect("read object");
    assert_eq!(bytes, b"hello");
}

#[sqlx::test]
async fn publish_cas_advances_and_a_stale_writer_conflicts_without_a_version_row(pool: PgPool) {
    let fx = Fixture::new(pool, "cas");
    let (w, b) = (ws("w1"), bundle("b1"));

    let (v1, _) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"one", None), None, NOW)
        .await
        .expect("genesis");
    let (v2, p2) = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"two", Some(v1.version_id)),
            Some(1),
            NOW + 1,
        )
        .await
        .expect("child publish");
    assert_eq!(p2.generation, 2);

    // A stale writer (still expecting generation 1) with DIFFERENT content: the typed CONFLICT
    // carrying the live pointer, and NO version row left behind.
    let stale = candidate("GUIDE.md", b"stale", Some(v1.version_id));
    let stale_id = topos_core::identity::commit_id(&topos_core::identity::Commit {
        parents: &[v1.version_id.0],
        tree: topos_core::digest::bundle_digest(&[topos_core::digest::ManifestEntry {
            path: "GUIDE.md".into(),
            mode: crate::FileMode::Regular,
            content_sha256: topos_core::digest::sha256(b"stale"),
        }])
        .expect("digest"),
        author: "Alice (test)",
        message: "test: candidate",
    })
    .expect("kernel id");
    let err = fx
        .authority
        .publish(&w, &b, stale, Some(1), NOW + 2)
        .await
        .expect_err("stale CAS must conflict");
    match err {
        AuthorityError::Conflict(Some(live)) => {
            assert_eq!(live.generation, 2);
            assert_eq!(live.version_id, v2.version_id);
        }
        other => panic!("expected Conflict, got {other:?}"),
    }
    assert!(matches!(
        fx.authority
            .read_version(&w, &b, crate::CommitId(stale_id))
            .await,
        Err(AuthorityError::NotFound)
    ));

    // The conflicted candidate's unique bytes are unrooted once its lease is released — one GC pass
    // reclaims them (and spares everything the live versions root).
    let reclaimed = fx.authority.run_gc(&w, NOW + 3).await.expect("gc");
    assert!(reclaimed >= 1, "the stale blob must be reclaimed");
    assert_eq!(
        fx.authority
            .read_object(&w, &b, super::support::object_id(b"two"))
            .await
            .expect("live object still reads"),
        b"two"
    );
}

#[sqlx::test]
async fn identical_candidates_converge_and_retries_replay_idempotently(pool: PgPool) {
    let fx = Fixture::new(pool, "idem");
    let (w, b) = (ws("w1"), bundle("b1"));

    // commit_version (the propose path) twice: same ids, second deduped.
    let first = fx
        .authority
        .commit_version(&w, &b, candidate("GUIDE.md", b"draft", None), NOW)
        .await
        .expect("first commit");
    assert!(!first.deduped);
    let second = fx
        .authority
        .commit_version(&w, &b, candidate("GUIDE.md", b"draft", None), NOW + 1)
        .await
        .expect("second commit");
    assert!(second.deduped);
    assert_eq!(second.version_id, first.version_id);
    assert_eq!(second.bundle_digest, first.bundle_digest);

    // Genesis publish + an identical retry: the retry lands on the idempotent-CAS carve-out.
    let (v1, p1) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"one", None), None, NOW + 2)
        .await
        .expect("genesis");
    assert_eq!(p1.generation, 1);
    let (v1b, p1b) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"one", None), None, NOW + 3)
        .await
        .expect("genesis retry");
    assert!(v1b.deduped);
    assert!(p1b.replayed);
    assert_eq!(p1b.generation, 1);
    assert_eq!(v1b.version_id, v1.version_id);

    // A DIFFERENT genesis candidate conflicts (the pointer exists).
    let err = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"other", None), None, NOW + 4)
        .await
        .expect_err("a second distinct genesis must conflict");
    assert!(matches!(err, AuthorityError::Conflict(Some(_))));

    // A child publish + an identical retry: the retry replays at generation expected+1.
    let (v2, p2) = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"two", Some(v1.version_id)),
            Some(1),
            NOW + 5,
        )
        .await
        .expect("child");
    assert_eq!(p2.generation, 2);
    let (v2b, p2b) = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"two", Some(v1.version_id)),
            Some(1),
            NOW + 6,
        )
        .await
        .expect("child retry");
    assert!(p2b.replayed);
    assert_eq!(p2b.generation, 2);
    assert_eq!(v2b.version_id, v2.version_id);
}

#[sqlx::test]
async fn the_lineage_fence_and_the_parent_probe_refuse_typed(pool: PgPool) {
    let fx = Fixture::new(pool, "lineage");
    let (w, b) = (ws("w1"), bundle("b1"));

    let (v1, _) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"one", None), None, NOW)
        .await
        .expect("genesis");
    let (v2, _) = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"two", Some(v1.version_id)),
            Some(1),
            NOW + 1,
        )
        .await
        .expect("child");
    let _ = v2;

    // A publish whose first parent is NOT the pointed version (v1, while the pointer names v2) at
    // the CORRECT generation is a lineage violation, refused typed.
    let err = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"fork", Some(v1.version_id)),
            Some(2),
            NOW + 2,
        )
        .await
        .expect_err("a wrong first parent at the right generation must be refused");
    assert!(matches!(err, AuthorityError::RejectedUpload(_)));

    // A declared parent that is not a version of THIS bundle is refused before anything lands.
    let ghost = crate::CommitId([0x5a; 32]);
    let err = fx
        .authority
        .commit_version(&w, &b, candidate("GUIDE.md", b"x", Some(ghost)), NOW + 3)
        .await
        .expect_err("an unknown parent must be refused");
    assert!(matches!(err, AuthorityError::RejectedUpload(_)));
}

#[sqlx::test]
async fn move_pointer_serves_the_approve_path(pool: PgPool) {
    let fx = Fixture::new(pool, "approve");
    let (w, b) = (ws("w1"), bundle("b1"));

    let (v1, _) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"one", None), None, NOW)
        .await
        .expect("genesis");
    // The propose path: a version current does not point at.
    let proposal = fx
        .authority
        .commit_version(
            &w,
            &b,
            candidate("GUIDE.md", b"proposed", Some(v1.version_id)),
            NOW + 1,
        )
        .await
        .expect("proposal commit");

    // Approve = CAS to the existing version.
    let moved = fx
        .authority
        .move_pointer(
            &w,
            &b,
            proposal.version_id,
            Some(1),
            "Reviewer Bob",
            NOW + 2,
        )
        .await
        .expect("approve move");
    assert_eq!(moved.generation, 2);
    assert_eq!(moved.version_id, proposal.version_id);
    assert_eq!(moved.moved_by, "Reviewer Bob");

    // The idempotent retry replays; an unknown target is the uniform NotFound.
    let replay = fx
        .authority
        .move_pointer(
            &w,
            &b,
            proposal.version_id,
            Some(1),
            "Reviewer Bob",
            NOW + 3,
        )
        .await
        .expect("replay");
    assert!(replay.replayed);
    assert_eq!(replay.generation, 2);
    assert!(matches!(
        fx.authority
            .move_pointer(&w, &b, crate::CommitId([9; 32]), Some(2), "Bob", NOW + 4)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn approve_of_a_stale_proposal_conflicts_rather_than_fast_forwarding(pool: PgPool) {
    // Two proposals open against the same base; approving the first advances current, so
    // approving the second (still based on the old current) must CONFLICT — never silently
    // fast-forward over the first, discarding it.
    let fx = Fixture::new(pool, "stale");
    let (w, b) = (ws("w1"), bundle("b1"));

    let (v1, _) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"one", None), None, NOW)
        .await
        .expect("genesis");

    // Proposal A and proposal B, both committed against v1 (both parents = v1).
    let prop_a = fx
        .authority
        .commit_version(
            &w,
            &b,
            candidate("GUIDE.md", b"A", Some(v1.version_id)),
            NOW + 1,
        )
        .await
        .expect("proposal A");
    let prop_b = fx
        .authority
        .commit_version(
            &w,
            &b,
            candidate("GUIDE.md", b"B", Some(v1.version_id)),
            NOW + 2,
        )
        .await
        .expect("proposal B");

    // Approve A: current advances v1(gen 1) -> A(gen 2).
    let moved = fx
        .authority
        .move_pointer(&w, &b, prop_a.version_id, Some(1), "Reviewer", NOW + 3)
        .await
        .expect("approve A");
    assert_eq!(moved.generation, 2);
    assert_eq!(moved.version_id, prop_a.version_id);

    // Approve B (parent v1, but current is now A) at the live generation — MUST conflict with the
    // live pointer, not move. Both a "current generation" and the stale base generation are refused.
    let at_current = fx
        .authority
        .move_pointer(&w, &b, prop_b.version_id, Some(2), "Reviewer", NOW + 4)
        .await;
    match at_current {
        Err(AuthorityError::Conflict(Some(live))) => {
            assert_eq!(live.generation, 2);
            assert_eq!(live.version_id, prop_a.version_id);
        }
        other => panic!("stale approve at current gen must conflict, got {other:?}"),
    }
    assert!(matches!(
        fx.authority
            .move_pointer(&w, &b, prop_b.version_id, Some(1), "Reviewer", NOW + 5)
            .await,
        Err(AuthorityError::Conflict(_))
    ));

    // A's change survived: current still names A.
    let current = fx
        .authority
        .read_current(&w, &b)
        .await
        .expect("read current");
    assert_eq!(current.expect("some").version_id, prop_a.version_id);
}

#[sqlx::test]
async fn revert_is_a_forward_commit_and_honors_the_purged_refusal(pool: PgPool) {
    let fx = Fixture::new(pool, "revert");
    let (w, b) = (ws("w1"), bundle("b1"));

    let (v1, _) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"good", None), None, NOW)
        .await
        .expect("genesis");
    let (v2, _) = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"bad", Some(v1.version_id)),
            Some(1),
            NOW + 1,
        )
        .await
        .expect("child");

    let (rv, rp) = fx
        .authority
        .revert(
            &w,
            &b,
            v1.version_id,
            2,
            "Alice (test)",
            "revert (test)",
            NOW + 2,
        )
        .await
        .expect("revert");
    assert_eq!(rp.generation, 3, "a revert moves FORWARD");
    assert_ne!(rv.version_id, v1.version_id, "a fresh forward commit");
    assert_eq!(rv.bundle_digest, v1.bundle_digest, "the target's bytes");
    assert_eq!(
        fx.authority
            .read_object(&w, &b, super::support::object_id(b"good"))
            .await
            .expect("the good bytes read"),
        b"good"
    );

    // The idempotent retry (same target, same expected generation) replays without staging again.
    let (rv2, rp2) = fx
        .authority
        .revert(
            &w,
            &b,
            v1.version_id,
            2,
            "Alice (test)",
            "revert (test)",
            NOW + 3,
        )
        .await
        .expect("revert retry");
    assert!(rp2.replayed);
    assert_eq!(rp2.generation, 3);
    assert_eq!(rv2.version_id, rv.version_id);

    // A purged target is refused typed (v2 is now un-pointed and purgeable).
    fx.authority
        .purge_version(&w, &b, v2.version_id, "Alice (test)", NOW + 4)
        .await
        .expect("purge v2");
    let err = fx
        .authority
        .revert(
            &w,
            &b,
            v2.version_id,
            3,
            "Alice (test)",
            "revert (test)",
            NOW + 5,
        )
        .await
        .expect_err("reverting to a purged target must be refused");
    assert!(matches!(err, AuthorityError::TargetPurged));

    // A stale expected generation is the typed CONFLICT.
    let err = fx
        .authority
        .revert(
            &w,
            &b,
            v1.version_id,
            1,
            "Alice (test)",
            "revert (test)",
            NOW + 6,
        )
        .await
        .expect_err("a stale revert must conflict");
    assert!(matches!(err, AuthorityError::Conflict(Some(_))));
}

#[sqlx::test]
async fn racing_publishes_yield_one_ok_and_one_stable_conflict(pool: PgPool) {
    let fx = Fixture::new(pool, "race");
    let (w, b) = (ws("w1"), bundle("b1"));
    let (v1, _) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"base", None), None, NOW)
        .await
        .expect("genesis");

    let a = fx.authority.publish(
        &w,
        &b,
        candidate("GUIDE.md", b"racer-a", Some(v1.version_id)),
        Some(1),
        NOW + 1,
    );
    let bfut = fx.authority.publish(
        &w,
        &b,
        candidate("GUIDE.md", b"racer-b", Some(v1.version_id)),
        Some(1),
        NOW + 1,
    );
    let (ra, rb) = tokio::join!(a, bfut);
    let oks = [ra.is_ok(), rb.is_ok()].iter().filter(|x| **x).count();
    assert_eq!(oks, 1, "exactly one racer lands");
    for r in [ra, rb] {
        if let Err(e) = r {
            assert!(matches!(e, AuthorityError::Conflict(Some(_))), "got {e:?}");
        }
    }
    let current = fx
        .authority
        .read_current(&w, &b)
        .await
        .expect("read")
        .expect("pointer");
    assert_eq!(current.generation, 2);
}

#[sqlx::test]
async fn attribution_is_shape_checked(pool: PgPool) {
    let fx = Fixture::new(pool, "attr");
    let (w, b) = (ws("w1"), bundle("b1"));
    for bad in ["", "a\nb"] {
        let cand = CandidateUpload {
            files: vec![UploadedFile {
                path: "GUIDE.md".into(),
                mode: crate::FileMode::Regular,
                bytes: b"x".to_vec(),
            }],
            parent: None,
            attribution: bad.to_owned(),
            message: "m".into(),
        };
        assert!(matches!(
            fx.authority.publish(&w, &b, cand, None, NOW).await,
            Err(AuthorityError::InvalidId(_))
        ));
    }
    let long = "x".repeat(201);
    assert!(matches!(
        fx.authority
            .move_pointer(&w, &b, crate::CommitId([1; 32]), None, &long, NOW)
            .await,
        Err(AuthorityError::InvalidId(_))
    ));
}
