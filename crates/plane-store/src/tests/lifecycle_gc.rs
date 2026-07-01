//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn migrate_installs_durably_and_committed_lease_protects_from_gc(pool: PgPool) {
    let fx = Fixture::new(pool, "e-migrate").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"# skill\nrun it\n";
    let staged = ingest_migrate(a, &w, "op1", vec![file("SKILL.md", body)], 100).await;

    // The object is present, and its bytes verify from their FINAL path (a real render of the version).
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    let store = a.open_store(&w).unwrap();
    let rendered = store
        .render_verified(staged.version_id.0, staged.bundle_digest)
        .expect("render the migrated version");
    assert_eq!(rendered.files.len(), 1);
    assert_eq!(rendered.files[0].bytes, body);

    // A successful migrate makes its lease non-expiring, so even a far-future GC reclaims nothing.
    assert_eq!(gc::run_gc(a, &w, 1_000_000_000).await.unwrap(), 0);
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    // The quarantine was cleaned up post-commit.
    assert!(!a.workspace_quarantine_dir(&w, &op("op1")).exists());
}

#[sqlx::test]
async fn gc_reclaims_an_abandoned_migrated_object_physically(pool: PgPool) {
    let fx = Fixture::new(pool, "e-abandon").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"abandon me";
    let staged = ingest_migrate(a, &w, "op1", vec![file("SKILL.md", body)], 100).await;
    // Abandon the migrate (the deferred pointer-move never took the root): release the committed lease.
    a.db().release_lease(&w, &op("op1")).await.unwrap();

    // No commit_object (migrate records none) + no live lease → GC reclaims it.
    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Absent
    );
    // Physical deletion: a fresh store can no longer render the orphaned version (its blob is gone).
    let fresh = a.open_store(&w).unwrap();
    assert!(
        fresh
            .render_verified(staged.version_id.0, staged.bundle_digest)
            .is_err(),
        "the blob must be physically unlinked"
    );
}

