//! Workspace standup — the two self-serve genesis doors + the hardened one-time claim.
//!
//! Door 1 (`standup`): an un-enrolled device starts a workspace-less session; a web-verified email's
//! approval CREATES the workspace, and the ordinary poll → redeem → genesis-publish chain follows. Door 2
//! (`create_workspace`): a verified email creates the workspace directly and gets a self-invite link.
//! The claim (`mint_admin_claim` → `admin_claim`) is the operator/bearer door. These tests are the
//! server-side release-blocker witnesses: one-owner-per-genesis, first-writer-wins approvals, the
//! consumed-replay lost-200 recovery, the per-owner cap, cross-door token separation, and the intent
//! guards that keep the enroll identity legs off standup sessions.

use super::enrollment_governance::{device_pub, make_invite, op_id, redeem, seat_owner};
use super::*;

use crate::enroll::device_key_id_for;
use crate::{
    ApproveStandupOutcome, CreateWorkspaceOutcome, DeviceAuthPoll, MintClaimOutcome, RedeemOutcome,
    SessionIntent,
};

const NOW: i64 = 1_000;
const T0: &str = "t0";

/// Mint a claim and return its plaintext token (panics on a denial).
async fn mint(
    a: &Authority,
    w: &WorkspaceId,
    display_name: Option<&str>,
    owner_email: Option<&str>,
    mode: DeploymentMode,
    ttl_ms: i64,
) -> String {
    match a
        .mint_admin_claim(w, display_name, owner_email, mode, ttl_ms, NOW, T0)
        .await
        .unwrap()
    {
        MintClaimOutcome::Minted(m) => m.token,
        MintClaimOutcome::Denied(reason) => panic!("mint denied: {reason}"),
    }
}

/// COUNT the confirmed owners of a workspace straight off the injected pool (the test's own eye).
async fn owner_rows(pool: &PgPool, ws: &str) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM workspace_member \
         WHERE workspace_id = $1 AND role = 'owner' AND status = 'confirmed'",
    )
    .bind(ws)
    .fetch_one(pool)
    .await
    .unwrap()
}

// ── the claim door: mint → redeem, replay, expiry, rebind, races ───────────────────────────────────────

#[sqlx::test]
async fn claim_mint_redeem_names_from_the_row_and_replays_only_same_device(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "st-claim").await; // a CLOUD plane
    let a = &fx.authority;
    let w = ws("w_break_glass");
    let token = mint(
        a,
        &w,
        Some("Real Name"),
        Some("owner@acme.com"),
        DeploymentMode::Cloud,
        60_000,
    )
    .await;

    let device_seed = [33u8; 32];
    let dpub = device_pub(&device_seed);
    let RedeemOutcome::Redeemed(r) = a.admin_claim(&token, dpub, NOW, T0).await.unwrap() else {
        panic!("claim redeem");
    };
    assert_eq!(r.workspace_id.as_str(), "w_break_glass");
    // The owner is the MINT-BOUND email, not a device-rooted id; the workspace name is the ROW's (the
    // adversarial wire display_name never reaches this op at all), and the mode is THE PLANE'S (cloud).
    assert_eq!(r.principal.as_str(), "owner@acme.com");
    let created = a.db().read_workspace(&w).await.unwrap().expect("workspace");
    assert_eq!(created.display_name, "Real Name");
    assert_eq!(created.deployment_mode, "cloud");
    assert_eq!(owner_rows(&pool, "w_break_glass").await, 1);

    // SAME-DEVICE consumed replay (lost-200 recovery) deterministically re-returns Redeemed…
    let RedeemOutcome::Redeemed(replay) = a.admin_claim(&token, dpub, NOW, T0).await.unwrap()
    else {
        panic!("same-device replay must be Redeemed");
    };
    assert_eq!(replay.principal.as_str(), "owner@acme.com");
    // …and a DIFFERENT device presenting the consumed token is denied; nothing was re-seated.
    let other = device_pub(&[44u8; 32]);
    assert!(matches!(
        a.admin_claim(&token, other, NOW, T0).await.unwrap(),
        RedeemOutcome::Denied(_)
    ));
    assert_eq!(owner_rows(&pool, "w_break_glass").await, 1);
}

