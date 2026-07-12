//! The web-session READ lane — the member-scoped ops' authorization matrix, the lane-threaded
//! re-authorize guard (both directions: reclaimed ⇒ 404, corruption ⇒ Integrity), the staleness
//! parity between the session list and the index count, and the two lanes' deliberate divergence
//! (per-skill roster vs workspace membership).
use super::*;
use crate::enroll::DeploymentMode;

const CLOUD: DeploymentMode = DeploymentMode::Cloud;

/// Seat a CONFIRMED workspace member (the session lane's whole entitlement — deliberately NO per-skill
/// roster row anywhere in this suite unless a test says so).
async fn seat(fx: &Fixture, w: &WorkspaceId, email: &str, role: &str) {
    fx.authority
        .db()
        .seed_workspace_member(w, &prin(email), role, "confirmed")
        .await
        .unwrap();
}

/// One real genesis publish; returns (current commit, its digest, the body object id).
async fn published_skill(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    key: &[u8; 32],
    op_id: &str,
    body: &[u8],
) -> (CommitId, [u8; 32], ObjectId) {
    let receipt = publish(
        fx,
        key,
        "dk",
        w,
        s,
        op_id,
        genesis(vec![file("SKILL.md", body)]),
        gn(0, 0),
    )
    .await;
    (
        receipt.version_id.expect("OK receipt carries the id"),
        receipt
            .bundle_digest
            .expect("OK receipt carries the digest"),
        object_id(body),
    )
}

#[sqlx::test]
async fn a_confirmed_member_of_every_role_reads_all_five_ops_with_no_roster_row(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-matrix").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(50);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    let body = b"# deploy\nship it\n";
    let (g, g_digest, obj) = published_skill(
        &fx,
        &w,
        &s,
        &key,
        "50000000-0000-4000-8000-000000000001",
        body,
    )
    .await;
    let g_hex = digest::to_hex(&g.0);
    let o_hex = digest::to_hex(&obj.0);

    for (email, role) in [
        ("owner@acme.com", "owner"),
        ("reviewer@acme.com", "reviewer"),
        ("member@acme.com", "member"),
    ] {
        seat(&fx, &w, email, role).await;
        // The acting email arrives MIXED-CASE (the canonical fold is the ops' job, not the caller's).
        let acting = email.to_uppercase();

        let index = a.list_skills_session(&w, &acting, CLOUD).await.unwrap();
        assert_eq!(index.len(), 1);
        assert_eq!(index[0].skill_id, "s_deploy");
        assert_eq!(index[0].version_id, g.0);
        assert_eq!(index[0].bundle_digest, g_digest);
        assert_eq!(index[0].generation, gn(1, 1));
        assert_eq!(index[0].open_proposals, 0);

        let cur = a
            .read_current_session(&w, "s_deploy", &acting, CLOUD)
            .await
            .unwrap()
            .expect("a published skill has a pointer");
        assert_eq!(cur.version_id, g.0);

        let meta = a
            .read_version_metadata_session(&w, "s_deploy", &g_hex, &acting, CLOUD)
            .await
            .unwrap();
        assert_eq!(meta.bundle_digest, g_digest);

        let bytes = a
            .serve_object_session(&w, "s_deploy", &o_hex, &acting, CLOUD)
            .await
            .unwrap();
        assert_eq!(bytes, body);

        assert!(
            a.list_open_proposals_session(&w, "s_deploy", &acting, CLOUD)
                .await
                .unwrap()
                .is_empty()
        );
    }
}

