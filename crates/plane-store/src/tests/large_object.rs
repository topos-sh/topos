//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn placement_independent_identity_same_bytes_either_store(pool: PgPool) {
    // THE load-bearing property: the SAME bytes yield the SAME version_id AND bundle_digest whether routed
    // to git or to large-local (every id is precomputed over real-byte sha256s, before any store write). We
    // force the placement by varying the configurable threshold across two runs of an identical bundle.
    let big = blob(4096, 0xA1);
    // DISTINCT workspaces: the two fixtures now share ONE injected per-test database, so the same bytes
    // under the same workspace would dedup on the second migrate (honoring the first run's recorded
    // location) and make the placement contrast vacuous. `version_id`/`bundle_digest` are content-addressed
    // (workspace-independent), so identity still holds across the two workspaces while each lands the blob
    // in its own store. (Under SQLite each fixture owned a separate database, so `w_acme` never collided.)
    let w_git = ws("w_git");
    let w_large = ws("w_large");

    // Run 1: a huge threshold keeps the 4 KiB blob in the git store.
    let fx_git = Fixture::with_large_limits(pool.clone(), "id-git", 1 << 30, 1 << 30).await;
    let s_git = ingest_migrate(
        &fx_git.authority,
        &w_git,
        "op1",
        vec![file("model.bin", &big)],
        100,
    )
    .await;

    // Run 2: a tiny threshold routes the SAME blob to the large-object store.
    let fx_large = Fixture::with_large_limits(pool, "id-large", 1, 1 << 30).await;
    let s_large = ingest_migrate(
        &fx_large.authority,
        &w_large,
        "op1",
        vec![file("model.bin", &big)],
        100,
    )
    .await;

    assert_eq!(
        s_git.version_id.0, s_large.version_id.0,
        "version_id must be byte-identical regardless of placement"
    );
    assert_eq!(
        s_git.bundle_digest, s_large.bundle_digest,
        "bundle_digest must be byte-identical regardless of placement"
    );

    // …and the two runs genuinely placed the bytes in different stores (else the assertion is vacuous).
    let obj = object_id(&big);
    assert_eq!(
        fx_git
            .authority
            .db()
            .object_location(&w_git, obj)
            .await
            .unwrap(),
        Some(Location::Git)
    );
    assert_eq!(
        fx_large
            .authority
            .db()
            .object_location(&w_large, obj)
            .await
            .unwrap(),
        Some(Location::LargeLocal)
    );
    // The large run physically holds the bytes in the side store; the git run does not.
    assert!(
        fx_large
            .authority
            .large_store(&w_large)
            .exists(obj.0)
            .unwrap()
    );
    assert!(!fx_git.authority.large_store(&w_git).exists(obj.0).unwrap());
}

#[sqlx::test]
async fn routes_by_size_keeps_small_in_git_and_rejects_oversize_at_ingest(pool: PgPool) {
    // threshold 1 KiB, hard cap 4 KiB.
    let fx = Fixture::with_large_limits(pool, "route", 1024, 4096).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let small: &[u8] = b"a small prose file well under the threshold";
    let big = blob(2048, 0x77); // >= 1 KiB and <= 4 KiB → offloaded
    let staged = ingest_migrate(
        a,
        &w,
        "op1",
        vec![file("SKILL.md", small), file("model.bin", &big)],
        100,
    )
    .await;

    assert_eq!(
        a.db().object_location(&w, object_id(small)).await.unwrap(),
        Some(Location::Git),
        "a sub-threshold blob stays in git"
    );
    assert_eq!(
        a.db().object_location(&w, object_id(&big)).await.unwrap(),
        Some(Location::LargeLocal),
        "a blob at/above the threshold offloads"
    );
    // Commits + trees always stay in git — the version still renders (render reads the git tree + commit).
    let rendered = crate::read::render_version(a, &w, staged.version_id.0, staged.bundle_digest)
        .await
        .expect("the mixed version renders");
    assert_eq!(rendered.files.len(), 2);

    // A blob over the per-blob cap is rejected TYPED at ingest, recording nothing (no row, no quarantine).
    let oversize = blob(5000, 0x99); // > 4 KiB cap
    let err = lifecycle::ingest(
        a,
        &w,
        &op("op2"),
        genesis(vec![file("huge.bin", &oversize)]),
        200,
    )
    .await;
    assert!(
        matches!(err, Err(AuthorityError::RejectedUpload(_))),
        "an oversize blob must be rejected typed at ingest"
    );
    assert_eq!(
        a.db()
            .object_status(&w, object_id(&oversize))
            .await
            .unwrap(),
        ObjectStatus::Absent,
        "a rejected oversize blob records no presence row"
    );
    assert!(
        !a.workspace_quarantine_dir(&w, &op("op2")).exists(),
        "a rejected oversize blob stages nothing to the quarantine"
    );
}