#[sqlx::test]
async fn claim_mint_refusals_are_typed(pool: PgPool) {
    let fx = Fixture::new(pool, "st-mint-refuse").await;
    let a = &fx.authority;
    // An existing workspace can never be claimed.
    let w = ws("w_live");
    a.db()
        .seed_workspace(&w, "Live", "unverified", "cloud")
        .await
        .unwrap();
    assert!(matches!(
        a.mint_admin_claim(
            &w,
            None,
            Some("o@x.com"),
            DeploymentMode::Cloud,
            1_000,
            NOW,
            T0
        )
        .await
        .unwrap(),
        MintClaimOutcome::Denied("workspace already exists")
    ));
    // A cloud-mode mint REQUIRES an owner email.
    let w2 = ws("w_fresh");
    assert!(matches!(
        a.mint_admin_claim(&w2, None, None, DeploymentMode::Cloud, 1_000, NOW, T0)
            .await
            .unwrap(),
        MintClaimOutcome::Denied("a cloud-mode claim requires an owner email")
    ));
    // A self-host mint REFUSES an owner email (the symmetric refusal): a self-host owner is
    // device-rooted, and an email-seated principal would later collide with the same device's
    // self-host invites (which derive `dev.{kid}` and hit the device-rebind denial).
    assert!(matches!(
        a.mint_admin_claim(
            &w2,
            None,
            Some("o@x.com"),
            DeploymentMode::SelfHost,
            1_000,
            NOW,
            T0
        )
        .await
        .unwrap(),
        MintClaimOutcome::Denied(
            "a self-host claim's owner is device-rooted; omit the owner email"
        )
    ));
    // Self-host WITHOUT the email mints (the claiming device roots the owner).
    assert!(matches!(
        a.mint_admin_claim(&w2, None, None, DeploymentMode::SelfHost, 1_000, NOW, T0)
            .await
            .unwrap(),
        MintClaimOutcome::Minted(_)
    ));
}

#[sqlx::test]
async fn a_self_host_claim_seats_a_device_rooted_owner(pool: PgPool) {
    // A SELF-HOST plane: the claim redeem must seat the claiming device's own `dev.{kid}` principal —
    // never an email — so a later self-host invite for the same device (which derives the same
    // device-rooted principal) resolves to the SAME identity instead of a device-rebind denial.
    let fx = Fixture::with_mode(pool.clone(), "st-claim-devroot", DeploymentMode::SelfHost).await;
    let a = &fx.authority;
    let w = ws("w_home");
    let token = mint(a, &w, Some("Home"), None, DeploymentMode::SelfHost, 60_000).await;
    let dpub = device_pub(&[48u8; 32]);
    let RedeemOutcome::Redeemed(r) = a.admin_claim(&token, dpub, NOW, T0).await.unwrap() else {
        panic!("self-host claim redeem");
    };
    assert_eq!(
        r.principal.as_str(),
        format!("dev.{}", device_key_id_for(&dpub)),
        "the seated owner is the device-rooted principal"
    );
    let row = a.db().read_workspace(&w).await.unwrap().expect("workspace");
    assert_eq!(row.deployment_mode, "self_host");
    assert_eq!(owner_rows(&pool, "w_home").await, 1);
}

#[sqlx::test]
async fn claim_expiry_applies_only_to_the_first_consumption(pool: PgPool) {
    let fx = Fixture::new(pool, "st-claim-expiry").await;
    let a = &fx.authority;
    let dpub = device_pub(&[35u8; 32]);

    // Unknown token: the uniform denial.
    assert!(matches!(
        a.admin_claim("no-such-token", dpub, NOW, T0).await.unwrap(),
        RedeemOutcome::Denied(_)
    ));

    // An EXPIRED, never-consumed claim is denied.
    let w1 = ws("w_expired");
    let stale = mint(a, &w1, None, Some("o@x.com"), DeploymentMode::Cloud, 1_000).await;
    assert!(matches!(
        a.admin_claim(&stale, dpub, NOW + 2_000, T0).await.unwrap(),
        RedeemOutcome::Denied(_)
    ));
    assert!(a.db().read_workspace(&w1).await.unwrap().is_none());

    // But a CONSUMED claim's same-device replay still answers AFTER the TTL (the probe runs first).
    let w2 = ws("w_consumed");
    let live = mint(a, &w2, None, Some("o@x.com"), DeploymentMode::Cloud, 1_000).await;
    assert!(matches!(
        a.admin_claim(&live, dpub, NOW, T0).await.unwrap(),
        RedeemOutcome::Redeemed(_)
    ));
    assert!(matches!(
        a.admin_claim(&live, dpub, NOW + 60_000, T0).await.unwrap(),
        RedeemOutcome::Redeemed(_)
    ));
}

