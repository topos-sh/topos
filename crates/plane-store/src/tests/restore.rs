//! The operator backup/restore epoch bump (`Authority::restore_bump_epochs`) — re-sign `current` one epoch
//! forward (same commit, same seq) so a restored plane's next record beats every tuple followers recorded.
use super::*;

const OP_G: &str = "0e000000-0000-4000-8000-000000000001";
const OP_C: &str = "0e000000-0000-4000-8000-000000000002";

/// Publish a genesis + one child through the real pointer-move, landing `current` at `(1, 2)`.
async fn genesis_and_child(fx: &Fixture, w: &WorkspaceId, s: &SkillId) -> CommitId {
    let key = dev_key(0xE1);
    register(fx, w, s, "dk_r", &key, "p_op").await;
    let g = publish(
        fx,
        &key,
        "dk_r",
        w,
        s,
        OP_G,
        genesis(vec![file("a", b"1")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(g.current, Some(gn(1, 1)));
    let c0 = g.version_id.unwrap();
    let r = publish(
        fx,
        &key,
        "dk_r",
        w,
        s,
        OP_C,
        child(c0, vec![file("a", b"2")]),
        gn(1, 1),
    )
    .await;
    assert_eq!(r.current, Some(gn(1, 2)));
    r.version_id.unwrap()
}

#[sqlx::test]
async fn bump_re_signs_current_same_commit_and_verifies(pool: PgPool) {
    let fx = Fixture::new(pool, "restore-bump").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let c2 = genesis_and_child(&fx, &w, &s).await;

    let reports = fx
        .authority
        .restore_bump_epochs(None, None, NOW + 1)
        .await
        .unwrap();
    assert_eq!(reports.len(), 1);
    let r = &reports[0];
    assert_eq!(r.workspace_id, w);
    assert_eq!(r.skill_id, s);
    assert_eq!(r.commit, c2, "the bump never changes the named commit");
    assert_eq!(r.old, gn(1, 2));
    assert_eq!(r.new, gn(2, 2), "epoch bumps by one; seq is preserved");
    assert_eq!(r.key_id, fx.authority.plane_key_id().unwrap());

    // The stored pointer moved to (2,2), same commit, and the fresh signature verifies under the plane key.
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &s)
            .await
            .unwrap(),
        Some(gn(2, 2))
    );
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c2)
    );
    let record = fx
        .authority
        .read_signed_record(&w, &s)
        .await
        .unwrap()
        .expect("signed");
    let pubkey = fx.authority.plane_public_key().unwrap();
    assert!(verify_record(&record, "w_acme", "s_deploy", &pubkey));
    let parsed: SignedCurrentRecord = serde_json::from_slice(&record).unwrap();
    assert_eq!(parsed.record.generation, gn(2, 2));
    assert_eq!(parsed.record.version_id, topos_core::digest::to_hex(&c2.0));

    // Running twice bumps twice — one more ordinary forward move, unguarded on purpose.
    let again = fx
        .authority
        .restore_bump_epochs(None, None, NOW + 2)
        .await
        .unwrap();
    assert_eq!(again[0].old, gn(2, 2));
    assert_eq!(again[0].new, gn(3, 2));
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &s)
            .await
            .unwrap(),
        Some(gn(3, 2))
    );
}

/// ENVELOPE PARITY — the pin on the rebuilt serializer. The promote path's envelope serializer is private
/// to the off-limits `db/set_current.rs`, so the bump reconstructs the typed DTO; this test JSON-parses a
/// publish-produced record and a bump-produced one and asserts an identical field set/shape (recursive key
/// structure, the signature `alg`/`key_id`, and the 86-char base64url signature length).
#[sqlx::test]
async fn bump_envelope_matches_the_publish_envelope_shape(pool: PgPool) {
    let fx = Fixture::new(pool, "restore-parity").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    genesis_and_child(&fx, &w, &s).await;

    let published = fx
        .authority
        .read_signed_record(&w, &s)
        .await
        .unwrap()
        .expect("publish-signed record");
    fx.authority
        .restore_bump_epochs(None, None, NOW + 1)
        .await
        .unwrap();
    let bumped = fx
        .authority
        .read_signed_record(&w, &s)
        .await
        .unwrap()
        .expect("bump-signed record");

    let a: serde_json::Value = serde_json::from_slice(&published).unwrap();
    let b: serde_json::Value = serde_json::from_slice(&bumped).unwrap();
    assert_eq!(
        shape(&a),
        shape(&b),
        "the two envelopes must carry the identical field set/shape"
    );
    assert_eq!(a["schema_version"], b["schema_version"]);
    assert_eq!(a["signature"]["alg"], b["signature"]["alg"]);
    assert_eq!(a["signature"]["key_id"], b["signature"]["key_id"]);
    let sig_a = a["signature"]["value"].as_str().unwrap();
    let sig_b = b["signature"]["value"].as_str().unwrap();
    assert_eq!(sig_a.len(), 86, "base64url-unpadded 64-byte signature");
    assert_eq!(sig_b.len(), 86);
    // Both parse as the one wire DTO (the closed alg enum would fail-close any drift).
    let _: SignedCurrentRecord = serde_json::from_slice(&published).unwrap();
    let _: SignedCurrentRecord = serde_json::from_slice(&bumped).unwrap();
}