#[sqlx::test]
async fn gc_retention_is_exactly_reachability(pool: PgPool) {
    let fx = Fixture::new(pool, "e-retain").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    // Two migrated objects, both abandoned (leases released) so only commit_object can root them.
    let keep = b"keep-me";
    let drop = b"drop-me";
    ingest_migrate(a, &w, "k", vec![file("keep.md", keep)], 100).await;
    ingest_migrate(a, &w, "d", vec![file("drop.md", drop)], 100).await;
    a.db().release_lease(&w, &op("k")).await.unwrap();
    a.db().release_lease(&w, &op("d")).await.unwrap();
    // Root `keep` via a commit_object edge (the read-authorization surface); leave `drop` unrooted.
    a.db()
        .seed_commit(&w, &s, CommitId([0x55; 32]), &[object_id(keep)])
        .await
        .unwrap();

    assert_eq!(gc::run_gc(a, &w, 200).await.unwrap(), 1); // exactly `drop`
    assert_eq!(
        a.db().object_status(&w, object_id(keep)).await.unwrap(),
        ObjectStatus::Present
    );
    assert_eq!(
        a.db().object_status(&w, object_id(drop)).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[sqlx::test]
async fn dedup_race_lease_protects_the_full_closure_under_a_slow_migrate(pool: PgPool) {
    // The release-blocker dedup race, exercised through the REAL migrate op (lease step), deterministically.
    let fx = Fixture::new(pool, "e-dedup").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let a_bytes = b"shared object A";
    let c_bytes = b"new object C";

    // V1 migrates A, then is abandoned → A is present + on disk but UNROOTED (no commit_object, no lease).
    ingest_migrate(a, &w, "v1", vec![file("a.txt", a_bytes)], 100).await;
    a.db().release_lease(&w, &op("v1")).await.unwrap();

    // V2 reuses A and adds C. Stage + lease the FULL set {A, C}, then — mid-migrate, before C installs —
    // run a GC. The lease must spare A (the dedup-skipped reused object); C is not present yet.
    let v2 = lifecycle::ingest(
        a,
        &w,
        &op("v2"),
        genesis(vec![file("a.txt", a_bytes), file("c.txt", c_bytes)]),
        200,
    )
    .await
    .unwrap();
    lifecycle::migrate_lease(a, &w, &v2, 200).await.unwrap();
    // Fault-injected slow migrate: GC interposes here.
    assert_eq!(
        gc::run_gc(a, &w, 200).await.unwrap(),
        0,
        "GC must reclaim nothing: A is protected by V2's full-closure lease, C is not yet present"
    );
    assert_eq!(
        a.db().object_status(&w, object_id(a_bytes)).await.unwrap(),
        ObjectStatus::Present
    );
    // Finish the migrate: A is reused (dedup), C installs.
    lifecycle::migrate_install(a, &w, &v2, 200).await.unwrap();
    lifecycle::migrate_finish(a, &w, &v2, 200).await.unwrap();
    let store = a.open_store(&w).unwrap();
    let rendered = store
        .render_verified(v2.version_id.0, v2.bundle_digest)
        .expect("render V2");
    let mut got: Vec<&[u8]> = rendered.files.iter().map(|f| f.bytes.as_slice()).collect();
    got.sort();
    let mut want = vec![a_bytes.as_slice(), c_bytes.as_slice()];
    want.sort();
    assert_eq!(
        got, want,
        "both A (reused) and C (installed) render byte-exact"
    );
}

#[sqlx::test]
async fn two_concurrent_migrations_of_one_object_do_not_corrupt(pool: PgPool) {
    let fx = Fixture::new(pool, "e-concurrent").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"identical content";
    // Two independent ops staging the SAME bytes, migrated concurrently. The absent→present CAS makes one
    // install and the other reuse; neither corrupts (write_blob is content-addressed/idempotent).
    let sa = lifecycle::ingest(a, &w, &op("a"), genesis(vec![file("x", body)]), 100)
        .await
        .unwrap();
    let sb = lifecycle::ingest(a, &w, &op("b"), genesis(vec![file("x", body)]), 100)
        .await
        .unwrap();
    let (ra, rb) = tokio::join!(
        lifecycle::migrate(a, &w, &sa, 100),
        lifecycle::migrate(a, &w, &sb, 100),
    );
    ra.unwrap();
    rb.unwrap();
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    // Both versions render the identical, verified bytes.
    let store = a.open_store(&w).unwrap();
    for s in [&sa, &sb] {
        let r = store
            .render_verified(s.version_id.0, s.bundle_digest)
            .unwrap();
        assert_eq!(r.files[0].bytes, body);
    }
}

#[sqlx::test]
async fn gc_never_touches_an_active_quarantine_but_the_janitor_sweeps_an_expired_one(pool: PgPool) {
    let fx = Fixture::new(pool, "e-quarantine").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    // Ingest WITHOUT migrating → bytes sit in the GC-excluded quarantine, not the main store.
    let staged = lifecycle::ingest(a, &w, &op("q"), genesis(vec![file("Q.md", b"queued")]), 100)
        .await
        .unwrap();
    let qdir = a.workspace_quarantine_dir(&w, &op("q"));
    assert!(qdir.exists(), "the quarantine objdir exists after ingest");

    // A GC pass reclaims nothing staged in the quarantine (it is not in the main store / has no present row).
    gc::run_gc(a, &w, 200).await.unwrap();
    assert!(qdir.exists(), "GC must never touch an active quarantine");
    assert_eq!(
        a.db()
            .object_status(&w, ObjectId(staged.entries[0].object_id))
            .await
            .unwrap(),
        ObjectStatus::Absent
    );

    // The janitor spares an unexpired quarantine and sweeps an expired one (TTL = ingest_now + 3600).
    assert_eq!(gc::quarantine_janitor(a, 200).await.unwrap(), 0);
    assert!(qdir.exists());
    assert_eq!(gc::quarantine_janitor(a, 100 + 3600 + 1).await.unwrap(), 1);
    assert!(
        !qdir.exists(),
        "the janitor sweeps an expired quarantine whole"
    );
}

#[sqlx::test]
async fn recovery_sweep_finalizes_a_crashed_unlink_end_to_end(pool: PgPool) {
    let fx = Fixture::new(pool, "e-recovery").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"half-unlinked";
    let staged = ingest_migrate(a, &w, "op", vec![file("X.md", body)], 100).await;
    let oid = object_id(body);
    // Abandon the migrate so the object is genuinely unrooted — only such an object can be GC-claimed, so
    // this is the realistic precondition for a crashed GC (the recovery claim now re-verifies the keep-set,
    // so a still-rooted `deleting` row is spared, not finalized — see
    // `recovery_sweep_spares_a_deleting_object_re_rooted_by_a_commit_edge`).
    a.db().release_lease(&w, &op("op")).await.unwrap();
    // Simulate a GC that claimed the object (present → deleting) long ago, then crashed before finalizing.
    a.db()
        .seed_deleting_object(&w, oid, &staged.entries[0].git_oid, 10)
        .await
        .unwrap();
    assert_eq!(
        a.db().object_status(&w, oid).await.unwrap(),
        ObjectStatus::Deleting
    );
    // The recovery sweep (far past the stale threshold) finalizes it: re-unlink + absent.
    assert_eq!(gc::recovery_sweep(a, 1_000_000).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, oid).await.unwrap(),
        ObjectStatus::Absent
    );
    let fresh = a.open_store(&w).unwrap();
    assert!(
        fresh
            .render_verified(staged.version_id.0, staged.bundle_digest)
            .is_err(),
        "the crashed unlink is completed (bytes gone)"
    );
    // Idempotent: a second sweep finds nothing stale.
    assert_eq!(gc::recovery_sweep(a, 1_000_001).await.unwrap(), 0);
}

#[sqlx::test]
async fn ingest_rejects_a_denylisted_blob(pool: PgPool) {
    let fx = Fixture::new(pool, "e-deny").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"a leaked secret";
    a.db()
        .insert_tombstone(&w, object_id(body), "leaked", 100)
        .await
        .unwrap();
    let res = lifecycle::ingest(a, &w, &op("d"), genesis(vec![file("S.md", body)]), 110).await;
    assert!(matches!(res, Err(AuthorityError::RejectedUpload(_))));
    // The denylist check runs BEFORE staging, so the purged bytes are never persisted to disk.
    assert!(
        !a.workspace_quarantine_dir(&w, &op("d")).exists(),
        "a denylisted candidate must not be staged into a quarantine"
    );
}

#[sqlx::test]
async fn gc_for_one_workspace_never_touches_another_with_identical_content(pool: PgPool) {
    let fx = Fixture::new(pool, "e-xws-gc").await;
    let a = &fx.authority;
    let (wa, wb) = (ws("w_a"), ws("w_b"));
    let body = b"identical bytes across tenants";
    // The SAME content migrated into BOTH workspaces — two DISTINCT physical objects + presence rows.
    ingest_migrate(a, &wa, "op", vec![file("X.md", body)], 100).await;
    let sb = ingest_migrate(a, &wb, "op", vec![file("X.md", body)], 100).await;
    // Abandon A's (so it is a GC candidate); keep B's rooted (committed lease).
    a.db().release_lease(&wa, &op("op")).await.unwrap();

    // GC for A reclaims A's object; B's identical-content object is untouched (workspace_id-bound).
    assert_eq!(gc::run_gc(a, &wa, 200).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&wa, object_id(body)).await.unwrap(),
        ObjectStatus::Absent
    );
    assert_eq!(
        a.db().object_status(&wb, object_id(body)).await.unwrap(),
        ObjectStatus::Present
    );
    // B's bytes are intact on disk (its version still renders).
    let store_b = a.open_store(&wb).unwrap();
    assert_eq!(
        store_b
            .render_verified(sb.version_id.0, sb.bundle_digest)
            .unwrap()
            .files[0]
            .bytes,
        body
    );
}

