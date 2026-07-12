//! The web-session roster leg — invite / remove / read-roster.
//!
//! The release-blocker witnesses for the session-authorized membership ops: the confirmed-owner
//! acting gate (one uniform denial for member / reviewer / invited / absent — and only a confirmed
//! member's denial is ever recorded), role-on-the-seat seeding, the self-host uniform denial, the
//! `request_id` replay/divergence discipline (including the cross-leg id collision with a
//! device-lane op), the last-owner lockout + instant revoke on remove, and the `web_session`
//! receipt method + acting-principal audit trail. An invitation is a ROSTER WRITE now — what the
//! ops disclose is the workspace ADDRESS (a name, not a door), and joining is the address flow
//! gated on the invited seat.

use super::enrollment_governance::{
    addr, device_pub, flow_to_grant, op_id, redeem, seat_owner, sign_governance,
};
use super::*;

use crate::{
    GovernanceOp, GovernanceOutcome, RedeemOutcome, Role, SessionInviteOutcome, SessionInviteRole,
};

const NOW: i64 = 1_000;
const T0: &str = "t0";
const CLOUD: DeploymentMode = DeploymentMode::Cloud;

/// Session-invite `emails` at `role` from `acting`, panicking on a denial. Returns the disclosed
/// `(address, seated)`.
async fn invite_ok(
    a: &Authority,
    w: &WorkspaceId,
    rid: &str,
    acting: &str,
    emails: &[&str],
    role: SessionInviteRole,
) -> (String, usize) {
    let emails: Vec<String> = emails.iter().map(|s| (*s).to_owned()).collect();
    match a
        .invite_members_session(w, rid, acting, &emails, role, CLOUD, T0, NOW)
        .await
        .unwrap()
    {
        SessionInviteOutcome::Invited { address, seated } => (address, seated),
        SessionInviteOutcome::Denied(reason) => panic!("invite denied: {reason}"),
    }
}

/// A seat row's `(role, status)` straight off the pool.
async fn seat_of(pool: &PgPool, ws: &str, email: &str) -> Option<(String, String)> {
    sqlx::query_as::<_, (String, String)>(
        "SELECT role, status FROM workspace_member WHERE workspace_id = $1 AND principal = $2",
    )
    .bind(ws)
    .bind(email)
    .fetch_optional(pool)
    .await
    .unwrap()
}

/// The workspace_events rows for a workspace as `(op_id, actor, gov_op_type, outcome, method)`.
async fn events_of(pool: &PgPool, ws: &str) -> Vec<(String, String, String, String, String)> {
    sqlx::query_as::<_, (String, String, String, String, String)>(
        "SELECT op_id, actor, gov_op_type, outcome, method FROM workspace_events \
         WHERE workspace_id = $1 ORDER BY op_id",
    )
    .bind(ws)
    .fetch_all(pool)
    .await
    .unwrap()
}

