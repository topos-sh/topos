//! Purge, GC, the janitor, and the bundle/workspace reclaims.

use sqlx::PgPool;

use super::support::{Fixture, NOW, bundle, candidate, object_id, ws};
use crate::{AuthorityError, lifecycle};

#[sqlx::test]
async fn purge_refuses_pointed_at_then_tombstones_and_reclaims_unique_blobs(pool: PgPool) {
    let fx = Fixture::new(pool, "purge");
    let (w, b) = (ws("w1"), bundle("b1"));

    // v1 carries a blob v2 KEEPS (shared) and one unique to v1.
    let v1_files = crate::CandidateUpload {
        files: vec![
            super::support::file("GUIDE.md", b"shared"),
            super::support::file("secret.txt", b"unique-to-v1"),
        ],
        parent: None,
        attribution: "Alice (test)".to_owned(),
        message: "test: candidate".to_owned(),
    };
    let (v1, _) = fx
        .authority
        .publish(&w, &b, v1_files, None, NOW)
        .await
        .expect("genesis");
    let (v2, _) = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"shared", Some(v1.version_id)),
            Some(1),
            NOW + 1,
        )
        .await
        .expect("child");

    // Purging the POINTED version is refused typed.
    let err = fx
        .authority
        .purge_version(&w, &b, v2.version_id, "Alice (test)", NOW + 2)
        .await
        .expect_err("pointed-at purge must be refused");
    assert!(matches!(err, AuthorityError::PointedAt));

    // Purge v1: exactly its unique blob is tombstoned + reclaimed; the shared one survives.
    let report = fx
        .authority
        .purge_version(&w, &b, v1.version_id, "Alice (test)", NOW + 3)
        .await
        .expect("purge v1");
    assert_eq!(report.tombstoned, 1);
    assert_eq!(report.reclaimed, 1);
    assert!(matches!(
        fx.authority
            .read_object(&w, &b, object_id(b"unique-to-v1"))
            .await,
        Err(AuthorityError::NotFound)
    ));
    assert_eq!(
        fx.authority
            .read_object(&w, &b, object_id(b"shared"))
            .await
            .expect("shared blob survives"),
        b"shared"
    );

    // The purged version's meta reads NotFound; the log still lists the row with its purge stamp.
    assert!(matches!(
        fx.authority.read_version(&w, &b, v1.version_id).await,
        Err(AuthorityError::NotFound)
    ));
    let log = fx.authority.log(&w, &b, 10).await.expect("log");
    assert_eq!(log.len(), 2);
    assert_eq!(log[1].version_id, v1.version_id);
    assert!(log[1].purged_at_ms.is_some());
    assert!(log[0].purged_at_ms.is_none());

    // Idempotent re-purge: an empty success.
    let again = fx
        .authority
        .purge_version(&w, &b, v1.version_id, "Alice (test)", NOW + 4)
        .await
        .expect("re-purge");
    assert_eq!(again.tombstoned, 0);

    // The denylist holds: re-introducing the purged bytes is refused at ingest.
    let err = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("secret.txt", b"unique-to-v1", Some(v2.version_id)),
            Some(2),
            NOW + 5,
        )
        .await
        .expect_err("a denylisted blob must be refused");
    assert!(matches!(err, AuthorityError::RejectedUpload(_)));
}

#[sqlx::test]
async fn delete_bundle_reclaims_rows_and_unshared_bytes(pool: PgPool) {
    let fx = Fixture::new(pool, "bdelete");
    let (w, b1, b2) = (ws("w1"), bundle("b1"), bundle("b2"));

    // b1 and b2 share one blob; b1 additionally holds a unique one.
    let (v1, _) = fx
        .authority
        .publish(
            &w,
            &b1,
            crate::CandidateUpload {
                files: vec![
                    super::support::file("GUIDE.md", b"shared-across-bundles"),
                    super::support::file("only.txt", b"only-in-b1"),
                ],
                parent: None,
                attribution: "Alice (test)".to_owned(),
                message: "test: candidate".to_owned(),
            },
            None,
            NOW,
        )
        .await
        .expect("b1 genesis");
    let _ = v1;
    fx.authority
        .publish(
            &w,
            &b2,
            candidate("GUIDE.md", b"shared-across-bundles", None),
            None,
            NOW + 1,
        )
        .await
        .expect("b2 genesis");

    let report = fx
        .authority
        .delete_bundle(&w, &b1, NOW + 2)
        .await
        .expect("delete b1");
    assert_eq!(report.versions_dropped, 1);
    assert!(report.objects_reclaimed >= 1, "b1's unique blob reclaimed");

    // b1 is gone (uniform not-found); b2 still serves the shared blob.
    assert!(
        fx.authority
            .read_current(&w, &b1)
            .await
            .expect("read")
            .is_none()
    );
    assert!(matches!(
        fx.authority
            .read_object(&w, &b1, object_id(b"only-in-b1"))
            .await,
        Err(AuthorityError::NotFound)
    ));
    assert_eq!(
        fx.authority
            .read_object(&w, &b2, object_id(b"shared-across-bundles"))
            .await
            .expect("shared survives through b2"),
        b"shared-across-bundles"
    );

    // Idempotent: deleting again drops zero rows.
    let again = fx
        .authority
        .delete_bundle(&w, &b1, NOW + 3)
        .await
        .expect("re-delete");
    assert_eq!(again.versions_dropped, 0);
}