#[sqlx::test]
async fn quarantines_are_per_workspace_and_the_janitor_is_scoped(pool: PgPool) {
    let fx = Fixture::new(pool, "e-xws-q").await;
    let a = &fx.authority;
    let (wa, wb) = (ws("w_a"), ws("w_b"));
    // Same op id in two workspaces → distinct quarantine dirs.
    lifecycle::ingest(a, &wa, &op("k"), genesis(vec![file("A.md", b"a")]), 100)
        .await
        .unwrap();
    lifecycle::ingest(a, &wb, &op("k"), genesis(vec![file("B.md", b"b")]), 100)
        .await
        .unwrap();
    let qa = a.workspace_quarantine_dir(&wa, &op("k"));
    let qb = a.workspace_quarantine_dir(&wb, &op("k"));
    assert_ne!(qa, qb);
    assert!(qa.exists() && qb.exists());
    // Expire only A's window: the janitor sweeps A's quarantine and spares B's.
    a.db()
        .insert_quarantine(&wa, &op("k"), &qa.to_string_lossy(), 150)
        .await
        .unwrap();
    assert_eq!(gc::quarantine_janitor(a, 200).await.unwrap(), 1);
    assert!(!qa.exists(), "A's expired quarantine swept");
    assert!(qb.exists(), "B's quarantine spared (its TTL is far off)");
}