// ── the acting gate ─────────────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn acting_gate_denies_member_reviewer_invited_and_absent_uniformly(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-gate").await;
    let a = &fx.authority;
    let w = ws("w_gate");
    let (_seed, owner, _dk) = seat_owner(a, &w, "cloud").await;
    a.db()
        .seed_workspace_member(&w, &prin("member@acme.com"), "member", "confirmed")
        .await
        .unwrap();
    a.db()
        .seed_workspace_member(&w, &prin("rev@acme.com"), "reviewer", "confirmed")
        .await
        .unwrap();
    a.db()
        .seed_workspace_member(&w, &prin("invited@acme.com"), "member", "invited")
        .await
        .unwrap();

    let mut n = 0u64;
    for acting in [
        "member@acme.com",
        "rev@acme.com",
        "invited@acme.com",
        "stranger@evil.com",
    ] {
        n += 1;
        let inv = a
            .invite_members_session(
                &w,
                &op_id(100 + n),
                acting,
                &["x@acme.com".to_owned()],
                SessionInviteRole::Member,
                CLOUD,
                T0,
                NOW,
            )
            .await
            .unwrap();
        let SessionInviteOutcome::Denied(inv_reason) = inv else {
            panic!("{acting} must be denied");
        };
        let rem = a
            .roster_remove_session(
                &w,
                &op_id(200 + n),
                acting,
                "member@acme.com",
                CLOUD,
                T0,
                NOW,
            )
            .await
            .unwrap();
        let GovernanceOutcome::Denied(rem_reason) = rem else {
            panic!("{acting} must be denied");
        };
        // ONE uniform string across every non-owner acting shape and every op.
        assert_eq!(inv_reason, rem_reason);
    }
    // Nothing was seated.
    assert!(seat_of(&pool, "w_gate", "x@acme.com").await.is_none());

    // Only the CONFIRMED members' denials were recorded (member + reviewer, 2 ops each); the
    // invited seat and the stranger recorded NOTHING — a stranger cannot grow the ledger or squat
    // an op-id slot.
    let events = events_of(&pool, "w_gate").await;
    assert_eq!(events.len(), 4);
    for (_op, actor, _verb, outcome, method) in &events {
        assert!(
            actor == "member@acme.com" || actor == "rev@acme.com",
            "actor {actor}"
        );
        assert_eq!(outcome, "DENIED");
        assert_eq!(method, "web_session");
    }

    // The read: a confirmed member sees the seats AND the address (member-visible — it is a name,
    // not a door); invited/absent are the uniform miss.
    let view = a.read_roster(&w, "member@acme.com", CLOUD).await.unwrap();
    assert_eq!(view.address, format!("https://plane.test/{}", addr(&w)));
    assert_eq!(view.seats.len(), 4);
    assert!(matches!(
        a.read_roster(&w, "invited@acme.com", CLOUD).await,
        Err(AuthorityError::NotFound)
    ));
    assert!(matches!(
        a.read_roster(&w, "stranger@evil.com", CLOUD).await,
        Err(AuthorityError::NotFound)
    ));
    let owner_view = a.read_roster(&w, owner.as_str(), CLOUD).await.unwrap();
    assert_eq!(owner_view.seats.len(), 4);
}

// ── roles on the seat ───────────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn invite_seeds_the_requested_role_and_never_demotes_a_confirmed_seat(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-roles").await;
    let a = &fx.authority;
    let w = ws("w_roles");
    let (_seed, owner, _dk) = seat_owner(a, &w, "cloud").await;

    invite_ok(
        a,
        &w,
        &op_id(1),
        owner.as_str(),
        &["alice@acme.com"],
        SessionInviteRole::Member,
    )
    .await;
    invite_ok(
        a,
        &w,
        &op_id(2),
        owner.as_str(),
        &["rev@acme.com"],
        SessionInviteRole::Reviewer,
    )
    .await;
    assert_eq!(
        seat_of(&pool, "w_roles", "alice@acme.com").await,
        Some(("member".to_owned(), "invited".to_owned()))
    );
    assert_eq!(
        seat_of(&pool, "w_roles", "rev@acme.com").await,
        Some(("reviewer".to_owned(), "invited".to_owned()))
    );

    // Re-inviting the OWNER at member never demotes the confirmed seat (the shared never-demote
    // row-writer) — and an owner-role request is unrepresentable at the type level.
    invite_ok(
        a,
        &w,
        &op_id(3),
        owner.as_str(),
        &[owner.as_str()],
        SessionInviteRole::Member,
    )
    .await;
    assert_eq!(
        seat_of(&pool, "w_roles", owner.as_str()).await,
        Some(("owner".to_owned(), "confirmed".to_owned()))
    );
    assert_eq!(SessionInviteRole::parse("owner"), None);
}

