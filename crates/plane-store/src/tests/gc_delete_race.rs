//! The delayed-delete-vs-reinstall race, replayed end-to-end (the gate-flaw-1 fault injection):
//!
//! 1. a GC acquires an unrooted object (token A) and its DELETE faults ambiguously;
//! 2. the row is left `deleting` — absence is NEVER finalized on ambiguity;
//! 3. the recovery sweep (once the row is stale) re-acquires under token B, re-runs the idempotent
//!    delete (which succeeds), and finalizes `absent`;
//! 4. an ingest re-uploads the same content and commits `present`;
//! 5. the ORIGINAL attempt can never land late — the seam issued it exactly once (no transport
//!    retries) inside a hard wall-clock bound strictly below the staleness threshold, so by the
//!    time token B (and the reinstall) can exist, attempt A is dead. The reinstalled bytes are
//!    readable and verified: Postgres never says `present` over missing bytes.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use sqlx::PgPool;

use crate::store::PlaneStore;
use crate::tests::store_contract::{FaultFirstDelete, FirstDeleteFault};
use crate::tests::support::{NOW, bundle, candidate, object_id, ws};

#[sqlx::test]
async fn an_ambiguous_delete_never_finalizes_and_a_reinstall_survives_it(pool: PgPool) {
    // An in-memory backend with a deleter whose FIRST delete faults ambiguously.
    let inner: Arc<dyn object_store::ObjectStore> = Arc::new(object_store::memory::InMemory::new());
    let deleter = FaultFirstDelete::new(Arc::clone(&inner), FirstDeleteFault::Error);
    let staging = std::env::temp_dir().join(format!(
        "topos-ps-race-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos()
    ));
    let authority = crate::Authority::from_pool_with_store(
        pool,
        PlaneStore::from_object_stores(Arc::clone(&inner), deleter.clone()),
        &staging,
    )
    .expect("authority");
    let w = ws("w1");

    // Seed: one bundle whose only version holds the content, then delete the bundle — its object
    // becomes unrooted garbage and the inline GC pass fires the FAULTING delete (token A).
    let content: &[u8] = b"the racing bytes";
    authority
        .publish(
            &w,
            &bundle("doomed"),
            candidate("GUIDE.md", content, None),
            None,
            NOW,
        )
        .await
        .expect("publish");
    let report = authority
        .delete_bundle(&w, &bundle("doomed"), NOW + 1)
        .await
        .expect("delete bundle");
    // The delete faulted ambiguously ⇒ NOTHING was reclaimed and absence was NOT finalized.
    assert_eq!(
        report.objects_reclaimed, 0,
        "ambiguity must not count as reclaim"
    );
    assert_eq!(deleter.deletes_started.load(Ordering::SeqCst), 1);
    assert_eq!(
        deleter.deletes_completed.load(Ordering::SeqCst),
        0,
        "the faulted attempt never reached the backend"
    );

    // The bytes are still in the store (nothing deleted), and the row is still `deleting`: a
    // fresh ingest of the SAME content cannot resurrect it — install_one waits on `deleting` —
    // which is exactly why recovery must run first. (Not exercised here: the wait costs seconds.)

    // Step 3: the recovery sweep, once the row is stale (now advanced past the threshold), takes
    // over under its own token, re-runs the idempotent delete — the second delete PASSES — and
    // finalizes `absent`.
    let stale_now = NOW + 1 + crate::gc::RECOVERY_STALE_MS + 1_000;
    let recovered = authority.run_recovery(stale_now).await.expect("recovery");
    assert_eq!(recovered, 1, "recovery finalized the ambiguous leftover");
    assert_eq!(deleter.deletes_completed.load(Ordering::SeqCst), 1);

    // Step 4: reinstall the same content in a new bundle — a fresh `absent → present` install.
    let (v, _) = authority
        .publish(
            &w,
            &bundle("reborn"),
            candidate("GUIDE.md", content, None),
            None,
            stale_now + 10,
        )
        .await
        .expect("reinstall publishes");

    // Step 5: the reinstalled bytes are present, readable, and verified — and the original
    // attempt is provably dead (exactly one delete ever reached the backend: recovery's). The
    // corruption the race threatens — Postgres saying `present` over missing bytes — cannot
    // happen.
    assert_eq!(
        authority
            .read_object(&w, &bundle("reborn"), object_id(content))
            .await
            .expect("reinstalled bytes serve"),
        content
    );
    let meta = authority
        .read_version(&w, &bundle("reborn"), v.version_id)
        .await
        .expect("meta");
    assert_eq!(meta.files.len(), 1);
    assert_eq!(deleter.deletes_started.load(Ordering::SeqCst), 2);
    assert_eq!(deleter.deletes_completed.load(Ordering::SeqCst), 1);

    let _ = std::fs::remove_dir_all(&staging);
}
