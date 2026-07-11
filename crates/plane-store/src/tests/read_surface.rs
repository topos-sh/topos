//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

#[sqlx::test]
async fn resolve_read_token_resolves_a_scope_and_a_miss_is_notfound(pool: PgPool) {
    let fx = Fixture::new(pool, "rt-token").await;
    let a = &fx.authority;
    let (w, s, p) = (ws("w_acme"), skill("s_pr"), prin("dev_read"));
    a.db()
        .seed_read_token(&w, &s, &p, "tok-secret-123")
        .await
        .unwrap();

    // A known token resolves to its exact (workspace, skill, principal) scope.
    let scope = a.resolve_read_token("tok-secret-123", 0).await.unwrap();
    assert_eq!(scope.ws().as_str(), "w_acme");
    assert_eq!(scope.skill().as_str(), "s_pr");
    assert_eq!(scope.principal().as_str(), "dev_read");

    // An unknown token is the single indistinguishable not-found (a caller cannot probe which tokens exist).
    assert!(matches!(
        a.resolve_read_token("tok-WRONG", 0).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn list_open_proposals_lists_open_then_a_staled_one_vanishes(pool: PgPool) {
    // keep == read == LIST: an OPEN, non-stale proposal is listed (its @hash + base, no bytes/proposer); the
    // instant a publish stales it, it VANISHES from the list on the shared predicate — no event, no reaper.
    let fx = Fixture::new(pool, "prop-list").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    a.db().seed_roster(&w, &s, &reader).await.unwrap();

    // `current` points at a base commit Cb at (1,1) — the proposal's base.
    let cb = CommitId([0xB0; 32]);
    a.db().seed_commit(&w, &s, cb, &[]).await.unwrap();
    a.db().seed_current(&w, &s, cb, 1, 1).await.unwrap();

    // A read token for the rostered reader → its (ws, skill, principal) scope.
    a.db()
        .seed_read_token(&w, &s, &reader, "tok-list")
        .await
        .unwrap();
    let scope = a.resolve_read_token("tok-list", 0).await.unwrap();

    // Rostered, but no proposals yet → an EMPTY list (never a not-found).
    assert!(
        a.list_open_proposals(&scope, "w_acme", "s_x")
            .await
            .unwrap()
            .is_empty()
    );

    // An OPEN, non-stale proposal cp based at (1,1).
    let cp = CommitId([0xC0; 32]);
    a.db()
        .seed_proposal(&w, "prop1", &s, cp, cb, 1, 1, "open", &prin("p_author"))
        .await
        .unwrap();

    // It lists — with its @hash + base generation; nothing else crosses.
    let listed = a
        .list_open_proposals(&scope, "w_acme", "s_x")
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].version_id, cp.0);
    assert_eq!(listed[0].base, gn(1, 1));

    // STALE it: a publish advances `current` past the base — the eventless derived transition.
    a.db().force_current_generation(&w, &s, 1, 2).await.unwrap();

    // It VANISHES from the list in the SAME step (a gate, not a reaper) — keep == read == list.
    assert!(
        a.list_open_proposals(&scope, "w_acme", "s_x")
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn list_open_proposals_non_rostered_token_is_empty_not_notfound(pool: PgPool) {
    // The roster JOIN IS the authorization: a VALID token whose principal is NOT on the skill's roster yields
    // an EMPTY list (silent membership), never a not-found — there is no per-row probe. (A scope/path mismatch
    // is the only 404 this op raises; membership is invisible.)
    let fx = Fixture::new(pool, "prop-list-noroster").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let outsider = prin("p_outsider");
    // A token for the outsider's scope, but NO roster row for them.
    a.db()
        .seed_read_token(&w, &s, &outsider, "tok-out")
        .await
        .unwrap();
    let scope = a.resolve_read_token("tok-out", 0).await.unwrap();

    // Even with an OPEN, non-stale proposal present, the non-rostered principal sees an empty list.
    let cb = CommitId([0xB0; 32]);
    a.db().seed_commit(&w, &s, cb, &[]).await.unwrap();
    a.db().seed_current(&w, &s, cb, 1, 1).await.unwrap();
    let cp = CommitId([0xC0; 32]);
    a.db()
        .seed_proposal(&w, "prop1", &s, cp, cb, 1, 1, "open", &prin("p_author"))
        .await
        .unwrap();
    assert!(
        a.list_open_proposals(&scope, "w_acme", "s_x")
            .await
            .unwrap()
            .is_empty()
    );
}

#[sqlx::test]
async fn list_open_proposals_rejects_a_scope_or_path_mismatch(pool: PgPool) {
    // The FIRST line of the op is the scope/path assert (the cross-skill/workspace leak guard): a token scoped
    // to (w_acme, s_x) used against another skill — or another workspace — is the indistinguishable not-found,
    // BEFORE any roster/proposal fact is read.
    let fx = Fixture::new(pool, "prop-list-scope").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_x"));
    let reader = prin("p_dev");
    a.db().seed_roster(&w, &s, &reader).await.unwrap();
    a.db()
        .seed_read_token(&w, &s, &reader, "tok-scope")
        .await
        .unwrap();
    let scope = a.resolve_read_token("tok-scope", 0).await.unwrap();

    // The in-scope path lists fine (empty here — no proposals).
    assert!(
        a.list_open_proposals(&scope, "w_acme", "s_x")
            .await
            .unwrap()
            .is_empty()
    );
    // A path whose skill differs from the scope's → the indistinguishable not-found (bound to one skill).
    assert!(matches!(
        a.list_open_proposals(&scope, "w_acme", "s_OTHER").await,
        Err(AuthorityError::NotFound)
    ));
    // …and a path whose workspace differs.
    assert!(matches!(
        a.list_open_proposals(&scope, "w_OTHER", "s_x").await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn read_current_present_absent_and_corrupt_blob(pool: PgPool) {
    let fx = Fixture::new(pool, "rt-readcur").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(40);
    register(&fx, &w, &s, "dk", &key, "p_dev").await;
    fx.authority
        .db()
        .seed_read_token(&w, &s, &prin("p_dev"), "tok-cur")
        .await
        .unwrap();
    let scope = fx.authority.resolve_read_token("tok-cur", 0).await.unwrap();

    // Absent: no pointer has moved yet → None.
    assert!(fx.authority.read_current(&scope).await.unwrap().is_none());

    // Present: a genesis publish signs a record at (1,1); read_current extracts the generation + the raw bytes.
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "40000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let cp = fx
        .authority
        .read_current(&scope)
        .await
        .unwrap()
        .expect("present");
    assert_eq!(cp.generation, gn(1, 1));
    assert_eq!(
        cp.record,
        fx.authority
            .db()
            .read_current_record(&w, &s)
            .await
            .unwrap()
            .unwrap(),
        "read_current serves exactly the stored record bytes"
    );

    // Corrupt: an unparseable stored record blob is an Integrity fault, NEVER a not-found (the record exists).
    fx.authority
        .db()
        .force_current_record(&w, &s, b"{ not json")
        .await
        .unwrap();
    assert!(matches!(
        fx.authority.read_current(&scope).await,
        Err(AuthorityError::Integrity(_))
    ));
}

#[sqlx::test]
async fn serve_object_serves_in_scope_and_rejects_a_scope_or_path_mismatch(pool: PgPool) {
    let fx = Fixture::new(pool, "rt-serve").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_pr"));
    let p = prin("dev_read");
    a.db().seed_roster(&w, &s, &p).await.unwrap();
    let body = b"served bytes";
    stage_committed(a, &w, &s, "serve", vec![file("SKILL.md", body)]).await;
    a.db()
        .seed_read_token(&w, &s, &p, "tok-serve")
        .await
        .unwrap();
    let scope = a.resolve_read_token("tok-serve", 0).await.unwrap();
    let oid_hex = digest::to_hex(&object_id(body).0);

    // Happy: the scope's (ws, skill) matches the path's → the bytes.
    assert_eq!(
        a.serve_object(&scope, "w_acme", "s_pr", &oid_hex)
            .await
            .unwrap(),
        body
    );
    // A path whose skill differs from the scope's → the indistinguishable not-found (bound to one skill).
    assert!(matches!(
        a.serve_object(&scope, "w_acme", "s_OTHER", &oid_hex).await,
        Err(AuthorityError::NotFound)
    ));
    // …and a path whose workspace differs.
    assert!(matches!(
        a.serve_object(&scope, "w_OTHER", "s_pr", &oid_hex).await,
        Err(AuthorityError::NotFound)
    ));
    // A malformed (non-hex) object id is the uniform not-found too, never a distinct error from here.
    assert!(matches!(
        a.serve_object(&scope, "w_acme", "s_pr", "not-a-valid-hex-id")
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn read_version_metadata_accepted_proposal_arm_and_unauthorized(pool: PgPool) {
    let fx = Fixture::new(pool, "rt-vmeta").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(41);
    register(&fx, &w, &s, "dk", &key, "p_author").await;

    // A genesis publish: the REAL pointer-move records skill_commit WITH its digest + the commit_object edges
    // (the accepted-trunk root) + the bytes — the readable state the trunk arm authorizes over.
    let body = b"# skill\nrun it\n";
    let pub_receipt = publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", body)]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    // The enriched OK receipt carries the server-rehashed version id + digest.
    assert_eq!(pub_receipt.version_id, Some(g));
    let g_digest = pub_receipt
        .bundle_digest
        .expect("an OK receipt carries the digest");

    // A rostered reader's scope (version-read authz is roster-based — no device needed).
    let reader = prin("p_reader");
    fx.authority
        .db()
        .seed_roster(&w, &s, &reader)
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_read_token(&w, &s, &reader, "tok-vm")
        .await
        .unwrap();
    let scope = fx.authority.resolve_read_token("tok-vm", 0).await.unwrap();
    let g_hex = digest::to_hex(&g.0);

    // rostered + accepted → ok: exact id, the complete (empty, genesis) parent set, the file leaf, the digest.
    let meta = fx
        .authority
        .read_version_metadata(&scope, "w_acme", "s_deploy", &g_hex)
        .await
        .unwrap();
    assert_eq!(meta.version_id, g.0);
    assert!(meta.parents.is_empty());
    assert_eq!(meta.bundle_digest, g_digest);
    assert_eq!(meta.files.len(), 1);
    assert_eq!(meta.files[0].path, "SKILL.md");
    assert_eq!(meta.files[0].object_id, object_id(body).0);

    // non-rostered → NotFound: a token for a principal with NO roster row resolves, but the version read is
    // the indistinguishable not-found (never a 403).
    fx.authority
        .db()
        .seed_read_token(&w, &s, &prin("p_outsider"), "tok-out")
        .await
        .unwrap();
    let outscope = fx.authority.resolve_read_token("tok-out", 0).await.unwrap();
    assert!(matches!(
        fx.authority
            .read_version_metadata(&outscope, "w_acme", "s_deploy", &g_hex)
            .await,
        Err(AuthorityError::NotFound)
    ));

    // An OPEN, non-stale proposal's version IS readable (the proposal arm): do_propose records the candidate's
    // skill_commit + digest, migrates its bytes, and opens the proposal at base (1,1) (current is unmoved).
    let (_r, cp, _d) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000002",
        child(
            g,
            vec![
                file("SKILL.md", body),
                file("NEW.md", b"a new reference doc"),
            ],
        ),
        gn(1, 1),
    )
    .await;
    let cp_hex = digest::to_hex(&cp.0);
    let pmeta = fx
        .authority
        .read_version_metadata(&scope, "w_acme", "s_deploy", &cp_hex)
        .await
        .unwrap();
    assert_eq!(pmeta.parents, vec![g.0]);
    assert_eq!(pmeta.files.len(), 2);

    // Stale it: a direct publish advances `current` past the proposal's base (1,1). The proposal is now stale
    // (open, but base != current), so its version reads NotFound — proving we NEVER authorize on bare
    // skill_commit (the candidate's provenance row, with its digest, still exists).
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "41000000-0000-4000-8000-000000000003",
        child(g, vec![file("SKILL.md", b"a different v2")]),
        gn(1, 1),
    )
    .await;
    assert!(matches!(
        fx.authority
            .read_version_metadata(&scope, "w_acme", "s_deploy", &cp_hex)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

/// The `commit_object` ≥1-edge join in the version-reachability test is load-bearing: a REJECTED
/// proposal's candidate keeps its `skill_commit` provenance row (with its digest) forever, but has no
/// trunk edge and no open proposal — bare-`skill_commit` authorization would leak its metadata. Pinned
/// here explicitly (the staled-proposal case above pins the `base != current` shape; this pins the
/// `status != 'open'` shape).
#[sqlx::test]
async fn read_version_metadata_rejected_candidate_is_the_uniform_notfound(pool: PgPool) {
    let fx = Fixture::new(pool, "rt-vmeta-rej").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(43);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "43000000-0000-4000-8000-000000000001",
        genesis(vec![file("SKILL.md", b"v0")]),
        gn(0, 0),
    )
    .await;
    let g = current_commit(&fx, &w, &s).await;
    let (_r, cp, digest) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "43000000-0000-4000-8000-000000000002",
        child(
            g,
            vec![file("SKILL.md", b"v0"), file("NEW.md", b"proposed")],
        ),
        gn(1, 1),
    )
    .await;

    let reader = prin("p_reader");
    fx.authority
        .db()
        .seed_roster(&w, &s, &reader)
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_read_token(&w, &s, &reader, "tok-rej")
        .await
        .unwrap();
    let scope = fx.authority.resolve_read_token("tok-rej", 0).await.unwrap();
    let cp_hex = digest::to_hex(&cp.0);

    // Open + non-stale → readable through the proposal arm.
    assert!(
        fx.authority
            .read_version_metadata(&scope, "w_acme", "s_deploy", &cp_hex)
            .await
            .is_ok()
    );

    // Reject it (current untouched — the base still matches, so ONLY the status arm distinguishes).
    do_reject(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "43000000-0000-4000-8000-000000000003",
        cp,
        digest,
        gn(1, 1),
    )
    .await;
    assert!(matches!(
        fx.authority
            .read_version_metadata(&scope, "w_acme", "s_deploy", &cp_hex)
            .await,
        Err(AuthorityError::NotFound)
    ));
}