/// The role comes from the SEAT, not any link: a reviewer invitee redeems through the ADDRESS into a
/// confirmed reviewer, and a verified-but-unrostered stranger dies at the roster gate.
#[sqlx::test]
async fn invited_seats_redeem_through_the_address_with_their_seeded_role(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-seat-role").await;
    let a = &fx.authority;
    let w = ws("w_sever");
    let (_seed, owner, _dk) = seat_owner(a, &w, "cloud").await;

    let (address, _n) = invite_ok(
        a,
        &w,
        &op_id(1),
        owner.as_str(),
        &["alice@acme.com"],
        SessionInviteRole::Reviewer,
    )
    .await;
    assert_eq!(address, format!("https://plane.test/{}", addr(&w)));

    let alice_seed = [21u8; 32];
    let alice_grant = flow_to_grant(a, &addr(&w), &alice_seed, "alice@acme.com").await;
    let RedeemOutcome::Redeemed(alice_red) =
        redeem(a, &w, &alice_grant, device_pub(&alice_seed)).await
    else {
        panic!("alice redeem");
    };
    assert_eq!(alice_red.principal.as_str(), "alice@acme.com");
    assert_eq!(
        seat_of(&pool, "w_sever", "alice@acme.com").await,
        Some(("reviewer".to_owned(), "confirmed".to_owned()))
    );
    assert!(
        a.resolve_read_scope("w_sever", "s_deploy", &alice_red.credential)
            .await
            .is_ok()
    );

    // A STRANGER with the address + a verified non-rostered email dies at the roster gate: the
    // address is public-shaped, the ROSTER is the lock.
    let eve_seed = [24u8; 32];
    let eve_grant = flow_to_grant(a, &addr(&w), &eve_seed, "eve@evil.com").await;
    assert!(matches!(
        redeem(a, &w, &eve_grant, device_pub(&eve_seed)).await,
        RedeemOutcome::Denied(_)
    ));
}

// ── posture ─────────────────────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn all_session_roster_ops_deny_on_self_host(pool: PgPool) {
    let fx = Fixture::new(pool, "sr-selfhost").await;
    let a = &fx.authority;
    let w = ws("w_sh");
    let (_seed, owner, _dk) = seat_owner(a, &w, "self_host").await;
    let sh = DeploymentMode::SelfHost;

    assert!(matches!(
        a.invite_members_session(
            &w,
            &op_id(1),
            owner.as_str(),
            &["a@x.com".to_owned()],
            SessionInviteRole::Member,
            sh,
            T0,
            NOW
        )
        .await
        .unwrap(),
        SessionInviteOutcome::Denied(_)
    ));
    assert!(matches!(
        a.roster_remove_session(&w, &op_id(2), owner.as_str(), "a@x.com", sh, T0, NOW)
            .await
            .unwrap(),
        GovernanceOutcome::Denied(_)
    ));
    assert!(matches!(
        a.read_roster(&w, owner.as_str(), sh).await,
        Err(AuthorityError::NotFound)
    ));
}

// ── idempotency + the cross-leg id space ────────────────────────────────────────────────────────

