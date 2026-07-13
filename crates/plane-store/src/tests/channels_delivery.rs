//! Channels — the delivery predicate matrix + the curation / membership / subscription gates.
//!
//! These drive the channel-era directory ops directly through `Authority` against a real Postgres +
//! git store (no HTTP): a genesis publish lands a skill in `everyone`, and `delivery(ws, credential)`
//! answers "what should THIS device have" through the ONE entitlement predicate (roster-derived
//! `everyone` ∪ followed channels ∪ direct follows − unfollows − this device's exclusions). Every
//! scenario is load-bearing because the delivery read is the currency hot path the session-start hook
//! fires once per workspace — a wrong predicate silently ships (or withholds) bytes.

use super::*;

use crate::channels::{
    ChannelMembershipOutcome, CurationOutcome, ProtectKind, ProtectLevel, ProtectOutcome,
    SubscriptionOutcome,
};
use crate::delivery::{AppliedSkill, DeliveredSkill, Delivery};
use crate::governance::{GovernanceOp, GovernanceOutcome, GovernanceRequest};

// ── local seeding + driving helpers ────────────────────────────────────────────────────────────────

/// Seat a person's device (holding its `(ws, dkid)` credential) + their workspace_member seat at
/// `role`/`status` — the whole authorization a device presents on every lane.
async fn seat(
    fx: &Fixture,
    w: &WorkspaceId,
    dkid: &str,
    seed: u8,
    principal: &str,
    role: &str,
    status: &str,
) {
    let p = prin(principal);
    fx.authority
        .db()
        .seed_device(w, dkid, &dev_key(seed), &p, false, &cred(w, dkid))
        .await
        .unwrap();
    fx.authority
        .db()
        .seed_workspace_member(w, &p, role, status)
        .await
        .unwrap();
}

/// Genesis-publish `skill` as the device `dkid` presents it, minting the catalog name from
/// `display_name` and placing it into `channel` (else `everyone`). Returns the durable receipt.
#[allow(clippy::too_many_arguments)]
async fn gpub(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &BundleId,
    dkid: &str,
    op_id: &str,
    files: Vec<UploadedFile>,
    display_name: Option<&str>,
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
            display_name,
            channel,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap()
}

/// The delivered skill for `skill_id`, if the device is entitled to it.
fn find<'a>(d: &'a Delivery, skill_id: &str) -> Option<&'a DeliveredSkill> {
    d.skills.iter().find(|s| s.skill_id == skill_id)
}

/// A one-parent child publish (review off ⇒ lands) — the real pointer-move, since the `test-fixtures`
/// `seed_published_child` shim is not compiled in the in-crate test build.
async fn child_pub(
    fx: &Fixture,
    w: &WorkspaceId,
    s: &BundleId,
    dkid: &str,
    op_id: &str,
    parent: CommitId,
    files: Vec<UploadedFile>,
) -> crate::SetCurrentReceipt {
    let expected = fx
        .authority
        .db()
        .read_current_generation(w, s)
        .await
        .unwrap()
        .unwrap();
    let auth = DeviceOpAuth {
        credential: cred(w, dkid),
        op: DeviceOp::PublishDirect,
        expected,
    };
    fx.authority
        .publish(
            w,
            s,
            &op(op_id),
            child(parent, files),
            auth,
            None,
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap()
}

/// A device's `device_skill_state` row `(detached, detached_at)` for one skill (raw `sqlx`, off the
/// committed `.sqlx` drift surface).
async fn dss(pool: &PgPool, w: &str, dkid: &str, skill_id: &str) -> Option<(i64, Option<i64>)> {
    use sqlx::Row as _;
    sqlx::query(
        "SELECT detached, detached_at FROM device_skill_state \
         WHERE workspace_id = $1 AND device_key_id = $2 AND skill_id = $3",
    )
    .bind(w)
    .bind(dkid)
    .bind(skill_id)
    .fetch_optional(pool)
    .await
    .unwrap()
    .map(|r| {
        (
            r.get::<i64, _>("detached"),
            r.get::<Option<i64>, _>("detached_at"),
        )
    })
}

/// The `channel_events` audit rows `(event, actor, skill_id, principal)` for one channel, in order.
async fn events(
    pool: &PgPool,
    w: &str,
    channel_id: &str,
) -> Vec<(String, String, Option<String>, Option<String>)> {
    use sqlx::Row as _;
    sqlx::query(
        "SELECT event, actor, skill_id, principal FROM channel_events \
         WHERE workspace_id = $1 AND channel_id = $2 ORDER BY id",
    )
    .bind(w)
    .bind(channel_id)
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|r| {
        (
            r.get::<String, _>("event"),
            r.get::<String, _>("actor"),
            r.get::<Option<String>, _>("skill_id"),
            r.get::<Option<String>, _>("principal"),
        )
    })
    .collect()
}