#[sqlx::test]
async fn renders_a_mixed_offloaded_and_git_bundle_byte_exact(pool: PgPool) {
    let fx = Fixture::with_large_limits(pool, "render-mix", 1024, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let prose: &[u8] = b"# SKILL\nrun the thing\n";
    let nested: &[u8] = b"#!/bin/sh\necho nested\n";
    let model = blob(3000, 0x42);
    let staged = ingest_migrate(
        a,
        &w,
        "op1",
        vec![
            file("SKILL.md", prose),
            file("model.bin", &model),
            file("scripts/run.sh", nested),
        ],
        100,
    )
    .await;
    assert_eq!(
        a.db().object_location(&w, object_id(&model)).await.unwrap(),
        Some(Location::LargeLocal)
    );
    assert_eq!(
        a.db().object_location(&w, object_id(prose)).await.unwrap(),
        Some(Location::Git)
    );

    let rendered = crate::read::render_version(a, &w, staged.version_id.0, staged.bundle_digest)
        .await
        .expect("render the offloaded bundle");
    assert_eq!(
        rendered.bundle_digest, staged.bundle_digest,
        "the recomputed digest must match the pin (consent holds across stores)"
    );
    let got: std::collections::HashMap<&str, &[u8]> = rendered
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.bytes.as_slice()))
        .collect();
    assert_eq!(got["SKILL.md"], prose);
    assert_eq!(got["model.bin"], model.as_slice());
    assert_eq!(got["scripts/run.sh"], nested);

    // A wrong pin is refused — the consent gate holds for an offloaded bundle too.
    assert!(matches!(
        crate::read::render_version(a, &w, staged.version_id.0, [0u8; 32]).await,
        Err(AuthorityError::Integrity(_))
    ));
}