#[sqlx::test]
async fn request_id_replays_identically_and_divergence_is_denied(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-replay").await;
    let a = &fx.authority;
    let w = ws("w_replay");
    let (owner_seed, owner, owner_dk) = seat_owner(a, &w, "cloud").await;

    // A malformed request id never reaches the transaction.
    assert!(matches!(
        a.invite_members_session(
            &w,
            "not-a-uuid",
            owner.as_str(),
            &["a@x.com".to_owned()],
            SessionInviteRole::Member,
            CLOUD,
            T0,
            NOW
        )
        .await
        .unwrap(),
        SessionInviteOutcome::Denied("request_id is not a canonical UUID")
    ));

    // Same request id + same payload ⇒ the byte-identical outcome (email order/duplication is
    // canonicalized away, so a resent form replays instead of key-reusing).
    let rid = op_id(10);
    let (addr_a, seated_a) = invite_ok(
        a,
        &w,
        &rid,
        owner.as_str(),
        &["b@x.com", "a@x.com"],
        SessionInviteRole::Member,
    )
    .await;
    let (addr_b, seated_b) = invite_ok(
        a,
        &w,
        &rid,
        owner.as_str(),
        &["a@x.com", "b@x.com", "a@x.com"],
        SessionInviteRole::Member,
    )
    .await;
    assert_eq!(addr_a, addr_b);
    assert_eq!(seated_a, 2);
    assert_eq!(seated_b, 2);

    // The same request id under a DIVERGENT payload is a denied key reuse.
    assert!(matches!(
        a.invite_members_session(
            &w,
            &rid,
            owner.as_str(),
            &["other@x.com".to_owned()],
            SessionInviteRole::Member,
            CLOUD,
            T0,
            NOW
        )
        .await
        .unwrap(),
        SessionInviteOutcome::Denied("op id reused with a different request")
    ));
    // …and under a different VERB too (remove reusing the invite's id).
    assert!(matches!(
        a.roster_remove_session(&w, &rid, owner.as_str(), "a@x.com", CLOUD, T0, NOW)
            .await
            .unwrap(),
        GovernanceOutcome::Denied("op id reused with a different request")
    ));

    // CROSS-LEG: a device-lane governance op's op_id can never replay as a session op (the
    // session preimage tag differs from the governance frame), and fails closed as a key reuse.
    let device_op = op_id(11);
    a.db()
        .seed_workspace_member(&w, &prin("carol@acme.com"), "member", "confirmed")
        .await
        .unwrap();
    let signed = sign_governance(
        &owner_seed,
        w.as_str(),
        &device_op,
        &owner_dk,
        GovernanceOp::RosterSet {
            role: Role::Reviewer,
            target: prin("carol@acme.com"),
        },
    );
    assert_eq!(
        a.roster_set(&w, &device_op, signed, T0, NOW).await.unwrap(),
        GovernanceOutcome::Ok
    );
    assert!(matches!(
        a.roster_remove_session(
            &w,
            &device_op,
            owner.as_str(),
            "carol@acme.com",
            CLOUD,
            T0,
            NOW
        )
        .await
        .unwrap(),
        GovernanceOutcome::Denied("op id reused with a different request")
    ));
}

#[sqlx::test]
async fn a_re_cased_retry_of_the_same_request_id_replays_identically(pool: PgPool) {
    // The session-leg request identity is computed over the FOLDED principals — `Principal::parse`
    // folds the acting owner and every invited email BEFORE `session_request_sha256` — so a retry
    // that re-cases both replays the byte-identical outcome (same address, same seated count), never
    // the key-reuse/divergent-payload denial.
    let fx = Fixture::new(pool, "sr-recase").await;
    let a = &fx.authority;
    let w = ws("w_recase");
    let (_seed, owner, _dk) = seat_owner(a, &w, "cloud").await;

    let rid = op_id(30);
    let (addr_a, seated_a) = invite_ok(
        a,
        &w,
        &rid,
        owner.as_str(),
        &["Alice@X.io"],
        SessionInviteRole::Member,
    )
    .await;
    // The SAME request id, acting owner AND email re-cased: the identical replay.
    let (addr_b, seated_b) = invite_ok(
        a,
        &w,
        &rid,
        "Owner@Acme.COM",
        &["alice@x.io"],
        SessionInviteRole::Member,
    )
    .await;
    assert_eq!(
        addr_a, addr_b,
        "the re-cased retry must replay the SAME address"
    );
    assert_eq!((seated_a, seated_b), (1, 1));
}

// ── remove: lockout + instant revoke ────────────────────────────────────────────────────────────