#[sqlx::test]
async fn every_pre_gate_miss_is_the_same_notfound(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-uniform").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(51);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    published_skill(
        &fx,
        &w,
        &s,
        &key,
        "51000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;
    // An INVITED (unconfirmed) seat and a confirmed member for the malformed-input probes.
    fx.authority
        .db()
        .seed_workspace_member(&w, &prin("invited@acme.com"), "member", "invited")
        .await
        .unwrap();
    seat(&fx, &w, "member@acme.com", "member").await;

    // (stranger, invited-unconfirmed, unknown workspace, malformed email) — each the uniform miss on
    // every op that takes it.
    for acting in ["stranger@evil.com", "invited@acme.com", ""] {
        assert!(matches!(
            a.list_skills_session(&w, acting, CLOUD).await,
            Err(AuthorityError::NotFound)
        ));
        assert!(matches!(
            a.read_current_session(&w, "s_deploy", acting, CLOUD).await,
            Err(AuthorityError::NotFound)
        ));
        assert!(matches!(
            a.serve_object_session(&w, "s_deploy", &"0".repeat(64), acting, CLOUD)
                .await,
            Err(AuthorityError::NotFound)
        ));
        assert!(matches!(
            a.read_version_metadata_session(&w, "s_deploy", &"0".repeat(64), acting, CLOUD)
                .await,
            Err(AuthorityError::NotFound)
        ));
        assert!(matches!(
            a.list_open_proposals_session(&w, "s_deploy", acting, CLOUD)
                .await,
            Err(AuthorityError::NotFound)
        ));
    }
    assert!(matches!(
        a.list_skills_session(&ws("w_ghost"), "member@acme.com", CLOUD)
            .await,
        Err(AuthorityError::NotFound)
    ));
    // Self-host: a confirmed member is ANSWERED exactly like a hosted plane — the acting gate is the
    // confirmed-roster-seat check, identical on both postures (the product app serves self-hosted
    // deployments through this session lane), so the mode no longer gates these reads.
    let sh = DeploymentMode::SelfHost;
    assert_eq!(
        a.list_skills_session(&w, "member@acme.com", sh)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        a.read_current_session(&w, "s_deploy", "member@acme.com", sh)
            .await
            .unwrap()
            .is_some()
    );
    // A malformed skill id is the SAME miss, post-gate (never a distinguishable 400).
    assert!(matches!(
        a.read_current_session(&w, "not a skill id!", "member@acme.com", CLOUD)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn a_staled_proposal_vanishes_from_the_session_list_and_the_index_count(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-stale").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(52);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    let (g, _d, _o) = published_skill(
        &fx,
        &w,
        &s,
        &key,
        "52000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;
    seat(&fx, &w, "member@acme.com", "member").await;
    do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "52000000-0000-4000-8000-000000000002",
        child(
            g,
            vec![file("SKILL.md", b"v0"), file("NEW.md", b"proposed")],
        ),
        gn(1, 1),
    )
    .await;

    let listed = a
        .list_open_proposals_session(&w, "s_deploy", "member@acme.com", CLOUD)
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    let index = a
        .list_skills_session(&w, "member@acme.com", CLOUD)
        .await
        .unwrap();
    assert_eq!(index[0].open_proposals, 1);
    // The index carries the catalog's minted name (the genesis publish registered the skill: `s_deploy`
    // folds to `s-deploy`) and its lifecycle status.
    assert_eq!(index[0].name, "s-deploy");
    assert_eq!(index[0].status, "active");

    // A direct publish advances `current` past the proposal's base — the eventless stale transition.
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "52000000-0000-4000-8000-000000000003",
        child(g, vec![file("SKILL.md", b"v2")]),
        gn(1, 1),
    )
    .await;

    // The session list and the index count agree (count delegates to the SAME listing statement).
    assert!(
        a.list_open_proposals_session(&w, "s_deploy", "member@acme.com", CLOUD)
            .await
            .unwrap()
            .is_empty()
    );
    let index = a
        .list_skills_session(&w, "member@acme.com", CLOUD)
        .await
        .unwrap();
    assert_eq!(index[0].open_proposals, 0);
}

#[sqlx::test]
async fn a_reclaimed_object_reads_404_on_the_member_lane_never_integrity(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-reclaim").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(53);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    let (g, _d, _o) = published_skill(
        &fx,
        &w,
        &s,
        &key,
        "53000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;
    seat(&fx, &w, "member@acme.com", "member").await;
    let unique = b"proposal-only bytes";
    do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "53000000-0000-4000-8000-000000000002",
        child(g, vec![file("SKILL.md", b"v0"), file("NEW.md", unique)]),
        gn(1, 1),
    )
    .await;
    let x = object_id(unique);
    let x_hex = digest::to_hex(&x.0);

    // Readable through the open proposal on the member lane.
    assert_eq!(
        a.serve_object_session(&w, "s_deploy", &x_hex, "member@acme.com", CLOUD)
            .await
            .unwrap(),
        unique
    );

    // Stale it, reclaim the unique bytes, and the member-lane read is the uniform 404 — never an
    // Integrity fault (the re-authorize-on-miss guard re-gates on the MEMBER lane).
    publish(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "53000000-0000-4000-8000-000000000003",
        child(g, vec![file("SKILL.md", b"v2")]),
        gn(1, 1),
    )
    .await;
    assert!(gc::run_gc(a, &w, NOW).await.unwrap() >= 1);
    assert!(matches!(
        a.serve_object_session(&w, "s_deploy", &x_hex, "member@acme.com", CLOUD)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

/// The lane-threading pin (the design-gate MAJOR): genuine corruption under a MEMBER-lane read must
/// surface Integrity, not fold to 404. The reader holds NO per-skill roster row, so a wrong-lane
/// (skill-roster) re-authorize inside the guard would return None and mask the fault as the uniform
/// miss — this test fails under exactly that bug.
#[sqlx::test]
async fn genuine_corruption_on_the_member_lane_is_integrity_not_masked_as_404(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-corrupt").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(54);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    let body = b"present then destroyed";
    let (_g, _d, obj) = published_skill(
        &fx,
        &w,
        &s,
        &key,
        "54000000-0000-4000-8000-000000000001",
        body,
    )
    .await;
    seat(&fx, &w, "member@acme.com", "member").await;
    let o_hex = digest::to_hex(&obj.0);
    assert_eq!(
        a.serve_object_session(&w, "s_deploy", &o_hex, "member@acme.com", CLOUD)
            .await
            .unwrap(),
        body
    );

    // Destroy the bytes underneath the still-trunk-rooted object (the presence row stays `present`).
    let (loc, goid) = a.db().object_dispatch(&w, obj).await.unwrap().unwrap();
    assert_eq!(loc, Location::Git);
    a.open_store(&w).unwrap().delete_loose_object(goid).unwrap();

    assert!(matches!(
        a.serve_object_session(&w, "s_deploy", &o_hex, "member@acme.com", CLOUD)
            .await,
        Err(AuthorityError::Integrity(_))
    ));
}

/// The wrong-bytes tamper pin: the corruption test above DELETES the object; this one swaps the stored
/// bytes ON DISK under the same recorded id, so the property pinned is the store's get()-time
/// sha256(bytes) == object_id re-verification — tampered bytes under an honest id must surface
/// Integrity on the member lane, never the bytes and never the uniform miss.
#[sqlx::test]
async fn tampered_bytes_under_a_recorded_id_read_integrity_on_the_member_lane(pool: PgPool) {
    // A 1-byte threshold routes the published body to the LARGE-OBJECT store, whose final file is a
    // plain overwritable path (a loose git object is zlib-framed, so the large store is where a
    // same-id/different-bytes swap is cleanly constructed). `put` re-checks the hash, so the tamper
    // is a direct filesystem overwrite of the final file.
    let fx = Fixture::with_large_limits(pool, "sr-tamper", 1, 1 << 30).await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(58);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    let body = b"genuine bytes, published for real";
    let (_g, _d, obj) = published_skill(
        &fx,
        &w,
        &s,
        &key,
        "58000000-0000-4000-8000-000000000001",
        body,
    )
    .await;
    seat(&fx, &w, "member@acme.com", "member").await;
    let o_hex = digest::to_hex(&obj.0);
    assert_eq!(
        a.db().object_location(&w, obj).await.unwrap(),
        Some(Location::LargeLocal),
        "the tamper target must be the offloaded store"
    );
    assert_eq!(
        a.serve_object_session(&w, "s_deploy", &o_hex, "member@acme.com", CLOUD)
            .await
            .unwrap(),
        body
    );

    // Overwrite the stored file's bytes with DIFFERENT content — same path, same recorded id (the
    // store's documented final layout: `<large_root>/<ws>/objects/<aa>/<bb>/<64-hex>`).
    let tampered = fx
        .dir
        .join("large")
        .join("w_acme")
        .join("objects")
        .join(&o_hex[0..2])
        .join(&o_hex[2..4])
        .join(&o_hex);
    assert!(tampered.is_file(), "the offloaded final file must exist");
    std::fs::write(&tampered, b"attacker bytes under the honest id").unwrap();

    // The presence row still says `present`, the member is confirmed, the id is honest — the only
    // tripwire left is the read-time hash re-verification, and it must fault, not serve or 404.
    assert!(matches!(
        a.serve_object_session(&w, "s_deploy", &o_hex, "member@acme.com", CLOUD)
            .await,
        Err(AuthorityError::Integrity(_))
    ));
}

#[sqlx::test]
async fn a_rejected_candidate_version_is_the_uniform_miss_on_the_member_lane(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-rejected").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(55);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    let (g, _d, _o) = published_skill(
        &fx,
        &w,
        &s,
        &key,
        "55000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;
    seat(&fx, &w, "member@acme.com", "member").await;
    let (_r, cp, digest_) = do_propose(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "55000000-0000-4000-8000-000000000002",
        child(g, vec![file("SKILL.md", b"v0"), file("NEW.md", b"prop")]),
        gn(1, 1),
    )
    .await;
    let cp_hex = digest::to_hex(&cp.0);
    assert!(
        a.read_version_metadata_session(&w, "s_deploy", &cp_hex, "member@acme.com", CLOUD)
            .await
            .is_ok()
    );
    do_reject(
        &fx,
        &key,
        "dk",
        &w,
        &s,
        "55000000-0000-4000-8000-000000000003",
        cp,
        digest_,
        gn(1, 1),
    )
    .await;
    assert!(matches!(
        a.read_version_metadata_session(&w, "s_deploy", &cp_hex, "member@acme.com", CLOUD)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn a_publish_is_visible_to_the_next_index_call(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-fresh").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(56);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    seat(&fx, &w, "member@acme.com", "member").await;

    // Before any publish: the gate admits the member and the catalog is honestly empty.
    assert!(
        a.list_skills_session(&w, "member@acme.com", CLOUD)
            .await
            .unwrap()
            .is_empty()
    );

    let (g, g_digest, _o) = published_skill(
        &fx,
        &w,
        &s,
        &key,
        "56000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;
    let index = a
        .list_skills_session(&w, "member@acme.com", CLOUD)
        .await
        .unwrap();
    assert_eq!(index.len(), 1);
    assert_eq!(index[0].version_id, g.0);
    assert_eq!(index[0].bundle_digest, g_digest);
}

/// Both read lanes now gate on the SAME predicate — a CONFIRMED workspace member. A per-skill roster
/// row grants NOTHING on either lane; membership is the entitlement everywhere. Two pins: a principal
/// WITH a roster row but WITHOUT membership is the uniform miss on BOTH lanes, and a confirmed member
/// with NO roster row reads on BOTH lanes.
#[sqlx::test]
async fn both_lanes_gate_on_membership_not_roster(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-lanes").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let key = dev_key(57);
    register(&fx, &w, &s, "dk", &key, "p_author").await;
    let body = b"v0";
    let (_g, _d, obj) = published_skill(
        &fx,
        &w,
        &s,
        &key,
        "57000000-0000-4000-8000-000000000001",
        body,
    )
    .await;
    let o_hex = digest::to_hex(&obj.0);

    // A per-skill roster row but NO confirmed membership → the uniform miss on BOTH lanes (roster grants
    // nothing): the device lane (public `read_object`, the shared membership gate) and the session lane.
    let rostered = prin("rostered@acme.com");
    assert!(matches!(
        a.read_object(&rostered, &w, &s, obj).await,
        Err(AuthorityError::NotFound)
    ));
    assert!(matches!(
        a.serve_object_session(&w, "s_deploy", &o_hex, "rostered@acme.com", CLOUD)
            .await,
        Err(AuthorityError::NotFound)
    ));

    // A confirmed member with NO roster row → reads on BOTH lanes (membership is the entitlement).
    seat(&fx, &w, "member@acme.com", "member").await;
    assert_eq!(
        a.serve_object_session(&w, "s_deploy", &o_hex, "member@acme.com", CLOUD)
            .await
            .unwrap(),
        body
    );
    assert_eq!(
        a.read_object(&prin("member@acme.com"), &w, &s, obj)
            .await
            .unwrap(),
        body
    );
}

/// `skill_commit.bundle_digest` is nullable (rows can predate the column), but a `current`-pointed
/// version must carry one — a NULL under the index is Integrity, never a silent skip or a miss.
/// (`seed_commit` writes no digest, which is exactly the corrupt shape here.)
#[sqlx::test]
async fn a_null_digest_under_a_current_row_is_integrity(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-nulldig").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "member@acme.com", "member").await;
    let c = CommitId([0x77; 32]);
    a.db().seed_commit(&w, &s, c, &[]).await.unwrap();
    a.db().seed_current(&w, &s, c, 1, 1).await.unwrap();
    assert!(matches!(
        a.list_skills_session(&w, "member@acme.com", CLOUD).await,
        Err(AuthorityError::Integrity(_))
    ));
}

// ── the DEVICE catalog read (`list --remote`) — the workspace-membership device lane ────────────────────
//
// Authorized by a NON-REVOKED registered device (resolved by its presented workspace credential) whose
// bound principal is a CONFIRMED workspace member — on BOTH cloud and self-host (the lane never consults
// a deployment mode). The credential lookup binds the caller's claimed workspace, so a device is only
// ever presented against the workspace it was registered in. Every miss is the one uniform NotFound.

/// Seed a NON-REVOKED reader device bound to `email`, and seat that email as a CONFIRMED member — the
/// device catalog lane's whole entitlement (deliberately NO per-skill roster row).
async fn seat_device_member(
    fx: &Fixture,
    w: &WorkspaceId,
    dkid: &str,
    key: &[u8; 32],
    email: &str,
) {
    fx.authority
        .db()
        .seed_device(w, dkid, key, &prin(email), false, &cred(w, dkid))
        .await
        .unwrap();
    seat(fx, w, email, "member").await;
}

#[sqlx::test]
async fn a_member_device_reads_the_catalog(pool: PgPool) {
    let fx = Fixture::new(pool, "sd-ok").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let pk = dev_key(70);
    register(&fx, &w, &s, "dk", &pk, "p_author").await;
    let body = b"# deploy\nship it\n";
    let (g, g_digest, _o) = published_skill(
        &fx,
        &w,
        &s,
        &pk,
        "70000000-0000-4000-8000-000000000001",
        body,
    )
    .await;

    // A DISTINCT reader device whose principal is a confirmed member, holding NO per-skill roster row.
    let rk = dev_key(71);
    seat_device_member(&fx, &w, "dk_read", &rk, "reader@acme.com").await;

    let idx = a
        .list_skills_device(&w, &cred(&w, "dk_read"), NOW)
        .await
        .unwrap();
    assert_eq!(idx.len(), 1);
    assert_eq!(idx[0].skill_id, "s_deploy");
    assert_eq!(idx[0].version_id, g.0);
    assert_eq!(idx[0].bundle_digest, g_digest);
    assert_eq!(idx[0].generation, gn(1, 1));
    assert_eq!(idx[0].open_proposals, 0);
}

/// Both read lanes serve the SAME confirmed member on a self-host plane — device auth and a
/// session-verified email are two authentications of the one membership gate, which is identical on both
/// postures.
#[sqlx::test]
async fn both_read_lanes_serve_a_confirmed_member_on_self_host(pool: PgPool) {
    let fx = Fixture::new(pool, "sd-selfhost").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let pk = dev_key(72);
    register(&fx, &w, &s, "dk", &pk, "p_author").await;
    published_skill(
        &fx,
        &w,
        &s,
        &pk,
        "72000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;

    let rk = dev_key(73);
    seat_device_member(&fx, &w, "dk_read", &rk, "reader@acme.com").await;

    // The device lane serves the catalog — it never consults deployment mode.
    assert_eq!(
        a.list_skills_device(&w, &cred(&w, "dk_read"), NOW)
            .await
            .unwrap()
            .len(),
        1
    );
    // The session lane, told the plane is self-host, serves the SAME confirmed member identically — the
    // acting gate is the confirmed seat, not the posture.
    assert_eq!(
        a.list_skills_session(&w, "reader@acme.com", DeploymentMode::SelfHost)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[sqlx::test]
async fn an_unknown_or_cross_workspace_device_is_notfound(pool: PgPool) {
    let fx = Fixture::new(pool, "sd-badkey").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let pk = dev_key(74);
    register(&fx, &w, &s, "dk", &pk, "p_author").await;
    published_skill(
        &fx,
        &w,
        &s,
        &pk,
        "74000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;
    let rk = dev_key(75);
    seat_device_member(&fx, &w, "dk_read", &rk, "reader@acme.com").await;

    // An UNKNOWN credential (never issued) is the uniform miss — the credential lookup fails.
    assert!(matches!(
        a.list_skills_device(&w, &cred(&w, "dk_ghost"), NOW).await,
        Err(AuthorityError::NotFound)
    ));
    // The credential lookup binds the CLAIMED workspace: `dk_read`'s credential is registered in
    // `w_acme`, so presenting it against a DIFFERENT workspace resolves nothing → the uniform miss (no
    // cross-workspace catalog leak).
    assert!(matches!(
        a.list_skills_device(&ws("w_other"), &cred(&w, "dk_read"), NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn a_revoked_device_is_notfound_on_the_catalog_read(pool: PgPool) {
    let fx = Fixture::new(pool, "sd-revoked").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let pk = dev_key(76);
    register(&fx, &w, &s, "dk", &pk, "p_author").await;
    published_skill(
        &fx,
        &w,
        &s,
        &pk,
        "76000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;

    // A REVOKED reader device whose principal is nonetheless a confirmed member.
    let rk = dev_key(77);
    a.db()
        .seed_device(
            &w,
            "dk_read",
            &rk,
            &prin("reader@acme.com"),
            true,
            &cred(&w, "dk_read"),
        )
        .await
        .unwrap();
    seat(&fx, &w, "reader@acme.com", "member").await;
    assert!(matches!(
        a.list_skills_device(&w, &cred(&w, "dk_read"), NOW).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn a_registered_device_whose_principal_is_not_a_confirmed_member_is_notfound(pool: PgPool) {
    let fx = Fixture::new(pool, "sd-nonmember").await;
    let a = &fx.authority;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    let pk = dev_key(78);
    register(&fx, &w, &s, "dk", &pk, "p_author").await;
    published_skill(
        &fx,
        &w,
        &s,
        &pk,
        "78000000-0000-4000-8000-000000000001",
        b"v0",
    )
    .await;

    // A registered, non-revoked device, but its principal holds no confirmed seat.
    let rk = dev_key(79);
    a.db()
        .seed_device(
            &w,
            "dk_read",
            &rk,
            &prin("stranger@acme.com"),
            false,
            &cred(&w, "dk_read"),
        )
        .await
        .unwrap();
    assert!(matches!(
        a.list_skills_device(&w, &cred(&w, "dk_read"), NOW).await,
        Err(AuthorityError::NotFound)
    ));
    // Even an INVITED (unconfirmed) seat is not a confirmed member → still the uniform miss.
    a.db()
        .seed_workspace_member(&w, &prin("stranger@acme.com"), "member", "invited")
        .await
        .unwrap();
    assert!(matches!(
        a.list_skills_device(&w, &cred(&w, "dk_read"), NOW).await,
        Err(AuthorityError::NotFound)
    ));
}