#[sqlx::test]
async fn claim_redeem_refuses_a_rebound_device_key(pool: PgPool) {
    let fx = Fixture::new(pool, "st-claim-rebind").await;
    let a = &fx.authority;
    let w = ws("w_squat");
    let token = mint(a, &w, None, Some("o@x.com"), DeploymentMode::Cloud, 60_000).await;

    // The claiming device's SERVER-DERIVED key id is already bound — to a different key + principal.
    let device_seed = [36u8; 32];
    let dpub = device_pub(&device_seed);
    let dkid = device_key_id_for(&dpub);
    a.db()
        .seed_device(
            &w,
            &dkid,
            &[9u8; 32],
            &prin("squatter@x.com"),
            false,
            &cred(&w, &dkid),
        )
        .await
        .unwrap();
    assert!(matches!(
        a.admin_claim(&token, dpub, NOW, T0).await.unwrap(),
        RedeemOutcome::Denied(_)
    ));
    // Nothing was seated and the claim was NOT consumed (the checks run before any write)…
    assert!(a.db().read_workspace(&w).await.unwrap().is_none());
}

#[sqlx::test]
async fn racing_double_redeem_seats_exactly_one_owner(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "st-claim-race").await;
    let a = &fx.authority;
    let w = ws("w_raced");
    let token = mint(a, &w, None, Some("o@x.com"), DeploymentMode::Cloud, 60_000).await;
    let dpub = device_pub(&[37u8; 32]);

    // Two concurrent redeems of the SAME claim by the SAME device (a double-sent lost-200 retry): the
    // SERIALIZABLE runner serializes them; the loser converges through the consumed-replay probe.
    let (r1, r2) = tokio::join!(
        a.admin_claim(&token, dpub, NOW, T0),
        a.admin_claim(&token, dpub, NOW, T0),
    );
    for out in [r1.unwrap(), r2.unwrap()] {
        assert!(matches!(out, RedeemOutcome::Redeemed(_)), "got {out:?}");
    }
    assert_eq!(
        owner_rows(&pool, "w_raced").await,
        1,
        "exactly one owner row"
    );
}

// ── door 2: create_workspace (idempotency, cap, self-invite, domain claim) ─────────────────────────────

#[sqlx::test]
async fn create_workspace_seats_workspace_owner_and_self_invite_in_one_txn(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "st-create").await;
    let a = &fx.authority;
    let out = a
        .create_workspace(
            "req-1",
            Some("Acme"),
            "robert@acme.com",
            DeploymentMode::Cloud,
            T0,
        )
        .await
        .unwrap();
    let CreateWorkspaceOutcome::Created(c) = out else {
        panic!("expected Created, got {out:?}");
    };
    assert!(c.workspace_id.as_str().starts_with("w_"));
    assert_eq!(c.display_name, "Acme");

    // The workspace row: the plane's mode + the freemail-aware domain claim (a corporate domain the owner
    // proved an address on ⇒ recorded verified).
    let row = a
        .db()
        .read_workspace(&c.workspace_id)
        .await
        .unwrap()
        .expect("workspace row");
    assert_eq!(row.deployment_mode, "cloud");
    assert_eq!(row.verified_domain.as_deref(), Some("acme.com"));
    assert_eq!(row.verified_domain_status, "verified");
    assert_eq!(owner_rows(&pool, c.workspace_id.as_str()).await, 1);

    // The self-invite resolves as a REAL invite bootstrap (owner email seeded; no skills), so the printed
    // `topos follow <link>` works — and the owner row it UPSERTs never demoted the confirmed owner.
    let boot = a.read_invite_bootstrap(&c.invite_token, NOW).await.unwrap();
    assert_eq!(boot.workspace_id.as_str(), c.workspace_id.as_str());
    assert!(boot.skills.is_empty());
    assert_eq!(owner_rows(&pool, c.workspace_id.as_str()).await, 1);

    // A freemail owner gets NO domain claim + the server-side default name.
    let CreateWorkspaceOutcome::Created(f) = a
        .create_workspace("req-2", None, "bob@gmail.com", DeploymentMode::Cloud, T0)
        .await
        .unwrap()
    else {
        panic!("freemail create");
    };
    assert_eq!(f.display_name, "bob's workspace");
    let row = a
        .db()
        .read_workspace(&f.workspace_id)
        .await
        .unwrap()
        .expect("workspace row");
    assert!(row.verified_domain.is_none());
    assert_eq!(row.verified_domain_status, "unverified");
}

