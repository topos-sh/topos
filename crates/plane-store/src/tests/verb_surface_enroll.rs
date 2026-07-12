//! The verb surface's ENROLLMENT legs — the ONE uniform membership denial, the self-host posture
//! (pending sessions + the roster gate), the LOGIN door, and the workspace ADDRESS-name rules at
//! the genesis doors.
//!
//! Links are addresses and the ROSTER is the lock: an address that never resolved, a grant redeemed
//! against the wrong workspace, and an off-roster identity must be byte-for-byte indistinguishable
//! at the redeem — on EVERY deployment posture. The login door proves an identity once and re-mints
//! this device's credential in every confirmed seat, deterministically per `(grant, workspace)`.

use super::enrollment_governance::{
    addr, device_pub, flow_to_grant, redeem, seat_invited, seat_owner,
};
use super::*;

use crate::enroll::ENROLL_UNAVAILABLE;
use crate::{
    ApproveStandupOutcome, CreateWorkspaceOutcome, DeviceAuthPoll, LoginOutcome, PasscodeComplete,
    RedeemOutcome,
};

const NOW: i64 = 1_000;
const T0: &str = "t0";

/// Drive a LOGIN device flow to a grant: start → confirm (the external-identity leg) → poll(Granted).
async fn login_grant(
    a: &Authority,
    device_seed: &[u8; 32],
    confirm_as: &str,
) -> crate::GrantIssued {
    let dpub = device_pub(device_seed);
    let start = a
        .start_login_device_auth(&dpub, "laptop", NOW, T0)
        .await
        .unwrap();
    assert!(matches!(
        a.poll_device_auth(&start.device_code, NOW, T0)
            .await
            .unwrap(),
        DeviceAuthPoll::Pending
    ));
    a.confirm_external_identity(&start.user_code, confirm_as, NOW)
        .await
        .unwrap();
    match a
        .poll_device_auth(&start.device_code, NOW, T0)
        .await
        .unwrap()
    {
        DeviceAuthPoll::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    }
}

// ── the ONE uniform membership denial ───────────────────────────────────────────────────────────

#[sqlx::test]
async fn every_not_yours_redeem_answers_the_one_uniform_denial(pool: PgPool) {
    let fx = Fixture::new(pool, "vse-uniform").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    seat_owner(a, &w, "cloud").await;
    seat_invited(a, &w, "alice@acme.com").await;
    // A second workspace for the wrong-workspace redeem.
    let w2 = ws("w_other");
    a.db()
        .seed_workspace(&w2, "Other", "unverified", "cloud")
        .await
        .unwrap();

    // (1) A NONEXISTENT address: the flow runs end-to-end (start → confirm → grant), and the redeem —
    // wherever the client points it — answers the uniform denial.
    let ghost_seed = [71u8; 32];
    let ghost_grant = flow_to_grant(a, "no-such-team", &ghost_seed, "alice@acme.com").await;
    assert!(
        ghost_grant.workspace_id.is_none() && ghost_grant.workspace_address.is_none(),
        "an unresolved address issues a workspace-less grant"
    );
    let RedeemOutcome::Denied(nonexistent) =
        redeem(a, &w, &ghost_grant, device_pub(&ghost_seed)).await
    else {
        panic!("an unresolved-address grant must not redeem");
    };

    // (2) A WRONG-WORKSPACE redeem: alice's real grant for w_acme presented against w_other.
    let alice_seed = [72u8; 32];
    let alice_grant = flow_to_grant(a, &addr(&w), &alice_seed, "alice@acme.com").await;
    let RedeemOutcome::Denied(wrong_ws) =
        redeem(a, &w2, &alice_grant, device_pub(&alice_seed)).await
    else {
        panic!("a wrong-workspace redeem must be denied");
    };

    // (3) An OFF-ROSTER identity: eve proves her real mailbox, the address resolves — no seat, no entry.
    let eve_seed = [73u8; 32];
    let eve_grant = flow_to_grant(a, &addr(&w), &eve_seed, "eve@evil.com").await;
    let RedeemOutcome::Denied(off_roster) = redeem(a, &w, &eve_grant, device_pub(&eve_seed)).await
    else {
        panic!("an off-roster redeem must be denied");
    };

    // (4) A LOGIN grant presented at the enroll door reads the same.
    let login_seed = [74u8; 32];
    let lg = login_grant(a, &login_seed, "alice@acme.com").await;
    let RedeemOutcome::Denied(cross_door) = a
        .redeem_enrollment(&w, &lg.grant_token, device_pub(&login_seed), NOW)
        .await
        .unwrap()
    else {
        panic!("a login grant must not redeem at the enroll door");
    };

    // ONE byte-identical detail across all four — no oracle distinguishes them.
    assert_eq!(nonexistent, ENROLL_UNAVAILABLE);
    assert_eq!(wrong_ws, nonexistent);
    assert_eq!(off_roster, nonexistent);
    assert_eq!(cross_door, nonexistent);
    // …and the ALICE grant, redeemed where it belongs, still works (the denials burned nothing).
    assert!(matches!(
        redeem(a, &w, &alice_grant, device_pub(&alice_seed)).await,
        RedeemOutcome::Redeemed(_)
    ));
}

