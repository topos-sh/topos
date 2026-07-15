//! Read confinement, the log, the large-object offload, and the ingest caps.

use sqlx::PgPool;

use super::support::{Fixture, NOW, bundle, candidate, object_id, ws};
use crate::AuthorityError;

#[sqlx::test]
async fn reads_are_bundle_confined_and_workspace_confined(pool: PgPool) {
    let fx = Fixture::new(pool, "confine");
    let (w1, w2) = (ws("w1"), ws("w2"));
    let (b1, b2) = (bundle("b1"), bundle("b2"));

    fx.authority
        .publish(&w1, &b1, candidate("GUIDE.md", b"in-b1", None), None, NOW)
        .await
        .expect("w1/b1 genesis");
    fx.authority
        .publish(
            &w1,
            &b2,
            candidate("GUIDE.md", b"in-b2", None),
            None,
            NOW + 1,
        )
        .await
        .expect("w1/b2 genesis");

    // The object exists in the SAME workspace repo, but bundle b2 does not reach it: NotFound.
    assert!(matches!(
        fx.authority
            .read_object(&w1, &b2, object_id(b"in-b1"))
            .await,
        Err(AuthorityError::NotFound)
    ));
    // Another workspace never sees it (nor the version, nor the pointer).
    assert!(matches!(
        fx.authority
            .read_object(&w2, &b1, object_id(b"in-b1"))
            .await,
        Err(AuthorityError::NotFound)
    ));
    assert!(
        fx.authority
            .read_current(&w2, &b1)
            .await
            .expect("read")
            .is_none()
    );

    // A version of b1 is not addressable through b2.
    let v1 = fx
        .authority
        .read_current(&w1, &b1)
        .await
        .expect("read")
        .expect("pointer")
        .version_id;
    assert!(matches!(
        fx.authority.read_version(&w1, &b2, v1).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn the_log_walks_the_first_parent_chain_capped(pool: PgPool) {
    let fx = Fixture::new(pool, "log");
    let (w, b) = (ws("w1"), bundle("b1"));

    // No pointer yet: the uniform NotFound.
    assert!(matches!(
        fx.authority.log(&w, &b, 10).await,
        Err(AuthorityError::NotFound)
    ));

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
    let (v3, _) = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"three", Some(v2.version_id)),
            Some(2),
            NOW + 2,
        )
        .await
        .expect("grandchild");

    let log = fx.authority.log(&w, &b, 10).await.expect("log");
    assert_eq!(
        log.iter().map(|e| e.version_id).collect::<Vec<_>>(),
        vec![v3.version_id, v2.version_id, v1.version_id],
        "newest-first along the first-parent chain"
    );
    assert_eq!(log[0].author_display, "Alice (test)");
    assert_eq!(log[0].message, "test: candidate");
    assert_eq!(log[2].created_at_ms, NOW);

    // The cap truncates the walk.
    let capped = fx.authority.log(&w, &b, 2).await.expect("capped log");
    assert_eq!(capped.len(), 2);
    assert_eq!(capped[0].version_id, v3.version_id);
}

#[sqlx::test]
async fn offloaded_blobs_keep_identity_and_verify_on_read(pool: PgPool) {
    // A 1-byte threshold routes EVERY file blob to the large-object store.
    let fx = Fixture::with_large_limits(pool, "offload", 1, 100 << 20);
    let (w, b) = (ws("w1"), bundle("b1"));

    let (version, _) = fx
        .authority
        .publish(
            &w,
            &b,
            candidate("GUIDE.md", b"offloaded bytes", None),
            None,
            NOW,
        )
        .await
        .expect("genesis");

    // Identity is placement-independent: the version id equals the kernel derivation regardless of
    // which store holds the bytes.
    let expected = topos_core::identity::commit_id(&topos_core::identity::Commit {
        parents: &[],
        tree: version.bundle_digest,
        author: "Alice (test)",
        message: "test: candidate",
    })
    .expect("kernel id");
    assert_eq!(version.version_id.0, expected);

    // The read dispatches to the large store and re-verifies.
    assert_eq!(
        fx.authority
            .read_object(&w, &b, object_id(b"offloaded bytes"))
            .await
            .expect("offloaded read"),
        b"offloaded bytes"
    );
    // The version metadata lists the leaf with its content id (no bytes).
    let meta = fx
        .authority
        .read_version(&w, &b, version.version_id)
        .await
        .expect("meta");
    assert_eq!(meta.files[0].object_id, object_id(b"offloaded bytes").0);
}

#[sqlx::test]
async fn the_reject_cap_refuses_oversize_blobs_at_ingest(pool: PgPool) {
    let fx = Fixture::with_large_limits(pool, "cap", 1, 4);
    let (w, b) = (ws("w1"), bundle("b1"));
    let err = fx
        .authority
        .publish(&w, &b, candidate("big.bin", b"12345", None), None, NOW)
        .await
        .expect_err("a blob over the cap must be refused");
    assert!(matches!(err, AuthorityError::RejectedUpload(_)));
    // Nothing landed.
    assert!(
        fx.authority
            .read_current(&w, &b)
            .await
            .expect("read")
            .is_none()
    );
}

#[sqlx::test]
async fn render_version_reassembles_and_pins_the_digest(pool: PgPool) {
    let fx = Fixture::new(pool, "render");
    let (w, b) = (ws("w1"), bundle("b1"));
    let (version, _) = fx
        .authority
        .publish(
            &w,
            &b,
            crate::CandidateUpload {
                files: vec![
                    super::support::file("GUIDE.md", b"alpha"),
                    super::support::file("scripts/run.sh", b"beta"),
                ],
                parent: None,
                attribution: "Alice (test)".to_owned(),
                message: "test: candidate".to_owned(),
            },
            None,
            NOW,
        )
        .await
        .expect("genesis");

    let rendered = crate::read::render_version(
        &fx.authority,
        &w,
        version.version_id.0,
        version.bundle_digest,
    )
    .await
    .expect("render");
    assert_eq!(rendered.bundle_digest, version.bundle_digest);
    assert_eq!(rendered.files.len(), 2);
    assert_eq!(rendered.files[0].path, "GUIDE.md");
    assert_eq!(rendered.files[0].bytes, b"alpha");
}