#[sqlx::test]
async fn recovery_sweep_spares_a_deleting_object_re_rooted_by_a_commit_edge(pool: PgPool) {
    // The recovery byte-loss guard for the RECOVERY path, where the keep-set root arrives AFTER the claim. A
    // crashed GC leaves a stale `deleting` row; before recovery runs, a `commit_object` edge over the same
    // object appears (making it read-authorized). recovery_sweep must re-verify the keep-set at delete time
    // and SPARE it, never unlink a now-readable, committed object's bytes. Fails (Integrity on the final read)
    // if `claim_stale_for_recovery` drops its `commit_object` re-check.
    let fx = Fixture::new(pool, "e-recover-reroot").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_p");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    let body = b"shared content the recovery must not reclaim";

    // (1) Fenced migrate of `body`, then abandon -> present, unrooted (a normal GC candidate). The migrated
    // version's git commit + blob exist on disk.
    let staged = ingest_migrate(a, &w, "op", vec![file("B.md", body)], 100).await;
    a.db().release_lease(&w, &op("op")).await.unwrap();
    let oid = object_id(body);

    // (2) A GC claims it (present -> deleting) then "crashes" before unlink/finalize: the row is `deleting`
    // with an old status_updated_at and the bytes are still on disk.
    assert!(matches!(
        a.db().claim_for_delete(&w, oid, 200).await.unwrap(),
        ClaimOutcome::Claimed { .. }
    ));

    // (3) A `commit_object` edge over the migrated commit now roots the object — it is read-authorized even
    // though its row is `deleting`.
    a.db()
        .seed_commit(&w, &s, staged.version_id, &[oid])
        .await
        .unwrap();
    assert_eq!(a.read_object(&p, &w, &s, oid).await.unwrap(), body);

    // (4) Recovery runs far past the stale threshold. It must SPARE the re-rooted object (recovered == 0),
    // leaving it `deleting` (non-resurrectable) with its bytes intact — the read keeps working.
    assert_eq!(
        gc::recovery_sweep(a, 1_000_000).await.unwrap(),
        0,
        "recovery must not reclaim a now-readable, commit-referenced object"
    );
    assert_eq!(
        a.db().object_status(&w, oid).await.unwrap(),
        ObjectStatus::Deleting
    );
    assert_eq!(
        a.read_object(&p, &w, &s, oid).await.unwrap(),
        body,
        "the committed object's bytes survive recovery"
    );
}