// ── the self-host posture ───────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn self_host_sessions_are_pending_and_the_roster_gates_the_redeem(pool: PgPool) {
    let fx = Fixture::with_mode(pool, "vse-selfhost", DeploymentMode::SelfHost).await;
    let a = &fx.authority;
    let w = ws("w_local");
    seat_owner(a, &w, "self_host").await;

    // The born-confirmed device-rooted shortcut is DEAD: a self-host session is pending until a human
    // identity is proven (its trust anchor was the invite bearer token, which no longer exists).
    let eve_seed = [75u8; 32];
    let dpub = device_pub(&eve_seed);
    let start = a
        .start_device_auth(&addr(&w), &dpub, "laptop", NOW, T0)
        .await
        .unwrap();
    assert!(matches!(
        a.poll_device_auth(&start.device_code, NOW, T0)
            .await
            .unwrap(),
        DeviceAuthPoll::Pending
    ));

    // An UN-ROSTERED identity is denied at redeem — self-host gates on the roster exactly like cloud.
    let eve_grant = flow_to_grant(a, &addr(&w), &eve_seed, "eve@evil.com").await;
    let RedeemOutcome::Denied(reason) = redeem(a, &w, &eve_grant, dpub).await else {
        panic!("an un-rostered identity must not redeem on self-host");
    };
    assert_eq!(reason, ENROLL_UNAVAILABLE);

    // An INVITED identity proves the same passcode leg and joins — the whole loop runs on self-host.
    seat_invited(a, &w, "alice@acme.com").await;
    let alice_seed = [76u8; 32];
    let alice_grant = flow_to_grant(a, &addr(&w), &alice_seed, "alice@acme.com").await;
    let RedeemOutcome::Redeemed(r) = redeem(a, &w, &alice_grant, device_pub(&alice_seed)).await
    else {
        panic!("an invited identity joins on self-host");
    };
    assert_eq!(r.principal.as_str(), "alice@acme.com");

    // LOGIN runs on self-host too (unlike standup, which stays cloud-only).
    let lg = login_grant(a, &alice_seed, "alice@acme.com").await;
    let LoginOutcome::Redeemed(login) = a
        .redeem_login(&lg.grant_token, device_pub(&alice_seed), NOW)
        .await
        .unwrap()
    else {
        panic!("login redeems on self-host");
    };
    assert_eq!(login.memberships.len(), 1);
}