#[sqlx::test]
async fn remove_locks_out_the_last_owner_and_revokes_the_members_reads(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-remove").await;
    let a = &fx.authority;
    let w = ws("w_rm");
    let (_seed, owner, _dk) = seat_owner(a, &w, "cloud").await;
    let alice = prin("alice@acme.com");
    a.db()
        .seed_workspace_member(&w, &alice, "member", "confirmed")
        .await
        .unwrap();
    // Alice's device credential authenticates her device read lane; her confirmed membership is the gate.
    a.db()
        .seed_device(
            &w,
            "dk_alice",
            &dev_key(9),
            &alice,
            false,
            &cred(&w, "dk_alice"),
        )
        .await
        .unwrap();
    assert!(
        a.resolve_read_scope("w_rm", "s_deploy", &cred(&w, "dk_alice"))
            .await
            .is_ok(),
        "alice can read before she is removed"
    );

    // The last confirmed owner cannot be removed — typed, recorded.
    assert!(matches!(
        a.roster_remove_session(
            &w,
            &op_id(1),
            owner.as_str(),
            owner.as_str(),
            CLOUD,
            T0,
            NOW
        )
        .await
        .unwrap(),
        GovernanceOutcome::Denied("would remove the last owner")
    ));

    // Removing alice severs the seat in ONE transaction.
    assert!(matches!(
        a.roster_remove_session(
            &w,
            &op_id(2),
            owner.as_str(),
            alice.as_str(),
            CLOUD,
            T0,
            NOW
        )
        .await
        .unwrap(),
        GovernanceOutcome::Ok
    ));
    assert!(seat_of(&pool, "w_rm", "alice@acme.com").await.is_none());
    // Her reads are instantly revoked: the member row is gone, so although her device credential still
    // resolves (the device is not revoked) the confirmed-member gate now denies — membership IS the
    // entitlement.
    assert!(matches!(
        a.resolve_read_scope("w_rm", "s_deploy", &cred(&w, "dk_alice"))
            .await,
        Err(AuthorityError::NotFound)
    ));

    // Removing an absent principal is an idempotent Ok (mirrors a DELETE of nothing).
    assert!(matches!(
        a.roster_remove_session(&w, &op_id(3), owner.as_str(), "ghost@x.com", CLOUD, T0, NOW)
            .await
            .unwrap(),
        GovernanceOutcome::Ok
    ));
}

// ── canonical principals (one mailbox, one identity, however the edge cased it) ─────────────────

#[sqlx::test]
async fn mixed_case_invite_variants_dedupe_to_one_canonical_seat(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-canon-invite").await;
    let a = &fx.authority;
    let w = ws("w_canon_inv");
    let (_seed, owner, _dk) = seat_owner(a, &w, "cloud").await;

    // ONE call carrying two casings of one mailbox: the parse fold makes them ONE principal, so
    // exactly one seat lands, stored canonical.
    let (_addr, seated) = invite_ok(
        a,
        &w,
        &op_id(1),
        owner.as_str(),
        &["Alice@Acme.COM", "alice@acme.com"],
        SessionInviteRole::Member,
    )
    .await;
    assert_eq!(seated, 1);
    assert_eq!(
        seat_of(&pool, "w_canon_inv", "alice@acme.com").await,
        Some(("member".to_owned(), "invited".to_owned()))
    );
    // Exactly one row for the mailbox under ANY casing — no mixed-case sibling seat exists.
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM workspace_member \
         WHERE workspace_id = $1 AND lower(principal) = $2",
    )
    .bind("w_canon_inv")
    .bind("alice@acme.com")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn mixed_case_acting_owner_passes_the_gate(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-canon-gate").await;
    let a = &fx.authority;
    let w = ws("w_canon_gate");
    a.db()
        .seed_workspace(&w, "Acme", "verified", "cloud")
        .await
        .unwrap();
    a.db()
        .seed_workspace_member(&w, &prin("robert@x.io"), "owner", "confirmed")
        .await
        .unwrap();

    // The web session's email arrives cased however the IdP rendered it; the gate folds, so the
    // canonical owner seat authorizes the op instead of denying the owner their own workspace.
    let (_addr, seated) = invite_ok(
        a,
        &w,
        &op_id(1),
        "Robert@X.io",
        &["sam@x.io"],
        SessionInviteRole::Member,
    )
    .await;
    assert_eq!(seated, 1);
    assert_eq!(
        seat_of(&pool, "w_canon_gate", "sam@x.io").await,
        Some(("member".to_owned(), "invited".to_owned()))
    );
    // The receipt names the CANONICAL acting principal, never the session's casing.
    let events = events_of(&pool, "w_canon_gate").await;
    assert_eq!(events.len(), 1);
    let (_op, actor, _verb, outcome, method) = &events[0];
    assert_eq!(
        (actor.as_str(), outcome.as_str(), method.as_str()),
        ("robert@x.io", "OK", "web_session")
    );
}