#[sqlx::test]
async fn offloaded_object_read_is_skill_scoped_404_never_by_bare_hash(pool: PgPool) {
    let fx = Fixture::with_large_limits(pool, "r1-offload", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (s, other) = (skill("s_pr"), skill("s_other"));
    let reader = prin("dev_read");
    let outsider = prin("dev_out");
    let body = blob(2048, 0x6F);
    let staged = ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    let obj = object_id(&body);
    assert_eq!(
        a.db().object_location(&w, obj).await.unwrap(),
        Some(Location::LargeLocal)
    );
    // Make the offloaded object readable for skill `s`: provenance + reachability + confirmed membership
    // (the read gate; the per-skill roster rows are follow-state only now).
    a.db()
        .seed_commit(&w, &s, staged.version_id, &[obj])
        .await
        .unwrap();
    a.db()
        .seed_workspace_member(&w, &reader, "member", "confirmed")
        .await
        .unwrap();
    a.db().seed_roster(&w, &s, &reader).await.unwrap();
    a.db().seed_roster(&w, &other, &reader).await.unwrap();

    // A member reads the offloaded bytes of `s` (the read dispatched to the large store + re-verified).
    assert_eq!(a.read_object(&reader, &w, &s, obj).await.unwrap(), body);
    // A non-member principal → the single indistinguishable NotFound (404, never 403).
    assert!(matches!(
        a.read_object(&outsider, &w, &s, obj).await,
        Err(AuthorityError::NotFound)
    ));
    // The reader, but via a DIFFERENT skill that does not reach the object → the SAME NotFound (never by
    // bare hash — the large surface is gated by exactly the same skill-scoped join as git).
    assert!(matches!(
        a.read_object(&reader, &w, &other, obj).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn cross_workspace_offload_has_no_dedup_and_stays_isolated(pool: PgPool) {
    let fx = Fixture::with_large_limits(pool, "xws-offload", 1, 1 << 30).await;
    let a = &fx.authority;
    let (wa, wb) = (ws("w_acme"), ws("w_globex"));
    let s = skill("s_pr");
    let p = prin("dev_read");
    let body = blob(2048, 0xC0); // byte-identical content uploaded by both workspaces
    let sa = ingest_migrate(a, &wa, "opa", vec![file("model.bin", &body)], 100).await;
    let _sb = ingest_migrate(a, &wb, "opb", vec![file("model.bin", &body)], 100).await;
    let obj = object_id(&body);

    // Two distinct physical objects under separate per-workspace roots — no cross-workspace dedup.
    assert!(a.large_store(&wa).exists(obj.0).unwrap());
    assert!(a.large_store(&wb).exists(obj.0).unwrap());

    // Readable in A only; an A-member cannot reach it via B (cross-workspace isolation). The read gate is
    // confirmed membership in the CLAIMED workspace (roster is follow-state only): p is a member of A, not B.
    a.db()
        .seed_commit(&wa, &s, sa.version_id, &[obj])
        .await
        .unwrap();
    a.db()
        .seed_workspace_member(&wa, &p, "member", "confirmed")
        .await
        .unwrap();
    a.db().seed_roster(&wa, &s, &p).await.unwrap();
    assert_eq!(a.read_object(&p, &wa, &s, obj).await.unwrap(), body);
    assert!(matches!(
        a.read_object(&p, &wb, &s, obj).await,
        Err(AuthorityError::NotFound)
    ));

    // Strong no-dedup guard: reclaiming B's (unrooted) copy must leave A's byte-identical object intact — if
    // the two workspaces shared one physical file, GC in B would destroy A's bytes too. (A is rooted by the
    // commit_object edge above; B is unrooted once its lease is released, so it is GC-eligible.)
    a.db().release_lease(&wb, &op("opb")).await.unwrap();
    assert_eq!(gc::run_gc(a, &wb, 1_000_000).await.unwrap(), 1);
    assert!(
        !a.large_store(&wb).exists(obj.0).unwrap(),
        "B's copy is reclaimed by B's GC"
    );
    assert!(
        a.large_store(&wa).exists(obj.0).unwrap(),
        "A's byte-identical object survives a GC in B — two distinct physical objects, no cross-ws dedup"
    );
    assert_eq!(a.read_object(&p, &wa, &s, obj).await.unwrap(), body);
}

#[sqlx::test]
async fn gc_reclaims_an_offloaded_object_by_the_same_fence(pool: PgPool) {
    let fx = Fixture::with_large_limits(pool, "gc-offload", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x5A);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    let obj = object_id(&body);
    assert_eq!(
        a.db().object_location(&w, obj).await.unwrap(),
        Some(Location::LargeLocal)
    );
    assert!(a.large_store(&w).exists(obj.0).unwrap());

    // Abandon the migrate (the deferred pointer-move never took the root): release the committed lease.
    // No commit_object roots it, so GC reclaims it — and the unlink step dispatches to the LARGE store.
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Absent
    );
    assert!(
        !a.large_store(&w).exists(obj.0).unwrap(),
        "the offloaded object must be physically unlinked from the large store"
    );
}

#[sqlx::test]
async fn a_live_lease_spares_an_offloaded_object_from_gc(pool: PgPool) {
    let fx = Fixture::with_large_limits(pool, "gc-lease", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x33);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    let obj = object_id(&body);
    // A successful migrate's lease is non-expiring, so even a far-future GC reclaims nothing — the fence's
    // lease protection holds for an offloaded object exactly as for a git one.
    assert_eq!(gc::run_gc(a, &w, 1_000_000_000).await.unwrap(), 0);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Present
    );
    assert!(a.large_store(&w).exists(obj.0).unwrap());
}

#[sqlx::test]
async fn a_reclaimed_large_local_object_reports_no_live_location(pool: PgPool) {
    // A reclaimed large-local object leaves an `absent` row that STILL records `location = large-local`, but
    // `object_location` honors only a `present` row — so a stale location can never mis-route a later read to
    // the deleted side-store object (it reports None; reads dispatch on the live presence row, never a stale one).
    let fx = Fixture::with_large_limits(pool, "stale-loc", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x7E);
    let obj = object_id(&body);

    // Offload it, then abandon + GC it → the row goes `absent` but keeps `location = large-local`.
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Absent
    );
    // The stale `absent` row records `large-local`, but `object_location` reports no LIVE location.
    assert_eq!(a.db().object_location(&w, obj).await.unwrap(), None);
}

#[sqlx::test]
async fn an_authorized_read_of_a_gone_offloaded_object_is_integrity_not_notfound(pool: PgPool) {
    // The skill-scoped-read invariant: a post-authz fetch failure on the LARGE surface is an Integrity fault,
    // never NotFound — so the indistinguishable 404 still only ever comes from the access join, not a miss.
    let fx = Fixture::with_large_limits(pool, "auth-integrity", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let s = skill("s_pr");
    let p = prin("dev_read");
    let body = blob(2048, 0x4D);
    let obj = object_id(&body);
    let staged = ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    a.db()
        .seed_commit(&w, &s, staged.version_id, &[obj])
        .await
        .unwrap();
    // The read gate is confirmed membership now (per-skill roster is follow-state only).
    a.db()
        .seed_workspace_member(&w, &p, "member", "confirmed")
        .await
        .unwrap();
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    // The object is authorized + offloaded; now remove the large bytes out-of-band (a provenance/store
    // divergence). The read is reachable only AFTER authz, so surfacing the fault discloses nothing.
    a.large_store(&w).delete(obj.0).unwrap();
    assert!(
        matches!(
            a.read_object(&p, &w, &s, obj).await,
            Err(AuthorityError::Integrity(_))
        ),
        "a gone offloaded object on an authorized read must be Integrity, never NotFound"
    );
}

#[sqlx::test]
async fn offloaded_dedup_reuse_and_the_re_materialize_belt(pool: PgPool) {
    // The migrate Present branch for a large-local object: a second migrate of the same bytes dedup-reuses
    // the existing row, and — if a crash lost the large bytes — the belt re-materializes them from the
    // candidate's quarantine into the RECORDED store (large-local), never re-routing by size.
    let fx = Fixture::with_large_limits(pool, "dedup-belt", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x2B);
    let obj = object_id(&body);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    assert!(a.large_store(&w).exists(obj.0).unwrap());

    // Simulate a crash that lost the large bytes, then a second migrate of the SAME content — install_one
    // hits Present, reads the recorded large-local location, and re-materializes from op2's quarantine.
    a.large_store(&w).delete(obj.0).unwrap();
    assert!(!a.large_store(&w).exists(obj.0).unwrap());
    ingest_migrate(a, &w, "op2", vec![file("model.bin", &body)], 200).await;
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Present
    );
    assert!(
        a.large_store(&w).exists(obj.0).unwrap(),
        "the dedup-reuse belt must re-materialize a crash-lost large object into the recorded store"
    );
}

#[sqlx::test]
async fn dedup_reuse_honors_the_recorded_location_when_the_threshold_diverges(pool: PgPool) {
    // The load-bearing Present-branch rule: a dedup-reuse re-materializes into the object's RECORDED store,
    // it NEVER re-routes by the new candidate's size. Construct a genuine divergence: migrate under a HUGE
    // threshold (the blob lands in git), then — simulating an operator lowering the threshold — migrate the
    // SAME bytes via a SECOND authority over the same stores with a tiny threshold (whose size-route would
    // now pick large-local). The object must stay in git; the large store must never receive it.
    let fx = Fixture::with_large_limits(pool.clone(), "recorded-loc", 1 << 30, 1 << 30).await; // huge → routes to git
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x1D);
    let obj = object_id(&body);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    assert_eq!(
        a.db().object_location(&w, obj).await.unwrap(),
        Some(Location::Git)
    );
    assert!(!a.large_store(&w).exists(obj.0).unwrap());

    // A second authority over the SAME stores (the same per-test database via a pool clone, plus fx's git +
    // large dirs), now with a tiny threshold (its size-route would say large).
    let a2 = Authority::from_pool(pool, &fx.dir.join("stores"), &fx.dir.join("large"))
        .expect("open a second authority over the same stores")
        .with_large_limits(1, 1 << 30);
    ingest_migrate(&a2, &w, "op2", vec![file("model.bin", &body)], 200).await;

    // The recorded location is still git, and the large store never received the bytes — the belt honored
    // the recorded location instead of re-routing the existing object by the new candidate's size.
    assert_eq!(
        a2.db().object_location(&w, obj).await.unwrap(),
        Some(Location::Git)
    );
    assert!(
        !a2.large_store(&w).exists(obj.0).unwrap(),
        "a dedup-reuse must not re-route an existing git object to the large store by the new size"
    );
}