#[sqlx::test]
async fn create_workspace_replays_for_the_same_owner_and_denies_another(pool: PgPool) {
    let fx = Fixture::new(pool, "st-genesis-idem").await;
    let a = &fx.authority;
    let CreateWorkspaceOutcome::Created(first) = a
        .create_workspace(
            "req-x",
            Some("Acme"),
            "robert@acme.com",
            DeploymentMode::Cloud,
            T0,
        )
        .await
        .unwrap()
    else {
        panic!("create");
    };
    // The SAME request by the SAME owner replays the SAME workspace AND the SAME self-invite link.
    let CreateWorkspaceOutcome::Replayed(again) = a
        .create_workspace(
            "req-x",
            Some("Acme"),
            "robert@acme.com",
            DeploymentMode::Cloud,
            T0,
        )
        .await
        .unwrap()
    else {
        panic!("replay");
    };
    assert_eq!(again.workspace_id.as_str(), first.workspace_id.as_str());
    assert_eq!(again.invite_token, first.invite_token);
    // The SAME request under a DIFFERENT owner is denied — the slot belongs to the original.
    assert!(matches!(
        a.create_workspace("req-x", None, "mallory@evil.com", DeploymentMode::Cloud, T0)
            .await
            .unwrap(),
        CreateWorkspaceOutcome::Denied("request id already used")
    ));
}

#[sqlx::test]
async fn the_workspace_creation_cap_denies_the_fourth(pool: PgPool) {
    let fx = Fixture::new(pool, "st-cap").await;
    let a = &fx.authority;
    for n in 0..3 {
        let out = a
            .create_workspace(
                &format!("req-{n}"),
                None,
                "serial@founder.com",
                DeploymentMode::Cloud,
                T0,
            )
            .await
            .unwrap();
        assert!(
            matches!(out, CreateWorkspaceOutcome::Created(_)),
            "create {n}: {out:?}"
        );
    }
    // The 4th create is the typed cap denial…
    assert!(matches!(
        a.create_workspace(
            "req-3",
            None,
            "serial@founder.com",
            DeploymentMode::Cloud,
            T0
        )
        .await
        .unwrap(),
        CreateWorkspaceOutcome::Denied("workspace creation limit reached")
    ));
    // …and the SAME cap denies a standup approve by the same identity (one durable floor, both doors).
    let dpub = device_pub(&[38u8; 32]);
    let start = a
        .start_standup_device_auth(&dpub, "laptop", NOW, T0)
        .await
        .unwrap();
    assert!(matches!(
        a.approve_standup(
            &start.user_code,
            "serial@founder.com",
            None,
            DeploymentMode::Cloud,
            NOW,
            T0
        )
        .await
        .unwrap(),
        ApproveStandupOutcome::Denied("workspace creation limit reached")
    ));
}

