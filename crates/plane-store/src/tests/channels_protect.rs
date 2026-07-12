//! Channels — the per-bundle protection cascade + the downgrade interplay.
//!
//! These drive the `protect` setter (skill protection / channel mode) and the way the resolved
//! per-bundle protection feeds the publish/revert gate. The three cases already proven verbatim by
//! `set_current.rs` are NOT re-tested here (referenced in comments): a member's direct publish
//! DOWNGRADES on a reviewed bundle, a reviewer's LANDS, and a per-skill `open` pin beats a reviewed
//! workspace default. What lives here is the OTHER pin direction, the protect-setter role matrix, the
//! pending-proposal-survives-a-loosening interplay, and the placement-independent-of-the-version-gate
//! property of `publish --to`.

use super::*;

use crate::channels::{ProtectKind, ProtectLevel, ProtectOutcome};

const ALICE: &str = "alice@acme.com";
const BOB: &str = "bob@acme.com";
const REV: &str = "rev@acme.com";

async fn seat(fx: &Fixture, w: &WorkspaceId, dkid: &str, seed: u8, principal: &str, role: &str) {
    let p = prin(principal);
    fx.authority
        .db()
        .seed_device(w, dkid, &dev_key(seed), &p, false, &cred(w, dkid))
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(w, &p, role, "confirmed")
        .await
        .unwrap();
}