#[sqlx::test]
async fn mixed_case_remove_severs_the_canonical_seat(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-canon-remove").await;
    let a = &fx.authority;
    let w = ws("w_canon_rm");
    let (_seed, owner, _dk) = seat_owner(a, &w, "cloud").await;
    let bob = prin("bob@x.io");
    invite_ok(
        a,
        &w,
        &op_id(1),
        owner.as_str(),
        &["bob@x.io"],
        SessionInviteRole::Member,
    )
    .await;
    // Bob has a device with an applied fleet-state row (device × skill) — removal writes the FINAL
    // DETACH RECORD on the person's fleet rows. Register the device under the CANONICAL principal,
    // then stage a live (detached = 0) row.
    //
    // The detach is EVENT-EXACT: it freezes exactly what the removal cost this person, so the skill
    // must actually be ENTITLED (catalog row + a delivering source) before the removal — a fleet row
    // for a skill they never received is not something their removal takes away. Seat it in the
    // structural `everyone` channel, which every confirmed member receives.
    a.db()
        .seed_device(&w, "dk_bob", &dev_key(9), &bob, false, &cred(&w, "dk_bob"))
        .await
        .unwrap();
    a.db()
        .seed_catalog(&w, &skill("s_deploy"), "deploy")
        .await
        .unwrap();
    // The entitlement union is membership-gated, so bob must hold a CONFIRMED seat to be receiving
    // anything at the moment of removal (the invite above seats him `invited`; his redeem would
    // confirm it — seed that end state directly).
    a.db()
        .seed_workspace_member(&w, &bob, "member", "confirmed")
        .await
        .unwrap();
    sqlx::query("SELECT topos_ensure_everyone($1, 'seed')")
        .bind("w_canon_rm")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO channel_skills (workspace_id, channel_id, skill_id, added_by, added_at) \
         VALUES ($1, 'everyone', 's_deploy', 'seed', 'seed')",
    )
    .bind("w_canon_rm")
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO device_skill_state \
           (workspace_id, device_key_id, skill_id, applied_commit, reported_at, detached) \
         VALUES ($1, 'dk_bob', 's_deploy', NULL, 0, 0)",
    )
    .bind("w_canon_rm")
    .execute(&pool)
    .await
    .unwrap();

    // Remove by the MIXED-CASE spelling: the target folds, the canonical seat is severed, and the fleet
    // row is stamped detached at `now` — all in the one transaction (the instant-revoke shape). The
    // device row itself survives (re-adding the member re-enables it — the git/GitHub model).
    assert!(matches!(
        a.roster_remove_session(&w, &op_id(2), owner.as_str(), "Bob@X.io", CLOUD, T0, NOW)
            .await
            .unwrap(),
        GovernanceOutcome::Ok
    ));
    assert!(seat_of(&pool, "w_canon_rm", "bob@x.io").await.is_none());
    // The fleet row is now the FINAL DETACH RECORD: detached = 1, frozen at `now`.
    let (detached, detached_at) = sqlx::query_as::<_, (i64, Option<i64>)>(
        "SELECT detached, detached_at FROM device_skill_state \
         WHERE workspace_id = $1 AND device_key_id = 'dk_bob' AND skill_id = 's_deploy'",
    )
    .bind("w_canon_rm")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(detached, 1, "removal writes the final detach record");
    assert_eq!(detached_at, Some(NOW), "the detach is frozen at `now`");
    // The device_registry row is audit-retained — removal deletes the seat, never the device.
    let device_rows = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM device_registry WHERE workspace_id = $1 AND device_key_id = 'dk_bob'",
    )
    .bind("w_canon_rm")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(device_rows, 1, "the device row survives the member removal");
}