#[sqlx::test]
async fn recovery_sweep_reclaims_a_crashed_offloaded_deleting_object(pool: PgPool) {
    // "a crashed deleting must still recover" — for an OFFLOADED object: a GC claim that crashed before
    // finalizing leaves a stale `deleting` large-local row; the recovery sweep re-claims it and the unlink
    // dispatches to the LARGE store, then finalizes it absent.
    let fx = Fixture::with_large_limits(pool, "recover-offload", 1, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x6C);
    let obj = object_id(&body);
    ingest_migrate(a, &w, "op1", vec![file("model.bin", &body)], 100).await;
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    // A GC claims it (present → deleting) at t=100 but "crashes" before unlinking/finalizing.
    assert!(matches!(
        a.db().claim_for_delete(&w, obj, 100).await.unwrap(),
        ClaimOutcome::Claimed {
            location: Location::LargeLocal,
            ..
        }
    ));
    assert!(a.large_store(&w).exists(obj.0).unwrap()); // bytes still on disk (crash before unlink)

    // The recovery sweep, far past the stale threshold, finalizes it — unlinking the LARGE object.
    assert_eq!(gc::recovery_sweep(a, 100_000).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Absent
    );
    assert!(
        !a.large_store(&w).exists(obj.0).unwrap(),
        "recovery must physically unlink the offloaded object from the large store"
    );
}