/// Whether a device_registry row (and its stored credential hash) survives — for the member-removal
/// "the device + credential outlive the seat" witness.
async fn device_has_credential(pool: &PgPool, w: &str, dkid: &str) -> bool {
    use sqlx::Row as _;
    sqlx::query(
        "SELECT credential_sha256 IS NOT NULL AS present FROM device_registry \
         WHERE workspace_id = $1 AND device_key_id = $2",
    )
    .bind(w)
    .bind(dkid)
    .fetch_optional(pool)
    .await
    .unwrap()
    .map(|r| r.get::<bool, _>("present"))
    .unwrap_or(false)
}

const ALICE: &str = "alice@acme.com";
const BOB: &str = "bob@acme.com";

// ── the predicate matrix ─────────────────────────────────────────────────────────────────────────

/// `everyone` delivers a genesis publish to every confirmed member: bob (a member with NO follows)
/// receives alice's skill via the structural `everyone` channel — the base case the whole currency
/// story rests on (membership alone entitles the builtin channel's contents).
#[sqlx::test]
async fn everyone_delivers_a_genesis_publish_to_every_member(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-everyone").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;

    let r = gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    assert!(r.is_ok());

    let d = fx
        .authority
        .delivery(&w, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    let ds = find(&d, "s_deploy").expect("bob is delivered the everyone skill");
    assert_eq!(ds.name, "deploy", "the catalog name folds the display name");
    assert_eq!(ds.via_channels, vec!["everyone".to_owned()]);
    assert!(
        !ds.direct,
        "bob receives it via the channel, not a direct follow"
    );
    assert_eq!(ds.protection, "open", "no review-required default ⇒ open");
    assert_eq!(ds.generation, gn(1, 1));
    // The delivery carries the consent facts the follower re-verifies against.
    assert_eq!(ds.version_id, r.version_id.unwrap().0);
    assert_eq!(ds.bundle_digest, r.bundle_digest.unwrap());
}

/// A non-member, an INVITED-but-unconfirmed member, a REVOKED device, and an UNKNOWN credential all
/// read the SAME indistinguishable `NotFound` — the delivery front door leaks nothing about who is or
/// is not entitled (the uniform-miss discipline the whole read surface holds).
#[sqlx::test]
async fn non_member_invited_revoked_and_unknown_credential_all_read_notfound(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-uniform-miss").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;

    // A registered device whose principal has NO confirmed seat.
    fx.authority
        .db()
        .seed_device(
            &w,
            "dk_stranger",
            &dev_key(20),
            &prin("stranger@acme.com"),
            false,
            &cred(&w, "dk_stranger"),
        )
        .await
        .unwrap();
    // An INVITED (unconfirmed) member's device.
    seat(
        &fx,
        &w,
        "dk_invited",
        21,
        "invited@acme.com",
        "member",
        "invited",
    )
    .await;
    // A REVOKED device bound to a confirmed member.
    seat(&fx, &w, "dk_bob", 22, BOB, "member", "confirmed").await;
    fx.authority.db().revoke_device(&w, "dk_bob").await.unwrap();

    for (label, credential) in [
        ("non-member", cred(&w, "dk_stranger")),
        ("invited", cred(&w, "dk_invited")),
        ("revoked", cred(&w, "dk_bob")),
        ("unknown", cred(&w, "dk_ghost")),
    ] {
        let r = fx.authority.delivery(&w, &credential).await;
        assert!(
            matches!(r, Err(AuthorityError::NotFound)),
            "{label} must be the uniform NotFound, got {r:?}"
        );
    }
}

/// Channel scoping + reference counting: a skill moved OUT of `everyone` into `ops` reaches bob only
/// after he joins `ops`; leaving drops it; and a skill referenced by TWO sources he still receives
/// stays delivered when one source drops (the DISTINCT union is reference-counted, not last-writer).
#[sqlx::test]
async fn channel_scoping_join_leave_and_reference_counting(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-scoping").await;
    let (w, sb) = (ws("w_acme"), skill("s_beacon"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;

    // B is born in `everyone` (bob receives it), then moved into `ops` only.
    gpub(
        &fx,
        &w,
        &sb,
        "dk_alice",
        "bbbbbbbb-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"beacon")],
        Some("Beacon"),
        None,
    )
    .await;
    assert!(
        find(&delivery(&fx, &w).await, "s_beacon").is_some(),
        "born in everyone, bob receives it"
    );
    let alice = cred(&w, "dk_alice");
    assert_eq!(
        fx.authority
            .channel_place(&w, &alice, "ops", sb.as_str(), CREATED_AT)
            .await
            .unwrap(),
        CurationOutcome::Created,
        "ops is created on first placement"
    );
    assert_eq!(
        fx.authority
            .channel_unplace(&w, &alice, "everyone", sb.as_str(), CREATED_AT)
            .await
            .unwrap(),
        CurationOutcome::Removed
    );
    // B now lives in `ops` only; bob (not a member of ops) is NOT delivered it.
    assert!(
        find(&delivery(&fx, &w).await, "s_beacon").is_none(),
        "moved out of everyone, not yet in ops ⇒ withheld"
    );

    // bob joins ops → delivered via ["ops"].
    let bob = cred(&w, "dk_bob");
    assert_eq!(
        fx.authority
            .channel_join(&w, &bob, "ops", CREATED_AT)
            .await
            .unwrap(),
        ChannelMembershipOutcome::Joined
    );
    let d = delivery(&fx, &w).await;
    assert_eq!(
        find(&d, "s_beacon").unwrap().via_channels,
        vec!["ops".to_owned()]
    );

    // Reference counting: re-place B in `everyone` too. Leaving ops now leaves it delivered (everyone
    // still references it).
    fx.authority
        .channel_place(&w, &alice, "everyone", sb.as_str(), CREATED_AT)
        .await
        .unwrap();
    assert_eq!(
        fx.authority
            .channel_leave(&w, &bob, "ops", NOW, CREATED_AT)
            .await
            .unwrap(),
        ChannelMembershipOutcome::Left
    );
    assert_eq!(
        find(&delivery(&fx, &w).await, "s_beacon")
            .unwrap()
            .via_channels,
        vec!["everyone".to_owned()],
        "still delivered via everyone after leaving ops (reference counted)"
    );
    // And with everyone ALSO unplaced, leaving really drops it.
    fx.authority
        .channel_unplace(&w, &alice, "everyone", sb.as_str(), CREATED_AT)
        .await
        .unwrap();
    assert!(
        find(&delivery(&fx, &w).await, "s_beacon").is_none(),
        "no remaining source ⇒ withdrawn"
    );
}

/// A skill referenced by TWO channels the person is in collapses to ONE delivered row whose
/// `via_channels` is the sorted set — the DISTINCT union attributes every delivering source without
/// double-delivering the bytes.
#[sqlx::test]
async fn two_channels_deliver_one_row_with_sorted_via(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-two-channels").await;
    let (w, sb) = (ws("w_acme"), skill("s_beacon"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &sb,
        "dk_alice",
        "bbbbbbbb-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"beacon")],
        Some("Beacon"),
        None,
    )
    .await;
    let alice = cred(&w, "dk_alice");
    // B in ops + eng; everyone dropped so ONLY the two named channels deliver it.
    fx.authority
        .channel_place(&w, &alice, "ops", sb.as_str(), CREATED_AT)
        .await
        .unwrap();
    fx.authority
        .channel_place(&w, &alice, "eng", sb.as_str(), CREATED_AT)
        .await
        .unwrap();
    fx.authority
        .channel_unplace(&w, &alice, "everyone", sb.as_str(), CREATED_AT)
        .await
        .unwrap();
    let bob = cred(&w, "dk_bob");
    fx.authority
        .channel_join(&w, &bob, "ops", CREATED_AT)
        .await
        .unwrap();
    fx.authority
        .channel_join(&w, &bob, "eng", CREATED_AT)
        .await
        .unwrap();

    let d = delivery(&fx, &w).await;
    let beacons: Vec<_> = d
        .skills
        .iter()
        .filter(|s| s.skill_id == "s_beacon")
        .collect();
    assert_eq!(beacons.len(), 1, "one skill, one row (deduped)");
    assert_eq!(
        beacons[0].via_channels,
        vec!["eng".to_owned(), "ops".to_owned()],
        "via names are the sorted set"
    );
}

/// A DIRECT follow survives every channel dropping the skill: unplacing it from all channels leaves
/// it delivered with `direct = true` and no `via` attribution — a person's own subscription is
/// independent of curation.
#[sqlx::test]
async fn a_direct_follow_survives_dropping_from_every_channel(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-direct").await;
    let (w, sb) = (ws("w_acme"), skill("s_beacon"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &sb,
        "dk_alice",
        "bbbbbbbb-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"beacon")],
        Some("Beacon"),
        None,
    )
    .await;

    let bob = cred(&w, "dk_bob");
    assert_eq!(
        fx.authority
            .follow_skill(&w, &bob, sb.as_str(), CREATED_AT)
            .await
            .unwrap(),
        SubscriptionOutcome::Followed
    );
    // Drop B from every channel it sits in.
    fx.authority
        .channel_unplace(
            &w,
            &cred(&w, "dk_alice"),
            "everyone",
            sb.as_str(),
            CREATED_AT,
        )
        .await
        .unwrap();

    let d = delivery(&fx, &w).await;
    let ds = find(&d, "s_beacon").expect("the direct follow keeps it delivered");
    assert!(ds.direct, "delivered as a direct follow");
    assert!(ds.via_channels.is_empty(), "no channel attribution");
}

/// Unfollow is the standing negative mask: it withholds the skill from ALL the person's devices,
/// records it in `detached[]`, and flips the fleet row `detached = 1`; a later `follow` re-attaches
/// (delivered again, `detached[]` empty, the row live). The who-acts principle the client's freeze
/// vs. clean decision reads.
#[sqlx::test]
async fn unfollow_masks_everything_and_follow_reattaches(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chd-unfollow").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    let r = gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let bob = cred(&w, "dk_bob");
    // Seed a fleet row via the real report path (the API-produced pre-state).
    fx.authority
        .report_applied(
            &w,
            &bob,
            &[AppliedSkill {
                skill_id: s.clone(),
                version_id: r.version_id.unwrap(),
            }],
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_deploy").await,
        Some((0, None))
    );

    // Unfollow → withheld everywhere, listed detached, the fleet row frozen.
    assert_eq!(
        fx.authority
            .unfollow_skill(&w, &bob, s.as_str(), NOW, CREATED_AT)
            .await
            .unwrap(),
        SubscriptionOutcome::Unfollowed
    );
    let d = delivery(&fx, &w).await;
    assert!(find(&d, "s_deploy").is_none(), "unfollow withholds it");
    assert!(
        d.detached.contains(&"s_deploy".to_owned()),
        "listed in detached"
    );
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_deploy").await,
        Some((1, Some(NOW))),
        "the fleet row is the final detach record"
    );

    // Follow re-attaches: delivered again, detached empty, the row live.
    assert_eq!(
        fx.authority
            .follow_skill(&w, &bob, s.as_str(), CREATED_AT)
            .await
            .unwrap(),
        SubscriptionOutcome::Followed
    );
    let d = delivery(&fx, &w).await;
    assert!(
        find(&d, "s_deploy").is_some(),
        "re-attached, delivered again"
    );
    assert!(d.detached.is_empty(), "detached cleared");
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_deploy").await,
        Some((0, None))
    );
}

/// A device exclusion is DEVICE-scoped: `exclude_device` withholds the skill from THAT device only
/// (another device of the same person keeps receiving), and it is NOT a person-level detach; a
/// `follow` on the excluded device lifts it. The "not on this device" the `remove` verb writes.
#[sqlx::test]
async fn a_device_exclusion_is_device_scoped_and_follow_lifts_it(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chd-exclusion").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    // bob has TWO devices, both his own principal.
    seat(&fx, &w, "dk_bob1", 12, BOB, "member", "confirmed").await;
    fx.authority
        .db()
        .seed_device(
            &w,
            "dk_bob2",
            &dev_key(13),
            &prin(BOB),
            false,
            &cred(&w, "dk_bob2"),
        )
        .await
        .unwrap();
    let r = gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let bob1 = cred(&w, "dk_bob1");
    fx.authority
        .report_applied(
            &w,
            &bob1,
            &[AppliedSkill {
                skill_id: s.clone(),
                version_id: r.version_id.unwrap(),
            }],
            NOW,
        )
        .await
        .unwrap();

    // Exclude from bob1 only.
    assert_eq!(
        fx.authority
            .exclude_device(&w, &bob1, s.as_str(), CREATED_AT)
            .await
            .unwrap(),
        SubscriptionOutcome::Excluded
    );
    let d1 = fx.authority.delivery(&w, &bob1).await.unwrap();
    assert!(find(&d1, "s_deploy").is_none(), "excluded on this device");
    assert!(
        !d1.detached.contains(&"s_deploy".to_owned()),
        "an exclusion is device-scoped, never a person-level detach"
    );
    let d2 = fx
        .authority
        .delivery(&w, &cred(&w, "dk_bob2"))
        .await
        .unwrap();
    assert!(
        find(&d2, "s_deploy").is_some(),
        "the other device still receives it"
    );

    // follow on the excluded device lifts the exclusion.
    fx.authority
        .follow_skill(&w, &bob1, s.as_str(), CREATED_AT)
        .await
        .unwrap();
    let d1 = fx.authority.delivery(&w, &bob1).await.unwrap();
    assert!(
        find(&d1, "s_deploy").is_some(),
        "follow lifts the exclusion"
    );
}

/// A skill with a catalog row but no `current` pointer is never delivered — the entitlement read
/// joins `current`, so a placed-but-unpublished skill silently produces nothing (the delivery is
/// the pointer, not the name).
#[sqlx::test]
async fn a_current_less_skill_is_never_delivered(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-currentless").await;
    let (w, s) = (ws("w_acme"), skill("s_draftonly"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    // A real genesis first, so `everyone` exists as the structural builtin channel.
    gpub(
        &fx,
        &w,
        &skill("s_real"),
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"real")],
        Some("Real"),
        None,
    )
    .await;
    // A catalog entry with NO current pointer, placed into the (now-existing) builtin everyone.
    fx.authority
        .db()
        .seed_catalog(&w, &s, "draftonly")
        .await
        .unwrap();
    assert_eq!(
        fx.authority
            .channel_place(
                &w,
                &cred(&w, "dk_alice"),
                "everyone",
                s.as_str(),
                CREATED_AT
            )
            .await
            .unwrap(),
        CurationOutcome::Placed
    );
    let d = delivery(&fx, &w).await;
    assert!(find(&d, "s_real").is_some(), "the published skill delivers");
    assert!(
        find(&d, "s_draftonly").is_none(),
        "a current-less skill is skipped by the delivery join"
    );
}

/// An archived skill leaves delivery (the status join excludes it), and a skill a person had
/// unfollowed BEFORE it was archived STAYS in `detached[]` — the fleet's blind-spot list keeps
/// naming what the person turned off, even after curation retires the identity.
#[sqlx::test]
async fn an_archived_skill_leaves_delivery_and_an_unfollowed_archived_stays_detached(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-archived").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    // The archiver must be an owner (a web-surface class act); bob is the observing member.
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let bob = cred(&w, "dk_bob");
    // bob unfollows it FIRST (a person-level detach), then the owner archives it.
    fx.authority
        .unfollow_skill(&w, &bob, s.as_str(), NOW, CREATED_AT)
        .await
        .unwrap();
    assert!(
        matches!(
            fx.authority
                .archive_skill_session(
                    &w,
                    ALICE,
                    "s_deploy",
                    DeploymentMode::Cloud,
                    CREATED_AT,
                    NOW
                )
                .await
                .unwrap(),
            crate::LifecycleOutcome::Archived { .. }
        ),
        "the owner archives it"
    );
    let d = delivery(&fx, &w).await;
    assert!(find(&d, "s_deploy").is_none(), "archived ⇒ out of delivery");
    assert!(
        d.detached.contains(&"s_deploy".to_owned()),
        "an unfollowed-then-archived skill stays named in detached"
    );
}

/// Member removal (device-lane, owner-driven `roster_remove`) severs the target's whole workspace:
/// their delivery reads `NotFound`, every fleet row gets its final detach record at removal time,
/// yet the device registry row + its credential SURVIVE (revocation is a seat delete, not a device
/// wipe) — and re-seating the member re-enables delivery. The credential-model "access dies with the
/// seat, the device outlives it" contract.
#[sqlx::test]
async fn member_removal_detaches_the_fleet_and_re_seating_re_enables(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chd-removal").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    // A confirmed owner (the acting device) + the target member bob.
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    let r = gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let bob = cred(&w, "dk_bob");
    fx.authority
        .report_applied(
            &w,
            &bob,
            &[AppliedSkill {
                skill_id: s.clone(),
                version_id: r.version_id.unwrap(),
            }],
            NOW,
        )
        .await
        .unwrap();
    assert!(
        find(&fx.authority.delivery(&w, &bob).await.unwrap(), "s_deploy").is_some(),
        "bob receives it while seated"
    );

    // The owner removes bob (device lane).
    let out = fx
        .authority
        .roster_remove(
            &w,
            "cccccccc-0000-4000-8000-000000000001",
            GovernanceRequest {
                credential: cred(&w, "dk_owner"),
                op: GovernanceOp::RosterRemove { target: prin(BOB) },
            },
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(out, GovernanceOutcome::Ok);

    // Delivery is NotFound; the fleet row is the final detach record; the device + credential survive.
    assert!(matches!(
        fx.authority.delivery(&w, &bob).await,
        Err(AuthorityError::NotFound)
    ));
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_deploy").await,
        Some((1, Some(NOW))),
        "removal writes the final detach record"
    );
    assert!(
        device_has_credential(&pool, "w_acme", "dk_bob").await,
        "the device row + credential outlive the seat"
    );

    // Re-seat bob → delivery works again (re-adding re-enables the same device).
    fx.authority
        .db()
        .seed_workspace_member(&w, &prin(BOB), "member", "confirmed")
        .await
        .unwrap();
    assert!(
        find(&fx.authority.delivery(&w, &bob).await.unwrap(), "s_deploy").is_some(),
        "re-seating re-enables delivery"
    );
}

// ── curation / membership structural gates ─────────────────────────────────────────────────────────

/// The `everyone` channel is STRUCTURAL: it cannot be joined or left (its membership IS the roster),
/// its row cannot be deleted or renamed (the trigger guards refuse), yet its MODE can flip via
/// `protect` (an org may mark #everyone curated). Invariants held in the database, not by convention.
#[sqlx::test]
async fn the_everyone_channel_is_structural(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chd-structural").await;
    let w = ws("w_acme");
    // A genesis creates the `everyone` channel; an owner can flip its mode.
    let s = skill("s_deploy");
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner", "confirmed").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let owner = cred(&w, "dk_owner");

    // Join / leave everyone ⇒ Builtin (structural refusal, not an error).
    assert_eq!(
        fx.authority
            .channel_join(&w, &owner, "everyone", CREATED_AT)
            .await
            .unwrap(),
        ChannelMembershipOutcome::Builtin
    );
    assert_eq!(
        fx.authority
            .channel_leave(&w, &owner, "everyone", NOW, CREATED_AT)
            .await
            .unwrap(),
        ChannelMembershipOutcome::Builtin
    );

    // A raw DELETE / rename of the builtin row is refused by the trigger guards.
    let del =
        sqlx::query("DELETE FROM channels WHERE workspace_id = $1 AND channel_id = 'everyone'")
            .bind("w_acme")
            .execute(&pool)
            .await;
    assert!(del.is_err(), "the everyone row cannot be deleted");
    let ren = sqlx::query(
        "UPDATE channels SET name = 'all' WHERE workspace_id = $1 AND channel_id = 'everyone'",
    )
    .bind("w_acme")
    .execute(&pool)
    .await;
    assert!(ren.is_err(), "the everyone row cannot be renamed");

    // The MODE can flip (an owner marks #everyone curated).
    assert_eq!(
        fx.authority
            .protect(
                &w,
                &owner,
                ProtectKind::Channel,
                "everyone",
                ProtectLevel::Protected,
                CREATED_AT,
            )
            .await
            .unwrap(),
        ProtectOutcome::Set
    );
    use sqlx::Row as _;
    let mode: String = sqlx::query(
        "SELECT mode FROM channels WHERE workspace_id = $1 AND channel_id = 'everyone'",
    )
    .bind("w_acme")
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("mode");
    assert_eq!(mode, "curated", "the builtin channel's mode is mutable");
}

/// A CURATED channel gates curation by role: a plain member's `channel_place` is refused
/// (`CuratedRoleRequired`) while a reviewer's lands; loosening the channel back to `open` is an
/// OWNER act (a reviewer's loosen is `OwnerRoleRequired`). Tightening protects, loosening widens —
/// asymmetric by design.
#[sqlx::test]
async fn a_curated_channel_gates_curation_and_loosening_by_role(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-curated").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_owner", 11, ALICE, "owner", "confirmed").await;
    seat(
        &fx,
        &w,
        "dk_rev",
        12,
        "rev@acme.com",
        "reviewer",
        "confirmed",
    )
    .await;
    seat(&fx, &w, "dk_mem", 13, BOB, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_owner",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        Some("ops"),
    )
    .await;
    let rev = cred(&w, "dk_rev");

    // A reviewer marks ops curated.
    assert_eq!(
        fx.authority
            .protect(
                &w,
                &rev,
                ProtectKind::Channel,
                "ops",
                ProtectLevel::Protected,
                CREATED_AT
            )
            .await
            .unwrap(),
        ProtectOutcome::Set
    );
    // A plain member's placement is refused; a reviewer's lands.
    assert_eq!(
        fx.authority
            .channel_place(&w, &cred(&w, "dk_mem"), "ops", s.as_str(), CREATED_AT)
            .await
            .unwrap(),
        CurationOutcome::CuratedRoleRequired
    );
    assert_eq!(
        fx.authority
            .channel_place(&w, &rev, "ops", s.as_str(), CREATED_AT)
            .await
            .unwrap(),
        CurationOutcome::Placed
    );
    // Loosening ops back to open is an OWNER act — a reviewer is refused, the owner lands.
    assert_eq!(
        fx.authority
            .protect(
                &w,
                &rev,
                ProtectKind::Channel,
                "ops",
                ProtectLevel::Open,
                CREATED_AT
            )
            .await
            .unwrap(),
        ProtectOutcome::OwnerRoleRequired
    );
    assert_eq!(
        fx.authority
            .protect(
                &w,
                &cred(&w, "dk_owner"),
                ProtectKind::Channel,
                "ops",
                ProtectLevel::Open,
                CREATED_AT
            )
            .await
            .unwrap(),
        ProtectOutcome::Set
    );
}

/// Curation creates the channel on FIRST use (member-level self-serve), and a charset-violating
/// channel name is a typed `BadName` — the one place a member conjures a new channel, guarded only
/// by the id charset.
#[sqlx::test]
async fn create_on_first_use_and_a_bad_name_is_typed(pool: PgPool) {
    let fx = Fixture::new(pool, "chd-create").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_mem", 11, BOB, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_mem",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let mem = cred(&w, "dk_mem");
    assert_eq!(
        fx.authority
            .channel_place(&w, &mem, "new-ch", s.as_str(), CREATED_AT)
            .await
            .unwrap(),
        CurationOutcome::Created,
        "a member self-serve-creates the channel on first placement"
    );
    assert_eq!(
        fx.authority
            .channel_place(&w, &mem, "Bad_Name", s.as_str(), CREATED_AT)
            .await
            .unwrap(),
        CurationOutcome::BadName,
        "a charset violation is a typed refusal"
    );
}

/// The applied-state report is a SNAPSHOT upsert: a second report naming a subset DELETES the rows
/// it no longer names, UPDATES the ones it keeps, and refreshes `last_report_at` — but NEVER touches
/// a detached row (the frozen "last known state" the fleet page shows).
#[sqlx::test]
async fn report_snapshot_deletes_unnamed_rows_and_never_touches_a_detached_row(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chd-report").await;
    let (w, sa, sb) = (ws("w_acme"), skill("s_alpha"), skill("s_beacon"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    let ra = gpub(
        &fx,
        &w,
        &sa,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"alpha")],
        Some("Alpha"),
        None,
    )
    .await;
    let rb = gpub(
        &fx,
        &w,
        &sb,
        "dk_alice",
        "bbbbbbbb-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"beacon")],
        Some("Beacon"),
        None,
    )
    .await;
    let bob = cred(&w, "dk_bob");
    let va = ra.version_id.unwrap();
    let vb = rb.version_id.unwrap();

    // Report {A@v1, B@v1}, then unfollow B (a detach record), then report {A@v2}.
    fx.authority
        .report_applied(
            &w,
            &bob,
            &[
                AppliedSkill {
                    skill_id: sa.clone(),
                    version_id: va,
                },
                AppliedSkill {
                    skill_id: sb.clone(),
                    version_id: vb,
                },
            ],
            NOW,
        )
        .await
        .unwrap();
    fx.authority
        .unfollow_skill(&w, &bob, sb.as_str(), NOW, CREATED_AT)
        .await
        .unwrap();
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_beacon").await,
        Some((1, Some(NOW))),
        "B's row is detached after the unfollow"
    );

    // A second child of A to report a moved version.
    let ca = fx
        .authority
        .db()
        .read_current_commit(&w, &sa)
        .await
        .unwrap()
        .unwrap();
    let ra2 = child_pub(
        &fx,
        &w,
        &sa,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000002",
        ca,
        vec![file("SKILL.md", b"alpha2")],
    )
    .await;
    let va2 = ra2.version_id.unwrap();
    let later = NOW + 5;
    fx.authority
        .report_applied(
            &w,
            &bob,
            &[AppliedSkill {
                skill_id: sa.clone(),
                version_id: va2,
            }],
            later,
        )
        .await
        .unwrap();

    use sqlx::Row as _;
    // A's row updated to v2 at the new report time; a non-detached row it stopped naming (there is
    // none here beyond A) would be deleted; B's DETACHED row is untouched (still v1, detached).
    let a_row = sqlx::query(
        "SELECT applied_commit, reported_at, detached FROM device_skill_state \
         WHERE workspace_id = $1 AND device_key_id = $2 AND skill_id = $3",
    )
    .bind("w_acme")
    .bind("dk_bob")
    .bind("s_alpha")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        a_row.get::<Vec<u8>, _>("applied_commit"),
        va2.0.to_vec(),
        "A updated to v2"
    );
    assert_eq!(
        a_row.get::<i64, _>("reported_at"),
        later,
        "last-writer-wins reported_at"
    );
    assert_eq!(a_row.get::<i64, _>("detached"), 0);
    // B stayed exactly the detach record the unfollow wrote — the snapshot never deletes/updates it.
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_beacon").await,
        Some((1, Some(NOW))),
        "the detached row is immutable under a report"
    );
    // The device's last_report_at is the staleness clock.
    let lr: Option<i64> = sqlx::query(
        "SELECT last_report_at FROM device_registry WHERE workspace_id=$1 AND device_key_id=$2",
    )
    .bind("w_acme")
    .bind("dk_bob")
    .fetch_one(&pool)
    .await
    .unwrap()
    .get("last_report_at");
    assert_eq!(lr, Some(later));

    // A LYING CLIENT changes nothing. The report is client-asserted data, so the plane re-checks
    // every named skill against its OWN entitlement predicate: re-reporting the DETACHED skill B
    // cannot revive the detach record the plane is deliberately holding (B is not entitled — that
    // is what "detached" means), and a skill the device was never entitled to cannot be conjured
    // into the fleet at all. Without the server-side filter a buggy or hostile client could erase
    // its own blind-spot row and fake fleet coverage.
    let sc = skill("s_ghost");
    fx.authority
        .db()
        .seed_catalog(&w, &sc, "ghost")
        .await
        .unwrap();
    fx.authority
        .report_applied(
            &w,
            &bob,
            &[
                AppliedSkill {
                    skill_id: sa.clone(),
                    version_id: va2,
                },
                // detached — the plane holds the freeze
                AppliedSkill {
                    skill_id: sb.clone(),
                    version_id: vb,
                },
                // never entitled — no channel delivers it, no follow names it
                AppliedSkill {
                    skill_id: sc.clone(),
                    version_id: va2,
                },
            ],
            later + 5,
        )
        .await
        .unwrap();
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_beacon").await,
        Some((1, Some(NOW))),
        "a client cannot revive its own detach record by re-reporting the skill"
    );
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_ghost").await,
        None,
        "a client cannot record a skill it was never entitled to"
    );
}