// ── receipts ────────────────────────────────────────────────────────────────────────────────────

#[sqlx::test]
async fn receipts_carry_the_method_discriminant_and_the_acting_principal(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-receipts").await;
    let a = &fx.authority;
    let w = ws("w_rcpt");
    let (owner_seed, owner, owner_dk) = seat_owner(a, &w, "cloud").await;

    invite_ok(
        a,
        &w,
        &op_id(1),
        owner.as_str(),
        &["alice@acme.com"],
        SessionInviteRole::Member,
    )
    .await;
    a.roster_remove_session(
        &w,
        &op_id(2),
        owner.as_str(),
        "alice@acme.com",
        CLOUD,
        T0,
        NOW,
    )
    .await
    .unwrap();
    // A DEVICE-credential op beside them, for the discriminant contrast.
    a.db()
        .seed_workspace_member(&w, &prin("carol@acme.com"), "member", "confirmed")
        .await
        .unwrap();
    let signed = sign_governance(
        &owner_seed,
        w.as_str(),
        &op_id(4),
        &owner_dk,
        GovernanceOp::RosterSet {
            role: Role::Reviewer,
            target: prin("carol@acme.com"),
        },
    );
    assert_eq!(
        a.roster_set(&w, &op_id(4), signed, T0, NOW).await.unwrap(),
        GovernanceOutcome::Ok
    );

    let events = events_of(&pool, "w_rcpt").await;
    assert_eq!(events.len(), 3);
    let by_op: std::collections::HashMap<_, _> = events
        .iter()
        .map(|(op, actor, verb, outcome, method)| {
            (
                op.clone(),
                (actor.clone(), verb.clone(), outcome.clone(), method.clone()),
            )
        })
        .collect();
    let (actor, verb, outcome, method) = &by_op[&op_id(1)];
    assert_eq!(
        (
            actor.as_str(),
            verb.as_str(),
            outcome.as_str(),
            method.as_str()
        ),
        (owner.as_str(), "invite", "OK", "web_session")
    );
    let (actor, verb, outcome, method) = &by_op[&op_id(2)];
    assert_eq!(
        (
            actor.as_str(),
            verb.as_str(),
            outcome.as_str(),
            method.as_str()
        ),
        (owner.as_str(), "roster_remove", "OK", "web_session")
    );
    // The device leg names the acting DEVICE KEY and the 'device' method.
    let (actor, verb, outcome, method) = &by_op[&op_id(4)];
    assert_eq!(
        (
            actor.as_str(),
            verb.as_str(),
            outcome.as_str(),
            method.as_str()
        ),
        (owner_dk.as_str(), "roster_set", "OK", "device")
    );
}

// ── concurrency (the session leg is a distinct code path; race it on its own) ─────────────────────