#[sqlx::test]
async fn routing_boundaries_at_threshold_and_cap_are_exact(pool: PgPool) {
    // threshold 1024 (offload at/above), cap 2048 (reject above).
    let fx = Fixture::with_large_limits(pool, "boundary", 1024, 2048).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let at = blob(1024, 0x01); // == threshold → large
    let below = blob(1023, 0x02); // < threshold → git
    ingest_migrate(
        a,
        &w,
        "op1",
        vec![file("at.bin", &at), file("below.bin", &below)],
        100,
    )
    .await;
    assert_eq!(
        a.db().object_location(&w, object_id(&at)).await.unwrap(),
        Some(Location::LargeLocal),
        "a blob exactly at the threshold offloads (>=)"
    );
    assert_eq!(
        a.db().object_location(&w, object_id(&below)).await.unwrap(),
        Some(Location::Git),
        "a blob one byte below the threshold stays in git"
    );

    // Exactly at the cap is accepted; one byte over is rejected at ingest.
    let at_cap = blob(2048, 0x03);
    ingest_migrate(a, &w, "op2", vec![file("atcap.bin", &at_cap)], 200).await;
    assert_eq!(
        a.db()
            .object_location(&w, object_id(&at_cap))
            .await
            .unwrap(),
        Some(Location::LargeLocal)
    );
    let over_cap = blob(2049, 0x04);
    assert!(
        matches!(
            lifecycle::ingest(
                a,
                &w,
                &op("op3"),
                genesis(vec![file("over.bin", &over_cap)]),
                300
            )
            .await,
            Err(AuthorityError::RejectedUpload(_))
        ),
        "a blob one byte over the cap is rejected at ingest"
    );
}

