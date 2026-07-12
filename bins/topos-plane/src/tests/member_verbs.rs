//! The member-lane VERB surface over the wire — the describe reads (me / channels / proposals / log /
//! reach) and the guarded row-op writes (follow / unfollow / protect), each authenticated by the ONE Bearer
//! workspace credential: happy paths, a role refusal, and the uniform 404.

use topos_types::requests::{WireChannelIndex, WireMe, WireProposalIndex, WireReach};

use super::*;

/// Seed a published genesis for `SKILL` authored by the owner device (a confirmed owner is a
/// confirmed-member device; genesis always lands).
async fn seed_owner_genesis(ctx: &EnrollCtx, op_id: &str) {
    ctx.authority()
        .seed_published_genesis(
            &WorkspaceId::parse(WS).unwrap(),
            &SkillId::parse(SKILL).unwrap(),
            OWNER_CRED,
            &OpId::parse(op_id).unwrap(),
            vec![file("SKILL.md", b"genesis v0\n")],
            AUTHOR,
            MESSAGE,
            Some("Deploy"),
            CREATED_AT,
            NOW,
        )
        .await
        .expect("seed genesis");
}


#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn the_describe_reads_serve_a_member(pool: PgPool) {
    let ctx = enroll_setup(pool, "verbs-describe").await;
    seed_owner_genesis(&ctx, "c0000000-0000-4000-8000-000000000001").await;
    let auth = [("authorization", format!("Bearer {OWNER_CRED}"))];
    let auth: Vec<(&str, &str)> = auth.iter().map(|(k, v)| (*k, v.as_str())).collect();

    // me — the owner's own membership + address.
    let (s, _, b) = send(ctx.app(), get(&format!("/v1/workspaces/{WS}/me"), &auth)).await;
    assert_eq!(s, StatusCode::OK);
    let me: WireMe = serde_json::from_slice(&b).expect("a WireMe");
    assert_eq!(me.role, "owner");
    assert_eq!(me.name, WS_NAME);
    assert!(
        me.address.ends_with(&format!("/{WS_NAME}")),
        "address: {}",
        me.address
    );

    // channels — the structural `everyone` is present, builtin, and the caller belongs.
    let (s, _, b) = send(
        ctx.app(),
        get(&format!("/v1/workspaces/{WS}/channels"), &auth),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let channels: WireChannelIndex = serde_json::from_slice(&b).expect("a WireChannelIndex");
    let everyone = channels
        .channels
        .iter()
        .find(|c| c.name == "everyone")
        .expect("the structural everyone");
    assert!(everyone.builtin && everyone.member);

    // proposals — none open yet.
    let (s, _, b) = send(
        ctx.app(),
        get(&format!("/v1/workspaces/{WS}/proposals"), &auth),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let proposals: WireProposalIndex = serde_json::from_slice(&b).expect("a WireProposalIndex");
    assert!(proposals.proposals.is_empty());
    // reach — the publish audience (the author self-follows, so at least one person is entitled).
    let (s, _, b) = send(
        ctx.app(),
        get(&format!("/v1/workspaces/{WS}/skills/{SKILL}/reach"), &auth),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let reach: WireReach = serde_json::from_slice(&b).expect("a WireReach");
    assert!(
        reach.persons >= 1,
        "at least the author is entitled: {reach:?}"
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_follow_then_unfollow_row_op_reports_its_status(pool: PgPool) {
    let ctx = enroll_setup(pool, "verbs-follow").await;
    seed_owner_genesis(&ctx, "c0000000-0000-4000-8000-000000000002").await;

    // PUT follows/{SKILL} → a 200 envelope carrying status "followed".
    let (s, _, b) = send(
        ctx.app(),
        req_json_auth(
            "PUT",
            &format!("/v1/workspaces/{WS}/follows/{SKILL}"),
            serde_json::json!({}),
            OWNER_CRED,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let env = envelope(&b);
    assert!(env.ok, "follow ok: {env:?}");
    assert_eq!(env.data["status"], "followed");

    // DELETE follows/{SKILL} → status "unfollowed".
    let (s, _, b) = send(
        ctx.app(),
        req_json_auth(
            "DELETE",
            &format!("/v1/workspaces/{WS}/follows/{SKILL}"),
            serde_json::json!({}),
            OWNER_CRED,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(envelope(&b).data["status"], "unfollowed");
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_member_tightening_protection_is_a_200_denied(pool: PgPool) {
    let ctx = enroll_setup(pool, "verbs-protect-role").await;
    seed_owner_genesis(&ctx, "c0000000-0000-4000-8000-000000000003").await;
    // A confirmed MEMBER device (tightening a skill to `reviewed` takes reviewer+).
    let ws = WorkspaceId::parse(WS).unwrap();
    let member = Principal::parse(MEMBER_PRINCIPAL).unwrap();
    ctx.authority()
        .seed_workspace_member(&ws, &member, "member", "confirmed")
        .await
        .unwrap();
    ctx.authority()
        .seed_device(
            &ws,
            MEMBER_DK,
            &dev_pubkey(MEMBER_SEED),
            &member,
            false,
            MEMBER_CRED,
        )
        .await
        .unwrap();

    let (s, _, b) = send(
        ctx.app(),
        req_json_auth(
            "PUT",
            &format!("/v1/workspaces/{WS}/skills/{SKILL}/protection"),
            serde_json::json!({ "level": "reviewed" }),
            MEMBER_CRED,
        ),
    )
    .await;
    // A role refusal is a 200 + DENIED (the actor is an authenticated member — nothing to hide).
    assert_eq!(s, StatusCode::OK);
    let env = envelope(&b);
    assert!(!env.ok, "a member's tighten must be denied: {env:?}");
    assert_eq!(
        env.error.expect("DENIED carries a WireError").code,
        "REVIEWER_ROLE_REQUIRED"
    );
}

#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn a_member_verb_with_no_credential_is_the_uniform_404(pool: PgPool) {
    let ctx = enroll_setup(pool, "verbs-404").await;
    // A describe read with no Authorization header — the uniform miss.
    let (s, _, _) = send(ctx.app(), get(&format!("/v1/workspaces/{WS}/me"), &[])).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // A bodyless row-op write (PUT follows) with no Authorization header — the missing-credential miss.
    let (s, _, _) = send(
        ctx.app(),
        Request::builder()
            .method("PUT")
            .uri(format!("/v1/workspaces/{WS}/follows/anything"))
            .body(axum::body::Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