#[sqlx::test]
async fn raced_identical_invites_converge_to_one_byte_identical_outcome(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-race-invite").await;
    let a = &fx.authority;
    let w = ws("w_race");
    let (_seed, owner, _dk) = seat_owner(a, &w, "cloud").await;

    // Two concurrent invites with the SAME request_id + payload: the workspace_events hard INSERT
    // (in run_serializable!'s convergent-23505 set) makes the loser abort, retry, and replay — so
    // both return the byte-identical Invited and exactly one row lands.
    let rid = op_id(7);
    let emails = ["alice@acme.com".to_owned()];
    let (ra, rb) = tokio::join!(
        a.invite_members_session(
            &w,
            &rid,
            owner.as_str(),
            &emails,
            SessionInviteRole::Member,
            CLOUD,
            T0,
            NOW
        ),
        a.invite_members_session(
            &w,
            &rid,
            owner.as_str(),
            &emails,
            SessionInviteRole::Member,
            CLOUD,
            T0,
            NOW
        ),
    );
    let tok = |o: SessionInviteOutcome| match o {
        SessionInviteOutcome::Invited { address, seated } => (address, seated),
        SessionInviteOutcome::Denied(r) => panic!("raced invite denied: {r}"),
    };
    let (ta, sa) = tok(ra.unwrap());
    let (tb, sb) = tok(rb.unwrap());
    assert_eq!(ta, tb);
    assert_eq!((sa, sb), (1, 1));
    // Exactly ONE receipt for the shared request id.
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM workspace_events WHERE workspace_id = $1 AND op_id = $2",
    )
    .bind("w_race")
    .bind(&rid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(n, 1);
}

#[sqlx::test]
async fn raced_mutual_owner_removes_keep_one_owner(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "sr-race-remove").await;
    let a = &fx.authority;
    let w = ws("w_race_rm");
    let (_seed, owner1, _dk) = seat_owner(a, &w, "cloud").await;
    let owner2 = prin("owner2@acme.com");
    a.db()
        .seed_workspace_member(&w, &owner2, "owner", "confirmed")
        .await
        .unwrap();

    // Each owner concurrently removes the OTHER — the write-skew SERIALIZABLE must catch (the two
    // targets are different rows, so no co-located lock serializes them; the retry re-counts and
    // would_orphan_owner DENIES the loser).
    let (op1, op2) = (op_id(1), op_id(2));
    let (ra, rb) = tokio::join!(
        a.roster_remove_session(&w, &op1, owner1.as_str(), owner2.as_str(), CLOUD, T0, NOW),
        a.roster_remove_session(&w, &op2, owner2.as_str(), owner1.as_str(), CLOUD, T0, NOW),
    );
    let outcomes = [ra.unwrap(), rb.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| matches!(o, GovernanceOutcome::Ok))
            .count(),
        1,
        "exactly one raced remove may succeed"
    );
    let owners = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM workspace_member WHERE workspace_id = $1 AND role = 'owner' AND status = 'confirmed'",
    )
    .bind("w_race_rm")
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(owners, 1, "one confirmed owner always remains");
}

#[sqlx::test]
async fn a_session_request_id_slot_is_closed_to_a_later_device_op(pool: PgPool) {
    // The reverse cross-leg direction: a session op takes an op-id slot; a device-lane governance
    // op that reuses that id later fails closed as a key reuse (the two preimages can never match).
    let fx = Fixture::new(pool, "sr-cross-leg").await;
    let a = &fx.authority;
    let w = ws("w_cross");
    let (owner_seed, owner, owner_dk) = seat_owner(a, &w, "cloud").await;

    let shared = op_id(9);
    invite_ok(
        a,
        &w,
        &shared,
        owner.as_str(),
        &["alice@acme.com"],
        SessionInviteRole::Member,
    )
    .await;
    // A device-lane roster op reusing the session's request_id as its op_id is denied key-reuse.
    let signed = sign_governance(
        &owner_seed,
        w.as_str(),
        &shared,
        &owner_dk,
        GovernanceOp::RosterSet {
            role: Role::Reviewer,
            target: prin("alice@acme.com"),
        },
    );
    assert!(matches!(
        a.roster_set(&w, &shared, signed, T0, NOW).await.unwrap(),
        GovernanceOutcome::Denied("op id reused with a different request")
    ));
}