#[sqlx::test]
async fn racing_creates_at_the_cap_boundary_never_overshoot_it(pool: PgPool) {
    // The per-owner cap has NO co-located CAS — it is a COUNT-then-INSERT read-write invariant that
    // relies on Postgres SSI (the serializable runner's retry) to serialize. Two concurrent creates by
    // the SAME owner sitting at cap-1, with DIFFERENT request ids: whatever the interleave, exactly ONE
    // may win (3 owned total) and the other must resolve to the typed cap denial — never 4 owned.
    let fx = Fixture::new(pool.clone(), "st-cap-race").await;
    let a = &fx.authority;
    for n in 0..2 {
        assert!(matches!(
            a.create_workspace(
                &format!("seed-{n}"),
                None,
                "serial@founder.com",
                DeploymentMode::Cloud,
                T0
            )
            .await
            .unwrap(),
            CreateWorkspaceOutcome::Created(_)
        ));
    }
    let (r1, r2) = tokio::join!(
        a.create_workspace(
            "race-a",
            None,
            "serial@founder.com",
            DeploymentMode::Cloud,
            T0
        ),
        a.create_workspace(
            "race-b",
            None,
            "serial@founder.com",
            DeploymentMode::Cloud,
            T0
        ),
    );
    let outcomes = [r1.unwrap(), r2.unwrap()];
    let created = outcomes
        .iter()
        .filter(|o| matches!(o, CreateWorkspaceOutcome::Created(_)))
        .count();
    let denied = outcomes
        .iter()
        .filter(|o| {
            matches!(
                o,
                CreateWorkspaceOutcome::Denied("workspace creation limit reached")
            )
        })
        .count();
    assert_eq!(
        (created, denied),
        (1, 1),
        "exactly one racer lands the third workspace and one takes the typed cap denial: {outcomes:?}"
    );
    // The durable floor held: the owner owns exactly the cap, never cap+1.
    let owned = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM workspace_member \
         WHERE principal = $1 AND role = 'owner' AND status = 'confirmed'",
    )
    .bind("serial@founder.com")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owned, 3, "SSI must serialize the COUNT-then-INSERT pair");
}

#[sqlx::test]
async fn a_cloud_claim_shares_the_workspace_creation_cap(pool: PgPool) {
    // The break-glass door shares the same durable floor as the two self-serve doors: a CLOUD claim
    // minted for an at-cap owner email may not seat a 4th workspace for that identity.
    let fx = Fixture::new(pool.clone(), "st-claim-cap").await;
    let a = &fx.authority;
    for n in 0..3 {
        assert!(matches!(
            a.create_workspace(
                &format!("req-{n}"),
                None,
                "serial@founder.com",
                DeploymentMode::Cloud,
                T0
            )
            .await
            .unwrap(),
            CreateWorkspaceOutcome::Created(_)
        ));
    }
    let w = ws("w_fourth");
    let token = mint(
        a,
        &w,
        None,
        Some("serial@founder.com"),
        DeploymentMode::Cloud,
        60_000,
    )
    .await;
    let dpub = device_pub(&[51u8; 32]);
    assert!(matches!(
        a.admin_claim(&token, dpub, NOW, T0).await.unwrap(),
        RedeemOutcome::Denied("workspace creation limit reached")
    ));
    // The denial wrote nothing: no workspace, no owner row.
    assert!(a.db().read_workspace(&w).await.unwrap().is_none());
    assert_eq!(owner_rows(&pool, "w_fourth").await, 0);

    // A SELF-HOST claim is unaffected — device-rooted, the operator-run posture with no self-serve cap:
    // even a device principal already owning 3 workspaces still redeems.
    let sh = Fixture::with_mode(pool.clone(), "st-claim-cap-sh", DeploymentMode::SelfHost).await;
    let a = &sh.authority;
    let dpub = device_pub(&[52u8; 32]);
    let dev_owner = prin(&format!("dev.{}", device_key_id_for(&dpub)));
    for n in 0..3 {
        let owned = ws(&format!("w_sh_owned_{n}"));
        a.db()
            .seed_workspace(&owned, "Owned", "unverified", "self_host")
            .await
            .unwrap();
        a.db()
            .seed_workspace_member(&owned, &dev_owner, "owner", "confirmed")
            .await
            .unwrap();
    }
    let w_sh = ws("w_sh_fourth");
    let token = mint(a, &w_sh, None, None, DeploymentMode::SelfHost, 60_000).await;
    let RedeemOutcome::Redeemed(r) = a.admin_claim(&token, dpub, NOW, T0).await.unwrap() else {
        panic!("a self-host claim stays uncapped");
    };
    assert_eq!(r.principal.as_str(), dev_owner.as_str());
    assert_eq!(owner_rows(&pool, "w_sh_fourth").await, 1);
}