#[sqlx::test]
async fn single_object_read_of_a_git_file_in_a_mixed_bundle_succeeds(pool: PgPool) {
    // Regression: a git-resident object in a version that ALSO contains an offloaded blob must read fine. The
    // git arm reads the loose object directly by its locator, NOT by walking the whole version tree — which
    // would fault on the offloaded sibling's intentionally-absent git object before reaching the requested
    // blob (and return a spurious Integrity for a perfectly valid read).
    let fx = Fixture::with_large_limits(pool, "mixed-read", 1024, 1 << 30).await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let s = skill("s_pr");
    let p = prin("dev_read");
    let small: &[u8] = b"a small git-resident file, well below the threshold";
    let big = blob(2048, 0x5E); // >= threshold → offloaded (its git object is absent)
    let (small_id, big_id) = (object_id(small), object_id(&big));
    let staged = ingest_migrate(
        a,
        &w,
        "op1",
        vec![file("small.txt", small), file("big.bin", &big)],
        100,
    )
    .await;
    assert_eq!(
        a.db().object_location(&w, small_id).await.unwrap(),
        Some(Location::Git)
    );
    assert_eq!(
        a.db().object_location(&w, big_id).await.unwrap(),
        Some(Location::LargeLocal)
    );
    a.db()
        .seed_commit(&w, &s, staged.version_id, &[small_id, big_id])
        .await
        .unwrap();
    // The read gate is confirmed membership now (per-skill roster is follow-state only).
    a.db()
        .seed_workspace_member(&w, &p, "member", "confirmed")
        .await
        .unwrap();
    a.db().seed_roster(&w, &s, &p).await.unwrap();

    // The git-resident file reads correctly despite the offloaded sibling…
    assert_eq!(a.read_object(&p, &w, &s, small_id).await.unwrap(), small);
    // …and the offloaded file reads too (dispatched to the large store).
    assert_eq!(a.read_object(&p, &w, &s, big_id).await.unwrap(), big);
}

#[sqlx::test]
async fn fenced_migrate_rejects_a_dotgit_path(pool: PgPool) {
    // The fenced migrate must reject a `.git` path exactly as the client write path does — the kernel
    // check_path allows `.git` (it only bars `.`/`..`/NUL/absolute), so ingest stages it, but the migrate's
    // tree build (the plumbing editor + restored component validation) refuses it, so no `.git` bundle is
    // ever recorded.
    let fx = Fixture::new(pool, "dotgit").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let staged = lifecycle::ingest(
        a,
        &w,
        &op("op1"),
        genesis(vec![file(".git/config", b"[core]\n")]),
        100,
    )
    .await
    .expect("ingest stages (check_path allows .git)");
    assert!(
        matches!(
            lifecycle::migrate(a, &w, &staged, 100).await,
            Err(AuthorityError::RejectedUpload(_))
        ),
        "the fenced migrate must reject a .git path at the tree build"
    );
}

#[sqlx::test]
async fn gc_reclaims_large_objects_when_no_git_repo_exists(pool: PgPool) {
    // A workspace whose FIRST migrate routed every blob to the large store, then crashed before
    // migrate_finish created the git repo: the large-local rows must still be reclaimable. GC opens the git
    // store lazily (only for a git unlink), so it does not abort on the missing repo.
    let fx = Fixture::with_large_limits(pool, "no-git-gc", 1, 1 << 30).await; // tiny threshold → all blobs offload
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = blob(2048, 0x4F);
    let obj = object_id(&body);

    // ingest + lease + install, but NOT migrate_finish (which is what creates the main git repo) — a crash
    // before finish. The blob is offloaded, so no git object/repo was ever created.
    let staged = lifecycle::ingest(
        a,
        &w,
        &op("op1"),
        genesis(vec![file("big.bin", &body)]),
        100,
    )
    .await
    .expect("ingest");
    lifecycle::migrate_lease(a, &w, &staged, 100).await.unwrap();
    lifecycle::migrate_install(a, &w, &staged, 100)
        .await
        .unwrap();
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Present
    );
    assert!(a.large_store(&w).exists(obj.0).unwrap());
    assert!(
        !fx.dir.join("stores").join("w_acme").exists(),
        "no git repo was created (all blobs offloaded)"
    );

    // Abandon the crashed migrate's lease, then GC — it must reclaim the large-local object WITHOUT a git repo.
    a.db().release_lease(&w, &op("op1")).await.unwrap();
    assert_eq!(gc::run_gc(a, &w, 1_000_000).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, obj).await.unwrap(),
        ObjectStatus::Absent
    );
    assert!(!a.large_store(&w).exists(obj.0).unwrap());
}