// ── the login door ──────────────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn login_re_mints_a_distinct_deterministic_credential_per_confirmed_seat(pool: PgPool) {
    let fx = Fixture::new(pool, "vse-login").await;
    let a = &fx.authority;
    let (w1, w2) = (ws("w_one"), ws("w_two"));
    a.db()
        .seed_workspace(&w1, "One", "unverified", "cloud")
        .await
        .unwrap();
    a.db()
        .seed_workspace(&w2, "Two", "unverified", "cloud")
        .await
        .unwrap();
    let alice = prin("alice@acme.com");
    a.db()
        .seed_workspace_member(&w1, &alice, "member", "confirmed")
        .await
        .unwrap();
    a.db()
        .seed_workspace_member(&w2, &alice, "reviewer", "confirmed")
        .await
        .unwrap();
    // An INVITED seat elsewhere must NOT appear (confirmed seats only).
    let w3 = ws("w_three");
    a.db()
        .seed_workspace(&w3, "Three", "unverified", "cloud")
        .await
        .unwrap();
    a.db()
        .seed_workspace_member(&w3, &alice, "member", "invited")
        .await
        .unwrap();

    let device_seed = [77u8; 32];
    let dpub = device_pub(&device_seed);
    let grant = login_grant(a, &device_seed, "alice@acme.com").await;
    let LoginOutcome::Redeemed(first) =
        a.redeem_login(&grant.grant_token, dpub, NOW).await.unwrap()
    else {
        panic!("login redeem");
    };
    assert_eq!(first.principal.as_str(), "alice@acme.com");
    assert_eq!(first.memberships.len(), 2, "confirmed seats only");
    // Ordered by workspace id, carrying the seat's role + the workspace's address facts.
    assert_eq!(first.memberships[0].workspace_id.as_str(), "w_one");
    assert_eq!(first.memberships[0].role, "member");
    assert_eq!(first.memberships[0].name, "w-one");
    assert_eq!(first.memberships[1].workspace_id.as_str(), "w_two");
    assert_eq!(first.memberships[1].role, "reviewer");
    // Each workspace got a DISTINCT credential; both resolve their own workspace's read lane and
    // never the other's.
    let c1 = first.memberships[0].credential.clone().expect("w1 mint");
    let c2 = first.memberships[1].credential.clone().expect("w2 mint");
    assert_ne!(c1, c2);
    assert!(a.resolve_read_scope("w_one", "s_x", &c1).await.is_ok());
    assert!(a.resolve_read_scope("w_two", "s_x", &c2).await.is_ok());
    assert!(matches!(
        a.resolve_read_scope("w_two", "s_x", &c1).await,
        Err(AuthorityError::NotFound)
    ));

    // A lost-ack REPLAY of the same grant re-returns the identical plaintexts (deterministic per
    // (grant, workspace)).
    let LoginOutcome::Redeemed(replay) =
        a.redeem_login(&grant.grant_token, dpub, NOW).await.unwrap()
    else {
        panic!("login replay");
    };
    assert_eq!(
        replay.memberships[0].credential.as_deref(),
        Some(c1.as_str())
    );
    assert_eq!(
        replay.memberships[1].credential.as_deref(),
        Some(c2.as_str())
    );

    // The binding check holds: a DIFFERENT device presenting the stolen grant is denied.
    assert!(matches!(
        a.redeem_login(&grant.grant_token, device_pub(&[78u8; 32]), NOW)
            .await
            .unwrap(),
        LoginOutcome::Denied(_)
    ));
    // An ENROLL grant refused at the login door (the other cross-door direction).
    seat_invited(a, &w1, "bob@acme.com").await;
    let bob_seed = [79u8; 32];
    let bob_grant = flow_to_grant(a, "w-one", &bob_seed, "bob@acme.com").await;
    assert!(matches!(
        a.redeem_login(&bob_grant.grant_token, device_pub(&bob_seed), NOW)
            .await
            .unwrap(),
        LoginOutcome::Denied(_)
    ));
}