#[allow(clippy::too_many_arguments)]
async fn gpub(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &SkillId,
    dkid: &str,
    op_id: &str,
    files: Vec<UploadedFile>,
    display_name: &str,
    channel: Option<&str>,
) -> crate::SetCurrentReceipt {
    let auth = DeviceOpAuth {
        credential: cred(w, dkid),
        op: DeviceOp::PublishDirect,
        expected: gn(0, 0),
    };
    fx.authority
        .publish(
            w,
            s,
            &op(op_id),
            genesis(files),
            auth,
            Some(display_name),
            channel,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap()
}

async fn open_proposals(pool: &PgPool, w: &str, skill_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::int8 FROM proposals WHERE workspace_id = $1 AND skill_id = $2 AND status = 'open'",
    )
    .bind(w)
    .bind(skill_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn channel_places(pool: &PgPool, w: &str, channel_id: &str, skill_id: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*)::int8 FROM channel_skills \
         WHERE workspace_id = $1 AND channel_id = $2 AND skill_id = $3",
    )
    .bind(w)
    .bind(channel_id)
    .bind(skill_id)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn detail_str(r: &crate::SetCurrentReceipt, key: &str) -> Option<String> {
    r.details
        .as_ref()
        .and_then(|d| d.get(key))
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// A per-skill `reviewed` PIN beats an OPEN workspace default: with review-required OFF, pinning the
/// bundle `reviewed` makes a plain member's direct child publish DOWNGRADE to a proposal. (The other
/// direction — an `open` pin beating a REVIEWED default — is proven verbatim by
/// `set_current::a_per_skill_open_pin_overrides_the_review_required_default`; the member-downgrade and
/// reviewer-lands base cases by that file's sibling tests.)
#[sqlx::test]
async fn a_per_skill_reviewed_pin_beats_an_open_workspace_default(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chp-reviewed-pin").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_rev", 11, REV, "reviewer").await;
    seat(&fx, &w, "dk_mem", 12, BOB, "member").await;
    // Workspace default is OPEN (review-required never turned on).
    let g = gpub(
        &fx,
        &w,
        &s,
        "dk_mem",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
        None,
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // A reviewer pins THIS bundle reviewed (tightening takes reviewer+).
    assert_eq!(
        fx.authority
            .protect(
                &w,
                &cred(&w, "dk_rev"),
                ProtectKind::Skill,
                s.as_str(),
                ProtectLevel::Protected,
                CREATED_AT
            )
            .await
            .unwrap(),
        ProtectOutcome::Set
    );
    // A plain member's direct child publish now DOWNGRADES (the pin beats the open default).
    let r = fx
        .authority
        .publish(
            &w,
            &s,
            &op("aaaaaaaa-0000-4000-8000-000000000002"),
            child(c0, vec![file("f", b"v1")]),
            DeviceOpAuth {
                credential: cred(&w, "dk_mem"),
                op: DeviceOp::PublishDirect,
                expected: g.current.unwrap(),
            },
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, TerminalOutcome::NeedsReview);
    assert_eq!(
        r.details
            .as_ref()
            .and_then(|d| d.get("downgraded"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    // The pointer is frozen at the genesis.
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(c0)
    );
}

/// The protect-setter role matrix: TIGHTENING a skill to `reviewed` takes reviewer+ (a member is
/// `ReviewerRoleRequired`), and LOOSENING back to `open` takes an owner (a reviewer is
/// `OwnerRoleRequired`) — the asymmetric "tighten protects, loosen widens" gate.
#[sqlx::test]
async fn protect_skill_role_matrix_tighten_reviewer_loosen_owner(pool: PgPool) {
    let fx = Fixture::new(pool, "chp-role-matrix").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_rev", 12, REV, "reviewer").await;
    seat(&fx, &w, "dk_mem", 13, BOB, "member").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
        None,
    )
    .await;

    let protect = async |cred_dk: &str, level: ProtectLevel| {
        fx.authority
            .protect(
                &w,
                &cred(&w, cred_dk),
                ProtectKind::Skill,
                s.as_str(),
                level,
                CREATED_AT,
            )
            .await
            .unwrap()
    };
    // Tighten: a member is refused; a reviewer lands.
    assert_eq!(
        protect("dk_mem", ProtectLevel::Protected).await,
        ProtectOutcome::ReviewerRoleRequired
    );
    assert_eq!(
        protect("dk_rev", ProtectLevel::Protected).await,
        ProtectOutcome::Set
    );
    // Loosen: a reviewer is refused; the owner lands.
    assert_eq!(
        protect("dk_rev", ProtectLevel::Open).await,
        ProtectOutcome::OwnerRoleRequired
    );
    assert_eq!(
        protect("dk_owner", ProtectLevel::Open).await,
        ProtectOutcome::Set
    );
}

/// A pending proposal SURVIVES a loosening of its skill's protection (it still awaits its verdict),
/// and once the bundle is loosened to `open` the four-eyes rule no longer bites — so the ORIGINAL
/// proposer can now self-approve their own open proposal and land it.
#[sqlx::test]
async fn a_pending_proposal_survives_a_loosening_and_the_proposer_can_then_self_approve(
    pool: PgPool,
) {
    let fx = Fixture::new(pool.clone(), "chp-survive-loosen").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner").await;
    seat(&fx, &w, "dk_mem", 12, BOB, "member").await;
    let g = gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
        None,
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &s)
        .await
        .unwrap()
        .unwrap();

    // Pin the bundle reviewed (an owner tightens), then the member opens a proposal on it.
    fx.authority
        .protect(
            &w,
            &cred(&w, "dk_owner"),
            ProtectKind::Skill,
            s.as_str(),
            ProtectLevel::Protected,
            CREATED_AT,
        )
        .await
        .unwrap();
    let key = dev_key(12);
    let (pr, cprop, digest) = do_propose(
        &fx,
        &key,
        "dk_mem",
        &w,
        &s,
        "cccccccc-0000-4000-8000-000000000001",
        child(c0, vec![file("f", b"v1")]),
        g.current.unwrap(),
    )
    .await;
    assert_eq!(pr.outcome, TerminalOutcome::NeedsReview);
    assert_eq!(open_proposals(&pool, "w_acme", "s_deploy").await, 1);

    // The owner LOOSENS the bundle back to open — the pending proposal is untouched (still open).
    fx.authority
        .protect(
            &w,
            &cred(&w, "dk_owner"),
            ProtectKind::Skill,
            s.as_str(),
            ProtectLevel::Open,
            CREATED_AT,
        )
        .await
        .unwrap();
    assert_eq!(
        open_proposals(&pool, "w_acme", "s_deploy").await,
        1,
        "loosening never touches a pending proposal"
    );

    // Four-eyes is now OFF (the bundle is open) — the ORIGINAL proposer self-approves and it LANDS.
    let ar = do_approve(
        &fx,
        &key,
        "dk_mem",
        &w,
        &s,
        "cccccccc-0000-4000-8000-000000000002",
        cprop,
        digest,
        g.current.unwrap(),
    )
    .await;
    assert_eq!(
        ar.outcome,
        TerminalOutcome::Ok,
        "self-approve lands once the bundle is open"
    );
    assert_eq!(
        fx.authority.db().read_current_commit(&w, &s).await.unwrap(),
        Some(cprop),
        "the pointer moved to the approved candidate"
    );
    assert_eq!(
        open_proposals(&pool, "w_acme", "s_deploy").await,
        0,
        "the proposal is resolved"
    );
}

/// `publish --to <channel>` on a reviewed bundle by a member: the VERSION downgrades to a proposal,
/// but the placement is STILL applied — curation is independent of the version gate. The receipt's
/// details carry BOTH `downgraded` and the placement outcome, and the `channel_skills` row exists even
/// though the new version never became current.
#[sqlx::test]
async fn publish_to_on_a_reviewed_bundle_downgrades_the_version_but_still_places(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chp-to-downgrade").await;
    let (w, sa, sb) = (ws("w_acme"), skill("s_deploy"), skill("s_other"));
    seat(&fx, &w, "dk_mem", 11, BOB, "member").await;
    // A: born in everyone. B: born in `ops`, which pre-creates the channel so A's later --to is a Placed.
    let g = gpub(
        &fx,
        &w,
        &sa,
        "dk_mem",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("f", b"v0")],
        "Deploy",
        None,
    )
    .await;
    gpub(
        &fx,
        &w,
        &sb,
        "dk_mem",
        "bbbbbbbb-0000-4000-8000-000000000001",
        vec![file("f", b"other")],
        "Other",
        Some("ops"),
    )
    .await;
    let c0 = fx
        .authority
        .db()
        .read_current_commit(&w, &sa)
        .await
        .unwrap()
        .unwrap();
    // Turn the workspace review-required ON (A now downgrades a member's direct publish).
    fx.authority.set_review_required(&w, true).await.unwrap();
    assert_eq!(
        channel_places(&pool, "w_acme", "ops", "s_deploy").await,
        0,
        "A not in ops yet"
    );

    let r = fx
        .authority
        .publish(
            &w,
            &sa,
            &op("aaaaaaaa-0000-4000-8000-000000000002"),
            child(c0, vec![file("f", b"v1")]),
            DeviceOpAuth {
                credential: cred(&w, "dk_mem"),
                op: DeviceOp::PublishDirect,
                expected: g.current.unwrap(),
            },
            None,
            Some("ops"),
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();

    // The version DOWNGRADED (NEEDS_REVIEW, pointer frozen)…
    assert_eq!(r.outcome, TerminalOutcome::NeedsReview);
    assert_eq!(
        r.details
            .as_ref()
            .and_then(|d| d.get("downgraded"))
            .and_then(serde_json::Value::as_bool),
        Some(true)
    );
    assert_eq!(
        fx.authority
            .db()
            .read_current_commit(&w, &sa)
            .await
            .unwrap(),
        Some(c0),
        "the pointer is frozen at the pre-publish version"
    );
    // …yet the placement STILL applied — the receipt names it, and the channel_skills row exists.
    assert_eq!(
        detail_str(&r, "placed_channel").as_deref(),
        Some("ops"),
        "the placement rides the receipt independently of the version gate"
    );
    assert_eq!(
        channel_places(&pool, "w_acme", "ops", "s_deploy").await,
        1,
        "the channel reference is written even though the new version never landed"
    );
}