/// The structural skeleton of a JSON value: objects → their (sorted) keys mapped to child skeletons, arrays
/// → element skeletons, scalars → the type name. Two values with equal skeletons carry the same field set.
fn shape(v: &serde_json::Value) -> serde_json::Value {
    match v {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, child)| (k.clone(), shape(child)))
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(shape).collect())
        }
        serde_json::Value::String(_) => serde_json::Value::String("string".to_owned()),
        serde_json::Value::Number(_) => serde_json::Value::String("number".to_owned()),
        serde_json::Value::Bool(_) => serde_json::Value::String("bool".to_owned()),
        serde_json::Value::Null => serde_json::Value::String("null".to_owned()),
    }
}

#[sqlx::test]
async fn epoch_at_least_is_a_max_floor(pool: PgPool) {
    let fx = Fixture::new(pool, "restore-floor").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    genesis_and_child(&fx, &w, &s).await;

    // A floor ABOVE old+1 wins: (1,2) → (7,2).
    let r = fx
        .authority
        .restore_bump_epochs(None, Some(7), NOW + 1)
        .await
        .unwrap();
    assert_eq!(r[0].new, gn(7, 2));

    // A floor BELOW old+1 is a no-op on the result (max semantics): (7,2) → (8,2), not (3,2).
    let r = fx
        .authority
        .restore_bump_epochs(None, Some(3), NOW + 2)
        .await
        .unwrap();
    assert_eq!(r[0].old, gn(7, 2));
    assert_eq!(r[0].new, gn(8, 2));
}

/// A bump past the JCS safe-integer bound (2^53 − 1) fails typed with NOTHING written or signed — and the
/// failure is all-or-nothing across the selection (an intact sibling row is rolled back too).
#[sqlx::test]
async fn bump_past_the_safe_integer_bound_fails_typed_and_writes_nothing(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "restore-bound").await;
    let (w, s_a, s_b) = (ws("w_acme"), skill("s_a"), skill("s_b"));
    let key = dev_key(0xE2);
    register(&fx, &w, &s_a, "dk_r", &key, "p_op").await;
    fx.authority
        .db()
        .seed_roster(&w, &s_b, &prin("p_op"))
        .await
        .unwrap();
    let g = publish(
        &fx,
        &key,
        "dk_r",
        &w,
        &s_a,
        "0e000000-0000-4000-8000-00000000000a",
        genesis(vec![file("a", b"1")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(g.current, Some(gn(1, 1)));
    let g2 = publish(
        &fx,
        &key,
        "dk_r",
        &w,
        &s_b,
        "0e000000-0000-4000-8000-00000000000b",
        genesis(vec![file("b", b"1")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(g2.current, Some(gn(1, 1)));

    // Seed s_a's row AT the bound (raw SQL — the row itself is still verifiable; only a bump would exceed).
    sqlx::query(
        "UPDATE current SET epoch = 9007199254740991 WHERE workspace_id = $1 AND skill_id = $2",
    )
    .bind(w.as_str())
    .bind(s_a.as_str())
    .execute(&pool)
    .await
    .unwrap();
    let record_a_before = fx.authority.read_signed_record(&w, &s_a).await.unwrap();
    let record_b_before = fx.authority.read_signed_record(&w, &s_b).await.unwrap();

    let err = fx
        .authority
        .restore_bump_epochs(None, None, NOW + 1)
        .await
        .expect_err("a bump past 2^53-1 must fail");
    assert!(matches!(err, AuthorityError::Internal(_)), "typed: {err:?}");
    let source = std::error::Error::source(&err).expect("a typed source");
    assert!(
        source.to_string().contains("safe-integer"),
        "the source names the bound: {source}"
    );

    // Nothing was written or signed — BOTH rows (the poisoned one and its intact sibling) are untouched.
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &s_a)
            .await
            .unwrap(),
        Some(gn(9_007_199_254_740_991, 1))
    );
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w, &s_b)
            .await
            .unwrap(),
        Some(gn(1, 1))
    );
    assert_eq!(
        fx.authority.read_signed_record(&w, &s_a).await.unwrap(),
        record_a_before
    );
    assert_eq!(
        fx.authority.read_signed_record(&w, &s_b).await.unwrap(),
        record_b_before
    );
}

#[sqlx::test]
async fn a_workspace_filter_touches_only_the_named_workspace(pool: PgPool) {
    let fx = Fixture::new(pool, "restore-filter").await;
    let (w_a, w_b, s) = (ws("w_acme"), ws("w_beta"), skill("s_deploy"));
    let key = dev_key(0xE3);
    register(&fx, &w_a, &s, "dk_a", &key, "p_op").await;
    register(&fx, &w_b, &s, "dk_b", &key, "p_op").await;
    let g = publish(
        &fx,
        &key,
        "dk_a",
        &w_a,
        &s,
        "0e000000-0000-4000-8000-0000000000a1",
        genesis(vec![file("a", b"1")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(g.current, Some(gn(1, 1)));
    let g2 = publish(
        &fx,
        &key,
        "dk_b",
        &w_b,
        &s,
        "0e000000-0000-4000-8000-0000000000b1",
        genesis(vec![file("b", b"1")]),
        gn(0, 0),
    )
    .await;
    assert_eq!(g2.current, Some(gn(1, 1)));

    let selection = [w_a.clone()];
    let reports = fx
        .authority
        .restore_bump_epochs(Some(&selection), None, NOW + 1)
        .await
        .unwrap();
    assert_eq!(reports.len(), 1, "only the named workspace is selected");
    assert_eq!(reports[0].workspace_id, w_a);
    assert_eq!(reports[0].new, gn(2, 1));

    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w_a, &s)
            .await
            .unwrap(),
        Some(gn(2, 1))
    );
    assert_eq!(
        fx.authority
            .db()
            .read_current_generation(&w_b, &s)
            .await
            .unwrap(),
        Some(gn(1, 1)),
        "the unnamed workspace is untouched"
    );
}