#[sqlx::test]
async fn login_blocks_a_revoked_or_squatted_seat_and_mints_the_rest(pool: PgPool) {
    let fx = Fixture::new(pool, "vse-login-blocked").await;
    let a = &fx.authority;
    let (w1, w2, w3) = (ws("w_one"), ws("w_two"), ws("w_three"));
    let alice = prin("alice@acme.com");
    for (w, dn) in [(&w1, "One"), (&w2, "Two"), (&w3, "Three")] {
        a.db()
            .seed_workspace(w, dn, "unverified", "cloud")
            .await
            .unwrap();
        a.db()
            .seed_workspace_member(w, &alice, "member", "confirmed")
            .await
            .unwrap();
    }
    let device_seed = [81u8; 32];
    let dpub = device_pub(&device_seed);
    let dkid = crate::enroll::device_key_id_for(&dpub);
    // In w2 this device is REVOKED; in w3 its key id is SQUATTED by a different key.
    a.db()
        .seed_device(&w2, &dkid, &dpub, &alice, true, &cred(&w2, &dkid))
        .await
        .unwrap();
    a.db()
        .seed_device(&w3, &dkid, &[9u8; 32], &alice, false, &cred(&w3, &dkid))
        .await
        .unwrap();

    let grant = login_grant(a, &device_seed, "alice@acme.com").await;
    let LoginOutcome::Redeemed(out) = a.redeem_login(&grant.grant_token, dpub, NOW).await.unwrap()
    else {
        panic!("login redeem");
    };
    assert_eq!(out.memberships.len(), 3);
    // Ordered by workspace id ("w_one" < "w_three" < "w_two").
    let by_ws = |id: &str| {
        out.memberships
            .iter()
            .find(|m| m.workspace_id.as_str() == id)
            .unwrap()
    };
    // w1: fresh registration + mint.
    assert!(by_ws("w_one").credential.is_some());
    assert!(by_ws("w_one").blocked.is_none());
    // w2: revoked there — blocked, no credential, and the revoke was NOT undone.
    assert!(by_ws("w_two").credential.is_none());
    assert_eq!(
        by_ws("w_two").blocked,
        Some("device revoked in this workspace — enroll a fresh device or ask an owner")
    );
    assert!(matches!(
        a.resolve_read_scope("w_two", "s_x", &cred(&w2, &dkid))
            .await,
        Err(AuthorityError::NotFound)
    ));
    // w3: the key id is bound to a DIFFERENT key — blocked (anti-squat), the squatter's row untouched.
    assert!(by_ws("w_three").credential.is_none());
    assert_eq!(
        by_ws("w_three").blocked,
        Some("device key id already bound to a different key/principal")
    );
}

#[sqlx::test]
async fn a_zero_seat_login_redeems_with_an_empty_membership_list(pool: PgPool) {
    let fx = Fixture::new(pool, "vse-login-empty").await;
    let a = &fx.authority;
    let device_seed = [82u8; 32];
    let grant = login_grant(a, &device_seed, "nobody@nowhere.io").await;
    let LoginOutcome::Redeemed(out) = a
        .redeem_login(&grant.grant_token, device_pub(&device_seed), NOW)
        .await
        .unwrap()
    else {
        panic!("a zero-seat login is a VALID success (identity established, nothing to mint)");
    };
    assert_eq!(out.principal.as_str(), "nobody@nowhere.io");
    assert!(out.memberships.is_empty());
}

// ── the ADDRESS-name rules at the genesis doors ─────────────────────────────────────────────────