/// The channel audit log is TRIGGER-emitted on every curation / membership / existence write, each
/// row attributed to the acting principal the guarded function set — so no write path can skip the
/// audit, and the trail names WHO acted.
#[sqlx::test]
async fn channel_operations_emit_audit_rows_attributed_to_the_actor(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chd-audit").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_alice", 11, ALICE, "member", "confirmed").await;
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    gpub(
        &fx,
        &w,
        &s,
        "dk_alice",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let alice = cred(&w, "dk_alice");
    let bob = cred(&w, "dk_bob");
    // create + place (alice), join + leave (bob), then a removal (alice).
    fx.authority
        .channel_place(&w, &alice, "ops", s.as_str(), CREATED_AT)
        .await
        .unwrap();
    fx.authority
        .channel_join(&w, &bob, "ops", CREATED_AT)
        .await
        .unwrap();
    fx.authority
        .channel_leave(&w, &bob, "ops", NOW, CREATED_AT)
        .await
        .unwrap();
    fx.authority
        .channel_unplace(&w, &alice, "ops", s.as_str(), CREATED_AT)
        .await
        .unwrap();

    let rows = events(&pool, "w_acme", "ops").await;
    // channel_created (alice) — skill_added (alice, deploy) — member_joined (bob) — member_left (bob)
    // — skill_removed (alice, deploy).
    let kinds: Vec<&str> = rows.iter().map(|(e, ..)| e.as_str()).collect();
    assert_eq!(
        kinds,
        vec![
            "channel_created",
            "skill_added",
            "member_joined",
            "member_left",
            "skill_removed"
        ],
        "every curation/membership/existence write is audited in order"
    );
    // The actor is the acting principal, and the join/leave name bob as the principal.
    let created = &rows[0];
    assert_eq!(
        created.1, ALICE,
        "channel_created attributed to the creator"
    );
    let joined = rows.iter().find(|(e, ..)| e == "member_joined").unwrap();
    assert_eq!(joined.1, BOB, "member_joined actor is bob");
    assert_eq!(
        joined.3.as_deref(),
        Some(BOB),
        "…and names bob as the principal"
    );
    let added = rows.iter().find(|(e, ..)| e == "skill_added").unwrap();
    assert_eq!(added.1, ALICE);
    assert_eq!(
        added.2.as_deref(),
        Some("s_deploy"),
        "skill_added names the skill id"
    );
}