#[sqlx::test]
async fn a_consumed_cloud_claim_replays_redeemed_even_at_cap(pool: PgPool) {
    // The cap check sits AFTER the consumed-replay probe: an owner whose successful claim redeem put them
    // AT the cap must still recover a lost 200 by replaying — the probe answers before the cap can deny.
    let fx = Fixture::new(pool.clone(), "st-claim-cap-replay").await;
    let a = &fx.authority;
    for n in 0..2 {
        assert!(matches!(
            a.create_workspace(
                &format!("req-{n}"),
                None,
                "serial@founder.com",
                DeploymentMode::Cloud,
                T0
            )
            .await
            .unwrap(),
            CreateWorkspaceOutcome::Created(_)
        ));
    }
    let w = ws("w_third");
    let token = mint(
        a,
        &w,
        None,
        Some("serial@founder.com"),
        DeploymentMode::Cloud,
        60_000,
    )
    .await;
    let dpub = device_pub(&[53u8; 32]);
    let RedeemOutcome::Redeemed(_) = a.admin_claim(&token, dpub, NOW, T0).await.unwrap() else {
        panic!("the third workspace is under the cap");
    };
    // Now at cap — the same-device replay of the consumed claim still answers Redeemed.
    let RedeemOutcome::Redeemed(replay) = a.admin_claim(&token, dpub, NOW, T0).await.unwrap()
    else {
        panic!("the consumed-replay probe must answer before the cap");
    };
    assert_eq!(replay.workspace_id.as_str(), "w_third");
    assert_eq!(owner_rows(&pool, "w_third").await, 1);
}

// ── door 1: the standup session (start → verify → approve → poll → redeem → genesis publish) ──────────

#[sqlx::test]
async fn standup_flow_end_to_end_through_the_genesis_publish_gate(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "st-hero").await;
    let a = &fx.authority;
    let device_seed = [40u8; 32];
    let dpub = device_pub(&device_seed);

    // START: no invite, no workspace; the HIGH-ENTROPY opaque code (a 32-byte base64url token — it rides
    // only inside `verification_uri_complete`, clicked never typed) and the verify-base URIs.
    let start = a
        .start_standup_device_auth(&dpub, "founder-laptop", NOW, T0)
        .await
        .unwrap();
    assert!(
        start.user_code.len() >= 40
            && start
                .user_code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "standup code is a long opaque URL-safe token, got {:?}",
        start.user_code
    );
    assert_eq!(start.verification_uri, "https://plane.test/verify");
    assert_eq!(
        start.verification_uri_complete,
        format!("https://plane.test/verify/{}", start.user_code)
    );

    // The poll is Pending until a human approves.
    assert!(matches!(
        a.poll_device_auth(&start.device_code, NOW, T0)
            .await
            .unwrap(),
        DeviceAuthPoll::Pending
    ));

    // The verification disclosure: intent standup, NO workspace yet ("" — the page renders standup copy).
    let ctx = a
        .read_verification_context(&start.user_code, NOW)
        .await
        .unwrap();
    assert_eq!(ctx.intent, SessionIntent::Standup);
    assert_eq!(ctx.machine_name, "founder-laptop");
    assert_eq!(ctx.workspace_display_name, "");
    assert!(ctx.offered_skills.is_empty());

    // Approving an UNKNOWN code is the uniform miss (guessing is the entropy's job; probing learns nothing).
    assert!(matches!(
        a.approve_standup(
            "XXXX-XXXX-XXXX-XXXX",
            "founder@acme.com",
            None,
            DeploymentMode::Cloud,
            NOW,
            T0
        )
        .await,
        Err(AuthorityError::NotFound)
    ));

    // APPROVE: the workspace is created and the session confirmed in ONE txn.
    let ApproveStandupOutcome::Approved {
        workspace_id: w,
        display_name,
    } = a
        .approve_standup(
            &start.user_code,
            "founder@acme.com",
            Some("Acme"),
            DeploymentMode::Cloud,
            NOW,
            T0,
        )
        .await
        .unwrap()
    else {
        panic!("approve");
    };
    assert_eq!(display_name, "Acme");
    assert_eq!(owner_rows(&pool, w.as_str()).await, 1);

    // A same-email RE-CLICK is AlreadyApproved (idempotent); a DIFFERENT email is the uniform miss
    // (first-writer-wins — the approval is never re-bound).
    assert!(matches!(
        a.approve_standup(&start.user_code, "founder@acme.com", None, DeploymentMode::Cloud, NOW, T0)
            .await
            .unwrap(),
        ApproveStandupOutcome::AlreadyApproved { workspace_id } if workspace_id.as_str() == w.as_str()
    ));
    assert!(matches!(
        a.approve_standup(
            &start.user_code,
            "mallory@evil.com",
            None,
            DeploymentMode::Cloud,
            NOW,
            T0
        )
        .await,
        Err(AuthorityError::NotFound)
    ));
    assert_eq!(owner_rows(&pool, w.as_str()).await, 1);

    // POLL: granted, carrying the workspace context the standup client lacks.
    let grant = match a
        .poll_device_auth(&start.device_code, NOW, T0)
        .await
        .unwrap()
    {
        DeviceAuthPoll::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    };
    assert_eq!(grant.workspace_id.as_str(), w.as_str());
    assert_eq!(grant.workspace_display_name, "Acme");

    // REDEEM: admits the owner (seated `confirmed` — the cloud gate admits any rostered status) and
    // registers the device, minting its ONE workspace credential (deterministic per grant).
    let out = redeem(a, &grant, &device_seed, dpub).await;
    let RedeemOutcome::Redeemed(r) = out else {
        panic!("redeem: {out:?}");
    };
    assert_eq!(r.principal.as_str(), "founder@acme.com");
    assert!(!r.credential.is_empty());

    // GENESIS PUBLISH: the registered device + its confirmed OWNER membership pass the genesis
    // (confirmed-member) gate — the first publish self-seats the roster follow-state and lands at (1,1).
    // The write presents the REAL minted credential (not the seed-helper convention), authenticated by
    // its stored sha256.
    let s = skill("s_onboarding");
    let receipt = fx
        .authority
        .publish(
            &w,
            &s,
            &op("b1111111-1111-4111-8111-111111111111"),
            genesis(vec![file("SKILL.md", b"standup genesis\n")]),
            DeviceOpAuth {
                credential: r.credential.clone(),
                op: DeviceOp::PublishDirect,
                expected: gn(0, 0),
            },
            None,
            CREATED_AT,
            NOW,
        )
        .await
        .unwrap();
    assert_eq!(receipt.outcome, TerminalOutcome::Ok);
    assert_eq!(receipt.current, Some(gn(1, 1)));
}