#[sqlx::test]
async fn create_workspace_validates_dedupes_and_returns_the_address(pool: PgPool) {
    let fx = Fixture::new(pool, "vse-names").await;
    let a = &fx.authority;

    // An EXPLICIT reserved/malformed name is a typed refusal (nothing created).
    for bad in ["api", "everyone", "Not Valid", "v2", "x-archived-2026"] {
        assert!(
            matches!(
                a.create_workspace(
                    "req-bad",
                    None,
                    Some(bad),
                    "o@x.io",
                    DeploymentMode::Cloud,
                    T0
                )
                .await
                .unwrap(),
                CreateWorkspaceOutcome::Denied(_)
            ),
            "explicit name {bad:?} must be refused"
        );
    }

    // An explicit VALID name lands verbatim and roots the address.
    let CreateWorkspaceOutcome::Created(c) = a
        .create_workspace(
            "req-1",
            Some("Acme Inc"),
            Some("acme"),
            "o@x.io",
            DeploymentMode::Cloud,
            T0,
        )
        .await
        .unwrap()
    else {
        panic!("create");
    };
    assert_eq!(c.name, "acme");
    assert_eq!(c.address, "https://plane.test/acme");

    // An explicit TAKEN name is the typed refusal…
    assert!(matches!(
        a.create_workspace(
            "req-2",
            None,
            Some("acme"),
            "p@y.io",
            DeploymentMode::Cloud,
            T0
        )
        .await
        .unwrap(),
        CreateWorkspaceOutcome::Denied("workspace name already taken")
    ));
    // …while a DERIVED collision dedupes with a numeric suffix.
    let CreateWorkspaceOutcome::Created(d) = a
        .create_workspace(
            "req-3",
            Some("Acme"),
            None,
            "p@y.io",
            DeploymentMode::Cloud,
            T0,
        )
        .await
        .unwrap()
    else {
        panic!("derived create");
    };
    assert_eq!(d.name, "acme-2");
    assert_eq!(d.address, "https://plane.test/acme-2");

    // A display name that slugifies to NOTHING falls back to the stable id-derived name.
    let CreateWorkspaceOutcome::Created(e) = a
        .create_workspace(
            "req-4",
            Some("!!!"),
            None,
            "q@z.io",
            DeploymentMode::Cloud,
            T0,
        )
        .await
        .unwrap()
    else {
        panic!("fallback create");
    };
    assert!(e.name.starts_with("ws-"), "fallback name: {}", e.name);
}

#[sqlx::test]
async fn approve_standup_applies_the_same_name_rules(pool: PgPool) {
    let fx = Fixture::new(pool, "vse-standup-name").await;
    let a = &fx.authority;
    let dpub = device_pub(&[83u8; 32]);
    let start = a
        .start_standup_device_auth(&dpub, "laptop", NOW, T0)
        .await
        .unwrap();
    // A reserved explicit slug from the web form is the typed refusal (the session stays approvable).
    assert!(matches!(
        a.approve_standup(
            &start.user_code,
            "founder@acme.com",
            Some("Acme"),
            Some("topos"),
            DeploymentMode::Cloud,
            NOW,
            T0,
        )
        .await
        .unwrap(),
        ApproveStandupOutcome::Denied(_)
    ));
    // A valid explicit slug lands, and the granted poll carries its address.
    assert!(matches!(
        a.approve_standup(
            &start.user_code,
            "founder@acme.com",
            Some("Acme"),
            Some("acme-team"),
            DeploymentMode::Cloud,
            NOW,
            T0,
        )
        .await
        .unwrap(),
        ApproveStandupOutcome::Approved { .. }
    ));
    let granted = match a
        .poll_device_auth(&start.device_code, NOW, T0)
        .await
        .unwrap()
    {
        DeviceAuthPoll::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    };
    assert_eq!(
        granted.workspace_address.as_deref(),
        Some("https://plane.test/acme-team")
    );
}

#[sqlx::test]
async fn a_passcode_confirms_a_login_session_too(pool: PgPool) {
    // The identity legs serve enroll AND login alike (the intent guard admits both; only standup is
    // excluded — its sole leg is the web approve).
    let fx = Fixture::new(pool, "vse-login-passcode").await;
    let a = &fx.authority;
    let device_seed = [84u8; 32];
    let dpub = device_pub(&device_seed);
    let start = a
        .start_login_device_auth(&dpub, "laptop", NOW, T0)
        .await
        .unwrap();
    let pc = a
        .start_passcode(&start.user_code, "alice@acme.com", NOW, T0)
        .await
        .unwrap();
    assert_eq!(
        a.complete_passcode(&start.user_code, "alice@acme.com", &pc.passcode, NOW)
            .await
            .unwrap(),
        PasscodeComplete::Confirmed
    );
    let grant = match a
        .poll_device_auth(&start.device_code, NOW, T0)
        .await
        .unwrap()
    {
        DeviceAuthPoll::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    };
    assert!(
        grant.workspace_id.is_none(),
        "a login grant is workspace-less"
    );
    let LoginOutcome::Redeemed(out) = a.redeem_login(&grant.grant_token, dpub, NOW).await.unwrap()
    else {
        panic!("login redeem");
    };
    assert_eq!(out.principal.as_str(), "alice@acme.com");
}