/// F1 REGRESSION (the door cutover's report/detach fence). An applied-state report concurrent with
/// the same person's unfollow-detach must NOT revive the frozen fleet row. Both callers are READ
/// COMMITTED now (the web tier and the autocommit Rust wrapper), where the old SERIALIZABLE report
/// used to catch this; the fix is that `topos_report_applied` FOR UPDATE-fences the device's
/// registry row AND `topos_detach_lapsed` FOR UPDATE-locks the person's registry rows, so the two
/// mutually exclude. The report that loses the race re-reads a post-commit snapshot — the skill no
/// longer entitled — and skips it, leaving `detached = 1`. Before the fence was made real the report
/// blocked on the `device_skill_state` row instead and revived it to `detached = 0`.
#[sqlx::test]
async fn a_report_racing_an_unfollow_detach_does_not_revive_the_frozen_row(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "chd-race").await;
    let (w, s) = (ws("w_acme"), skill("s_deploy"));
    seat(&fx, &w, "dk_bob", 12, BOB, "member", "confirmed").await;
    let r = gpub(
        &fx,
        &w,
        &s,
        "dk_bob",
        "aaaaaaaa-0000-4000-8000-000000000001",
        vec![file("SKILL.md", b"v0")],
        Some("Deploy"),
        None,
    )
    .await;
    let commit = r.version_id.unwrap();
    // Seed the fleet row through the real report path (detached = 0).
    fx.authority
        .report_applied(
            &w,
            &cred(&w, "dk_bob"),
            &[AppliedSkill {
                skill_id: s.clone(),
                version_id: commit,
            }],
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(
        dss(&pool, "w_acme", "dk_bob", "s_deploy").await,
        Some((0, None))
    );

    // Conn A: begin an unfollow and HOLD the transaction open — its detach reconcile takes the
    // person's registry rows FOR UPDATE and leaves the fleet row's `detached = 1` uncommitted.
    let mut a = pool.acquire().await.unwrap();
    sqlx::query("BEGIN").execute(&mut *a).await.unwrap();
    sqlx::query("SELECT topos_unfollow_skill($1, $2, $3, $4, $5)")
        .bind("w_acme")
        .bind(BOB)
        .bind("s_deploy")
        .bind(NOW + 1)
        .bind(CREATED_AT)
        .execute(&mut *a)
        .await
        .unwrap();

    // Conn B: a concurrent report must BLOCK on A's registry lock (its own FOR UPDATE fence).
    let pool_b = pool.clone();
    let commit_bytes = commit.0.to_vec();
    let b = tokio::spawn(async move {
        sqlx::query("SELECT topos_report_applied($1, $2, $3, $4, $5::text[], $6::bytea[])")
            .bind("w_acme")
            .bind(BOB)
            .bind("dk_bob")
            .bind(NOW + 2)
            .bind(vec!["s_deploy".to_owned()])
            .bind(vec![commit_bytes])
            .execute(&pool_b)
            .await
    });
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    assert!(
        !b.is_finished(),
        "the report must block on the detach's registry lock, not race past it"
    );

    // A commits; B unblocks and runs against the post-detach snapshot (s_deploy no longer entitled).
    sqlx::query("COMMIT").execute(&mut *a).await.unwrap();
    b.await.unwrap().unwrap();

    // The frozen row stays detached = 1 — never revived to 0.
    let (detached, _) = dss(&pool, "w_acme", "dk_bob", "s_deploy")
        .await
        .expect("the fleet row survives");
    assert_eq!(
        detached, 1,
        "the racing report must not revive the frozen fleet row"
    );
}

/// The confirmed-member's delivery, by that member's own credential (the common read the assertions
/// above share — bob unless a test says otherwise).
async fn delivery(fx: &Fixture, w: &WorkspaceId) -> Delivery {
    fx.authority.delivery(w, &cred(w, "dk_bob")).await.unwrap()
}