#[sqlx::test]
async fn standup_start_is_the_uniform_miss_on_a_self_host_plane(pool: PgPool) {
    let fx = Fixture::with_mode(pool, "st-selfhost", DeploymentMode::SelfHost).await;
    let a = &fx.authority;
    let dpub = device_pub(&[41u8; 32]);
    assert!(matches!(
        a.start_standup_device_auth(&dpub, "laptop", NOW, T0).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn standup_sessions_refuse_every_enroll_identity_leg(pool: PgPool) {
    let fx = Fixture::new(pool, "st-intent-guard").await;
    let a = &fx.authority;
    let dpub = device_pub(&[42u8; 32]);
    let start = a
        .start_standup_device_auth(&dpub, "laptop", NOW, T0)
        .await
        .unwrap();

    // A standup session is only ever advanced by approve_standup: the passcode start/complete and the
    // OIDC confirm are all the uniform miss.
    assert!(matches!(
        a.start_passcode(&start.user_code, "x@y.com", NOW, T0).await,
        Err(AuthorityError::NotFound)
    ));
    assert!(matches!(
        a.complete_passcode(&start.user_code, "x@y.com", "000000", NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
    assert!(matches!(
        a.confirm_external_identity(&start.user_code, "x@y.com", NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn approve_standup_refuses_an_enroll_session(pool: PgPool) {
    let fx = Fixture::new(pool, "st-approve-enroll").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
    let invite = make_invite(
        a,
        &w,
        &owner_seed,
        &owner_dk,
        &op_id(1),
        "alice@acme.com",
        "s_deploy",
    )
    .await;
    let start = a
        .start_device_auth(&invite, &device_pub(&[43u8; 32]), "laptop", NOW, T0)
        .await
        .unwrap();
    assert!(
        start.user_code.len() >= 40
            && start
                .user_code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "enroll codes share the opaque URL-safe token shape, got {:?}",
        start.user_code
    );
    // Approving an ENROLL session through the standup door is the uniform miss — it must never CREATE a
    // workspace for a session that already has one.
    assert!(matches!(
        a.approve_standup(
            &start.user_code,
            "alice@acme.com",
            None,
            DeploymentMode::Cloud,
            NOW,
            T0
        )
        .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn enroll_confirms_are_first_writer_wins(pool: PgPool) {
    let fx = Fixture::new(pool, "st-fww").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
    let invite = make_invite(
        a,
        &w,
        &owner_seed,
        &owner_dk,
        &op_id(1),
        "alice@acme.com",
        "s_deploy",
    )
    .await;
    let device_seed = [44u8; 32];
    let dpub = device_pub(&device_seed);
    let start = a
        .start_device_auth(&invite, &dpub, "laptop", NOW, T0)
        .await
        .unwrap();

    // First confirm binds alice; a SAME-principal re-confirm is idempotent; a DIFFERENT principal is the
    // uniform miss — the confirmed principal is never overwritten.
    a.confirm_external_identity(&start.user_code, "alice@acme.com", NOW)
        .await
        .unwrap();
    a.confirm_external_identity(&start.user_code, "alice@acme.com", NOW)
        .await
        .unwrap();
    assert!(matches!(
        a.confirm_external_identity(&start.user_code, "bob@acme.com", NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
    // The passcode leg agrees: bob cannot complete against alice's confirmed session (miss BEFORE any code
    // check), so the session's grant still names alice.
    assert!(matches!(
        a.complete_passcode(&start.user_code, "bob@acme.com", "000000", NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
    let grant = match a
        .poll_device_auth(&start.device_code, NOW, T0)
        .await
        .unwrap()
    {
        DeviceAuthPoll::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    };
    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("redeem");
    };
    assert_eq!(r.principal.as_str(), "alice@acme.com");
}

// ── the /i/ claim bootstrap + cross-door token separation ──────────────────────────────────────────────

#[sqlx::test]
async fn claim_bootstrap_serves_until_consumed_and_never_crosses_doors(pool: PgPool) {
    let fx = Fixture::new(pool, "st-claim-boot").await;
    let a = &fx.authority;
    let w = ws("w_boot");
    let token = mint(
        a,
        &w,
        Some("Boot Co"),
        Some("o@boot.com"),
        DeploymentMode::Cloud,
        60_000,
    )
    .await;

    // The claim bootstrap serves the claim's OWN disclosure: its name, no skills, the admin_claim method.
    let boot = a.read_claim_bootstrap(&token, NOW).await.unwrap();
    assert_eq!(boot.workspace_id.as_str(), "w_boot");
    assert_eq!(boot.display_name, "Boot Co");
    assert_eq!(boot.enrollment_method, "admin_claim");
    assert!(boot.skills.is_empty());

    // A claim token can NEVER resolve as an invite (disjoint tables), nor start a device-auth.
    assert!(matches!(
        a.read_invite_bootstrap(&token, NOW).await,
        Err(AuthorityError::NotFound)
    ));
    assert!(matches!(
        a.start_device_auth(&token, &device_pub(&[45u8; 32]), "laptop", NOW, T0)
            .await,
        Err(AuthorityError::NotFound)
    ));

    // Expired ⇒ the uniform miss; consumed ⇒ the uniform miss.
    assert!(matches!(
        a.read_claim_bootstrap(&token, NOW + 120_000).await,
        Err(AuthorityError::NotFound)
    ));
    let dpub = device_pub(&[46u8; 32]);
    assert!(matches!(
        a.admin_claim(&token, dpub, NOW, T0).await.unwrap(),
        RedeemOutcome::Redeemed(_)
    ));
    assert!(matches!(
        a.read_claim_bootstrap(&token, NOW).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn an_invite_token_never_redeems_as_a_claim(pool: PgPool) {
    let fx = Fixture::new(pool, "st-cross-invite").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
    let invite = make_invite(
        a,
        &w,
        &owner_seed,
        &owner_dk,
        &op_id(1),
        "alice@acme.com",
        "s_deploy",
    )
    .await;
    // The other cross-redeem direction: an invite token presented to the claim door is the uniform denial.
    assert!(matches!(
        a.admin_claim(&invite, device_pub(&[47u8; 32]), NOW, T0)
            .await
            .unwrap(),
        RedeemOutcome::Denied(_)
    ));
}