#[sqlx::test]
async fn delete_workspace_drops_rows_and_stores(pool: PgPool) {
    let fx = Fixture::new(pool, "wsdelete");
    let (w, b) = (ws("w1"), bundle("b1"));
    fx.authority
        .publish(&w, &b, candidate("GUIDE.md", b"bytes", None), None, NOW)
        .await
        .expect("genesis");
    let git_dir = fx.dir().join("stores").join("w1");
    assert!(git_dir.is_dir(), "the workspace store exists");

    fx.authority.delete_workspace(&w).await.expect("delete ws");
    assert!(
        fx.authority
            .read_current(&w, &b)
            .await
            .expect("read")
            .is_none()
    );
    assert!(!git_dir.exists(), "the workspace store dir is removed");
    assert!(
        fx.authority
            .workspaces()
            .await
            .expect("workspaces")
            .is_empty(),
        "no presence rows remain"
    );
    // Idempotent.
    fx.authority.delete_workspace(&w).await.expect("re-delete");
}

#[sqlx::test]
async fn the_janitor_sweeps_abandoned_staging_quarantines(pool: PgPool) {
    let fx = Fixture::new(pool, "janitor");
    let (w, b) = (ws("w1"), bundle("b1"));

    // Simulate a crashed ingest: stage a candidate (upload row + quarantine dir) and stop — no
    // migrate, no commit.
    let op = crate::OpId::parse("abandoned-op").expect("op id");
    let staged = lifecycle::ingest(
        &fx.authority,
        &w,
        &b,
        &op,
        candidate("GUIDE.md", b"never-committed", None),
        NOW,
    )
    .await
    .expect("ingest");
    assert!(staged.quarantine_dir.is_dir());

    // Before the TTL: the janitor leaves it alone.
    assert_eq!(fx.authority.run_janitor(NOW + 1).await.expect("janitor"), 0);
    // Past the TTL: swept — the dir is gone.
    let later = NOW + lifecycle::QUARANTINE_TTL_MS + 1;
    assert_eq!(fx.authority.run_janitor(later).await.expect("janitor"), 1);
    assert!(!staged.quarantine_dir.exists());
    // Idempotent: a second pass finds nothing.
    assert_eq!(fx.authority.run_janitor(later).await.expect("janitor"), 0);
}

#[sqlx::test]
async fn gc_spares_everything_a_live_version_roots(pool: PgPool) {
    let fx = Fixture::new(pool, "gcspare");
    let (w, b) = (ws("w1"), bundle("b1"));
    let (v1, _) = fx
        .authority
        .publish(&w, &b, candidate("GUIDE.md", b"keep-me", None), None, NOW)
        .await
        .expect("genesis");

    // Nothing is unrooted — a pass reclaims zero and the bytes still read.
    assert_eq!(fx.authority.run_gc(&w, NOW + 1).await.expect("gc"), 0);
    assert_eq!(
        fx.authority
            .read_object(&w, &b, object_id(b"keep-me"))
            .await
            .expect("still readable"),
        b"keep-me"
    );

    // An un-pointed version (the propose path) is a root too: its bytes are retained.
    let proposal = fx
        .authority
        .commit_version(
            &w,
            &b,
            candidate("GUIDE.md", b"proposed-bytes", Some(v1.version_id)),
            NOW + 2,
        )
        .await
        .expect("proposal");
    assert_eq!(fx.authority.run_gc(&w, NOW + 3).await.expect("gc"), 0);
    assert_eq!(
        fx.authority
            .read_object(&w, &b, object_id(b"proposed-bytes"))
            .await
            .expect("proposal bytes retained"),
        b"proposed-bytes"
    );
    let _ = proposal;
}

#[sqlx::test]
async fn verify_on_read_faults_integrity_never_not_found(pool: PgPool) {
    let fx = Fixture::new(pool, "verify");
    let (w, b) = (ws("w1"), bundle("b1"));
    fx.authority
        .publish(&w, &b, candidate("GUIDE.md", b"trusted", None), None, NOW)
        .await
        .expect("genesis");

    // Vandalize the physical store: the bookkeeping still says the object is present + reachable,
    // so the read must alarm Integrity — never fold into the uniform NotFound.
    let objects_dir = fx.dir().join("stores").join("w1").join("objects");
    std::fs::remove_dir_all(&objects_dir).expect("vandalize the loose objects");
    let err = fx
        .authority
        .read_object(&w, &b, object_id(b"trusted"))
        .await
        .expect_err("a vanished-but-rooted object is corruption");
    assert!(matches!(err, AuthorityError::Integrity(_)), "got {err:?}");
}