#[sqlx::test]
async fn recovery_finalizes_a_leased_deleting_row_to_unblock_a_waiting_migrate(pool: PgPool) {
    // A migrate that hits a crashed-GC's stale `deleting` row leases its full object set (including this
    // object) BEFORE `install_one` waits for `absent`. Recovery MUST still finalize the stale row to unblock
    // that waiter — a lease over a `deleting` object means "waiting to re-install", not "readable" (only a
    // `commit_object` edge spares; see `recovery_sweep_spares_a_deleting_object_re_rooted_by_a_commit_edge`).
    // Regression: a recovery guard that also checked the lease would strand the migrate until the lease TTL
    // lapsed.
    let fx = Fixture::new(pool, "e-recover-leased").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"crashed then re-leased by a waiting migrate";
    // A real object + store (so recovery can open the store and unlink the loose object); then abandon it.
    let staged = ingest_migrate(a, &w, "mig", vec![file("X.md", body)], 100).await;
    let o = object_id(body);
    a.db().release_lease(&w, &op("mig")).await.unwrap();
    // A crashed GC left it stale `deleting`.
    a.db()
        .seed_deleting_object(&w, o, &staged.entries[0].git_oid, 10)
        .await
        .unwrap();
    // A NEW migrate (a retry of the same content) leases the object set and is now waiting in install_one for
    // `absent` — the lease was taken BEFORE the wait.
    a.db()
        .insert_lease(&w, &op("retry"), staged.version_id, &[o], 9_999)
        .await
        .unwrap();
    // Recovery still finalizes it (recovered == 1) so the waiter can re-copy — the lease must not spare it.
    assert_eq!(gc::recovery_sweep(a, 1_000_000).await.unwrap(), 1);
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[sqlx::test]
async fn migrate_install_waits_out_deleting_then_recopies(pool: PgPool) {
    // Exercises install_one's deleting-wait branch (the sole justification for the normal `tokio` time dep):
    // an object mid-GC (`deleting`) is NEVER resurrected by a concurrent migrate — the install waits for
    // `absent`, then re-copies the bytes. A regression that treated `deleting` as a dedup reuse (e.g.
    // `return Ok(())`) would leave the row `deleting` and fail the final `Present` assertion.
    let fx = Fixture::new(pool, "e-deleting-wait").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"installed only after the unlink finishes";
    let staged = lifecycle::ingest(a, &w, &op("d"), genesis(vec![file("X.md", body)]), 100)
        .await
        .unwrap();
    let oid = ObjectId(staged.entries[0].object_id);
    // Seed the object as `deleting` (a GC mid-unlink) so the install must wait it out, not reuse it.
    a.db()
        .seed_deleting_object(&w, oid, &staged.entries[0].git_oid, 10)
        .await
        .unwrap();

    // Run the install concurrently with a task that finalizes the unlink (deleting -> absent). `join!` is
    // polled in order, so install_one observes `deleting` first and enters its (transaction-free) wait; the
    // flip then commits, and the install's next poll sees `absent` and re-copies.
    let install = lifecycle::migrate_install(a, &w, &staged, 200);
    let finish_unlink = async {
        // The seeded `deleting` row's token is its status_updated_at (10).
        a.db().finalize_delete(&w, oid, 10, 200).await.unwrap();
    };
    let (install_res, ()) = tokio::join!(install, finish_unlink);
    install_res.expect("install succeeds after the deleting row clears");

    assert_eq!(
        a.db().object_status(&w, oid).await.unwrap(),
        ObjectStatus::Present,
        "the migrate re-copied the bytes only after `absent`, never resurrecting `deleting`"
    );
}

#[sqlx::test]
async fn tombstone_does_not_interrupt_an_in_flight_deletion(pool: PgPool) {
    // `insert_tombstone`'s `WHERE status IN ('present','absent')` deliberately leaves a `deleting` row alone
    // (flipping it to `unavailable` would strand the unlink — `finalize_delete` only fires on `deleting`).
    // The blob is still denylisted, and the in-flight unlink still completes to `absent`.
    let fx = Fixture::new(pool, "t-tomb-deleting").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let o = object_id(b"dying-but-denylisted");
    a.db()
        .seed_deleting_object(&w, o, &goid(7), 10)
        .await
        .unwrap();
    a.db().insert_tombstone(&w, o, "leaked", 100).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Deleting,
        "a tombstone must not interrupt an in-flight unlink"
    );
    assert!(a.db().is_tombstoned(&w, o).await.unwrap());
    // The unlink still finalizes normally (the seeded `deleting` row's token is its status_updated_at, 10).
    a.db().finalize_delete(&w, o, 10, 200).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[sqlx::test]
async fn claim_expired_quarantine_spares_a_refreshed_reused_op(pool: PgPool) {
    // The janitor's claim-before-rm guard: a quarantine row whose expiry was refreshed into the future (op-id
    // reuse by a retry) must NOT be claimed for sweeping at a `now` past the OLD expiry — only a still-expired
    // row is. This is what stops the janitor from rm'ing an active, re-staged quarantine out from under an
    // in-flight migrate.
    let fx = Fixture::new(pool, "t-q-claim").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let dir = a.workspace_quarantine_dir(&w, &op("k"));
    a.db()
        .insert_quarantine(&w, &op("k"), &dir.to_string_lossy(), 150)
        .await
        .unwrap();
    // A retry reuses the op id and refreshes the expiry far into the future (the active in-flight upload).
    a.db()
        .insert_quarantine(&w, &op("k"), &dir.to_string_lossy(), 10_000)
        .await
        .unwrap();
    // At now = 200 (past the OLD 150 expiry) the refreshed row is spared, and stays tracked.
    assert!(
        !a.db()
            .claim_expired_quarantine(&w, &op("k"), 200)
            .await
            .unwrap(),
        "a refreshed (reused) quarantine must not be claimed"
    );
    assert_eq!(
        a.db().expired_quarantine_ops(&w, 10_000).await.unwrap(),
        vec![op("k")],
        "the active quarantine row survives"
    );
    // Once it truly expires, the claim wins and removes the row.
    assert!(
        a.db()
            .claim_expired_quarantine(&w, &op("k"), 10_001)
            .await
            .unwrap()
    );
    assert!(
        a.db()
            .expired_quarantine_ops(&w, 10_001)
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn recovery_reclaim_fences_off_the_superseded_gc_claimant(pool: PgPool) {
    // A pre-merge review finding: a recovery sweep that re-claims a `deleting` row a live GC claimed (because a
    // long/frozen pass let it look stale) must FENCE OFF that original claimant — only one actor may unlink +
    // finalize, or a re-migrate's freshly re-installed bytes could be deleted out from under it (a
    // phantom-`present` byte loss). The fence is the claim token (`status_updated_at`): both
    // `confirm_deleting_owner` (gating the unlink) and the token-gated `finalize_delete` reject a superseded
    // claimant. Driven at the SQL layer with explicit timestamps (no real clock) so the interleaving is
    // deterministic.
    let fx = Fixture::new(pool, "t-claim-fence").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let o = object_id(b"contended");
    a.db()
        .install_object(&w, o, Location::Git, &goid(9), 1, 100)
        .await
        .unwrap();

    // (1) The live GC claims it with token T=100 (a back-dated, pass-fixed `now`).
    match a.db().claim_for_delete(&w, o, 100).await.unwrap() {
        ClaimOutcome::Claimed { git_oid, .. } => assert_eq!(git_oid, goid(9)),
        ClaimOutcome::Spared => panic!("expected claimed"),
    }
    // (2) A recovery sweep finds it stale (sua=100 < older_than=200) and re-claims it with token T=500.
    assert_eq!(
        a.db()
            .claim_stale_for_recovery(&w, o, 200, 500)
            .await
            .unwrap(),
        Some((Location::Git, goid(9))),
        "recovery re-claims the stale deleting row"
    );

    // (3) The superseded original GC has LOST ownership: its owner-check fails (so it skips its unlink)…
    assert!(
        !a.db().confirm_deleting_owner(&w, o, 100).await.unwrap(),
        "the superseded GC must not still own the row"
    );
    // …and its token-gated finalize is a no-op (it must NOT flip the row recovery now owns).
    a.db().finalize_delete(&w, o, 100, 600).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Deleting,
        "a superseded finalize must not advance the row"
    );

    // (4) The recovery owner (token=500) confirms ownership and finalizes the row exactly once.
    assert!(a.db().confirm_deleting_owner(&w, o, 500).await.unwrap());
    a.db().finalize_delete(&w, o, 500, 600).await.unwrap();
    assert_eq!(
        a.db().object_status(&w, o).await.unwrap(),
        ObjectStatus::Absent
    );
}

#[sqlx::test]
async fn migrate_re_materializes_a_present_row_whose_bytes_a_crash_removed(pool: PgPool) {
    // A pre-merge review finding: a `present` row whose loose object a past crash silently removed (the
    // WAL power-loss residual) must NOT be blindly dedup-reused — `migrate_finish`'s non-expiring lease would
    // then root a version over gone bytes (a permanent, dedup-poisoning byte loss). install_one's belt stats
    // the loose object and re-materializes it from the candidate's quarantine instead of dedup-skipping.
    let fx = Fixture::new(pool, "e-belt").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let body = b"resurrect me from quarantine";
    let v1 = ingest_migrate(a, &w, "op1", vec![file("SKILL.md", body)], 100).await;
    let git_oid = v1.entries[0].git_oid;

    // Simulate the crash residual: physically remove the loose object but LEAVE the row `present`.
    a.open_store(&w)
        .unwrap()
        .delete_loose_object(git_oid)
        .unwrap();
    assert!(
        !a.open_store(&w).unwrap().object_exists(git_oid).unwrap(),
        "precondition: the bytes are physically gone"
    );
    assert_eq!(
        a.db().object_status(&w, object_id(body)).await.unwrap(),
        ObjectStatus::Present,
        "but the DB row still says present (the residual a migrate must not trust blindly)"
    );

    // A re-migrate of the same content hits install_one's Present branch — the belt must re-materialize the
    // bytes from this candidate's quarantine rather than dedup-skip over the gone object.
    let v2 = ingest_migrate(a, &w, "op2", vec![file("SKILL.md", body)], 200).await;

    assert!(
        a.open_store(&w).unwrap().object_exists(git_oid).unwrap(),
        "the belt re-materialized the loose object"
    );
    // The version now renders byte-exact (the bytes are back at their final path).
    let store = a.open_store(&w).unwrap();
    let rendered = store
        .render_verified(v2.version_id.0, v2.bundle_digest)
        .expect("render after the belt heals the missing bytes");
    assert_eq!(rendered.files.len(), 1);
    assert_eq!(rendered.files[0].bytes, body);
}
