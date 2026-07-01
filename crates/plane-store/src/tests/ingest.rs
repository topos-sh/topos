//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn ingest_rejects_an_empty_or_malformed_bundle(pool: PgPool) {
    let fx = Fixture::new(pool, "ingest-reject").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    // Empty: the authority rejects a zero-file bundle itself (the git store would happily snapshot a
    // zero-entry tree; the client scanner cannot be trusted to have enforced this).
    assert!(matches!(
        lifecycle::ingest(a, &w, &op("empty"), genesis(vec![]), 100).await,
        Err(AuthorityError::RejectedUpload(_))
    ));
    // A forbidden path: the canonical reject rules fire ONCE, inside the kernel during staging.
    assert!(matches!(
        lifecycle::ingest(
            a,
            &w,
            &op("badpath"),
            genesis(vec![file("/abs/forbidden", b"x")]),
            100,
        )
        .await,
        Err(AuthorityError::RejectedUpload(_))
    ));
}

#[sqlx::test]
async fn foreign_key_is_enforced_a_dangling_pointer_insert_is_rejected(pool: PgPool) {
    let fx = Fixture::new(pool, "fk").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    // Seeding `current` for a commit with no provenance violates the foreign key (proving foreign_keys
    // is ON — silently ignored otherwise).
    let res = a
        .db()
        .seed_current(&w, &s, CommitId([0x22; 32]), 1, 1)
        .await;
    assert!(
        res.is_err(),
        "a dangling current insert must be rejected by the FK"
    );
}
