//! Split from the former monolithic `tests.rs` (behavior-preserving).
use super::*;

use crate::enroll::device_key_id_for;
use crate::{
    ConfirmOutcome, CreateInviteOutcome, CreateWorkspaceOutcome, DeviceAuthPoll, GovernanceOp,
    GovernanceOutcome, GovernanceRequest, GrantIssued, PasscodeComplete, RedeemOutcome, Role,
};

const NOW: i64 = 1_000;

/// A canonical lowercase-hyphenated UUID op id seeded by `n`.
pub(super) fn op_id(n: u64) -> String {
    format!("00000000-0000-4000-8000-{n:012x}")
}

/// A device public key from a seed. Nothing verifies it — the credential presented at enroll/redeem and
/// governance is the `device_key_id` derived from this public key — so the seed IS the key.
pub(super) fn device_pub(seed: &[u8; 32]) -> [u8; 32] {
    *seed
}

/// Pull the opaque token out of a `/i/<token>` link.
fn token_of(link: &str) -> String {
    link.rsplit('/').next().expect("a link tail").to_owned()
}

/// Build a governance request as an owner's device presents it — the acting device's workspace
/// CREDENTIAL + the typed op. The transaction resolves the credential to its registry row (the acting
/// `device_key_id`) and authenticates by that lookup; nothing signs. The credential is derived from
/// `(ws, device_key_id)` so it matches whatever `seat_owner`/`seed_device` seeded the acting device with.
/// (`_owner_seed`, `_op_id` are vestigial now that nothing signs — kept so the call sites stay stable;
/// `op_id` rides the outer `create_invite`/`roster_*`/`revoke_device` call, never the request body.)
pub(super) fn sign_governance(
    _owner_seed: &[u8; 32],
    ws: &str,
    _op_id: &str,
    device_key_id: &str,
    op: GovernanceOp,
) -> GovernanceRequest {
    GovernanceRequest {
        credential: cred_str(ws, device_key_id),
        op,
    }
}

/// Seat an owner: a workspace row, an `owner`/`confirmed` member, and the owner's registered device.
/// Returns `(owner_seed, owner_principal, owner_device_key_id)`.
pub(super) async fn seat_owner(
    a: &Authority,
    w: &WorkspaceId,
    mode: &str,
) -> ([u8; 32], Principal, String) {
    a.db()
        .seed_workspace(w, "Acme", "verified", mode)
        .await
        .unwrap();
    let owner_seed = [7u8; 32];
    let owner_pub = device_pub(&owner_seed);
    let owner_dk = device_key_id_for(&owner_pub);
    let owner = prin("owner@acme.com");
    a.db()
        .seed_workspace_member(w, &owner, "owner", "confirmed")
        .await
        .unwrap();
    a.db()
        .seed_device(w, &owner_dk, &owner_pub, &owner, false, &cred(w, &owner_dk))
        .await
        .unwrap();
    (owner_seed, owner, owner_dk)
}

/// Owner-create an invite offering `skill` to `invitee`; return its opaque token.
pub(super) async fn make_invite(
    a: &Authority,
    w: &WorkspaceId,
    owner_seed: &[u8; 32],
    owner_dk: &str,
    op: &str,
    invitee: &str,
    skill_name: &str,
) -> String {
    let signed = sign_governance(
        owner_seed,
        w.as_str(),
        op,
        owner_dk,
        GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![prin(invitee)],
            skills: vec![(skill(skill_name), Some("Deploy".to_owned()))],
        },
    );
    match a.create_invite(w, op, signed, "t0", NOW).await.unwrap() {
        CreateInviteOutcome::Created(c) => token_of(&c.link),
        other => panic!("expected Created, got {other:?}"),
    }
}

/// Drive a CLOUD device flow to a grant: start → poll(Pending) → passcode → poll(Granted). `confirm_as`
/// is the email proven on the verification page (the grant's principal).
async fn cloud_flow_to_grant(
    a: &Authority,
    invite_token: &str,
    device_seed: &[u8; 32],
    confirm_as: &str,
) -> GrantIssued {
    let dpub = device_pub(device_seed);
    let start = a
        .start_device_auth(invite_token, &dpub, "laptop", NOW, "t0")
        .await
        .unwrap();
    assert!(matches!(
        a.poll_device_auth(&start.device_code, NOW, "t0")
            .await
            .unwrap(),
        DeviceAuthPoll::Pending
    ));
    let pc = a
        .start_passcode(&start.user_code, confirm_as, NOW, "t0")
        .await
        .unwrap();
    assert_eq!(
        a.complete_passcode(&start.user_code, confirm_as, &pc.passcode, NOW)
            .await
            .unwrap(),
        PasscodeComplete::Confirmed
    );
    match a
        .poll_device_auth(&start.device_code, NOW, "t0")
        .await
        .unwrap()
    {
        DeviceAuthPoll::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    }
}

/// Redeem a grant with the (honest) enrolling device. The grant IS the bearer credential; the server
/// checks the redeem body's `device_public_key` matches the grant's bound key (a binding check, not a
/// possession proof). `_device_seed` is vestigial now that nothing signs — kept so call sites stay stable.
pub(super) async fn redeem(
    a: &Authority,
    grant: &GrantIssued,
    _device_seed: &[u8; 32],
    dpub: [u8; 32],
) -> RedeemOutcome {
    a.redeem_enrollment(&grant.grant_token, dpub, NOW, "t0")
        .await
        .unwrap()
}

#[sqlx::test]
async fn verification_context_discloses_the_session_device_and_offered_skills(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-verify-ctx").await;
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

    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let start = a
        .start_device_auth(&invite, &dpub, "alice-laptop", NOW, "t0")
        .await
        .unwrap();

    // The verification page discloses the device + workspace + offered skills — no secret.
    let ctx = a
        .read_verification_context(&start.user_code, NOW)
        .await
        .unwrap();
    assert_eq!(ctx.machine_name, "alice-laptop");
    assert_eq!(ctx.workspace_display_name, "Acme");
    assert_eq!(ctx.verified_domain_status, "verified");
    assert_eq!(ctx.offered_skills.len(), 1);
    assert_eq!(ctx.offered_skills[0].0.as_str(), "s_deploy");
    assert_eq!(ctx.offered_skills[0].1.as_deref(), Some("Deploy"));
    // The fingerprint is the leading 16 hex of sha256(device pubkey) — no secret, no `dk_` prefix.
    let expected_fp = &digest::to_hex(&digest::sha256(&dpub))[..16];
    assert_eq!(ctx.device_fingerprint, expected_fp);
    assert!(!ctx.device_fingerprint.starts_with("dk_"));

    // An unknown user code is the single indistinguishable NotFound.
    assert!(matches!(
        a.read_verification_context("ZZZZ-ZZZZ", NOW).await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn confirm_external_identity_confirms_the_session_so_the_next_poll_grants(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-oidc-confirm").await;
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

    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let start = a
        .start_device_auth(&invite, &dpub, "laptop", NOW, "t0")
        .await
        .unwrap();
    // A cloud session starts pending → a poll is Pending until an identity is confirmed.
    assert!(matches!(
        a.poll_device_auth(&start.device_code, NOW, "t0")
            .await
            .unwrap(),
        DeviceAuthPoll::Pending
    ));

    // The OIDC callback proved the email out-of-band; confirm it directly (no passcode, no code check).
    assert_eq!(
        a.confirm_external_identity(&start.user_code, "alice@acme.com", NOW)
            .await
            .unwrap(),
        ConfirmOutcome::Confirmed
    );

    // The session is confirmed for alice → the next poll yields a grant bound to her, redeemable.
    let grant = match a
        .poll_device_auth(&start.device_code, NOW, "t0")
        .await
        .unwrap()
    {
        DeviceAuthPoll::Granted(g) => g,
        other => panic!("expected Granted, got {other:?}"),
    };
    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("expected a redeem");
    };
    assert_eq!(r.principal.as_str(), "alice@acme.com");

    // An unknown user code is the indistinguishable NotFound.
    assert!(matches!(
        a.confirm_external_identity("ZZZZ-ZZZZ", "alice@acme.com", NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn cloud_device_flow_to_redeem_mints_a_resolvable_credential(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-happy").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _owner, owner_dk) = seat_owner(a, &w, "cloud").await;
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

    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;
    assert_eq!(grant.device_key_id, device_key_id_for(&dpub)); // server-derived

    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("expected a redeem");
    };
    assert_eq!(r.principal.as_str(), "alice@acme.com");
    assert_eq!(r.device_key_id, device_key_id_for(&dpub));
    assert!(!r.credential.is_empty());
    // The redeem wrote the device's `credential_sha256` and confirmed alice's membership (the read gate),
    // so the minted workspace credential resolves a read scope on any of the workspace's skills.
    let scope = a
        .resolve_read_scope("w_acme", "s_deploy", &r.credential)
        .await
        .unwrap();
    assert_eq!(scope.ws().as_str(), "w_acme");
    assert_eq!(scope.skill().as_str(), "s_deploy");
}

#[sqlx::test]
async fn a_leaked_grant_redeemed_by_a_different_device_is_denied(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-leak").await;
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
    let device_seed = [11u8; 32];
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;

    // An attacker who stole the grant token but presents a DIFFERENT device key cannot redeem it: the
    // server checks the redeem body's device_public_key against the grant's bound key (the binding check
    // that replaced the possession proof).
    let attacker_seed = [99u8; 32];
    let attacker_pub = device_pub(&attacker_seed);
    let out = a
        .redeem_enrollment(&grant.grant_token, attacker_pub, NOW, "t0")
        .await
        .unwrap();
    assert!(matches!(out, RedeemOutcome::Denied(_)), "got {out:?}");
}

#[sqlx::test]
async fn redeem_replay_re_derives_the_identical_credential(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-replay").await;
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
    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;

    let RedeemOutcome::Redeemed(r1) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("first redeem");
    };
    let RedeemOutcome::Redeemed(r2) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("replay redeem");
    };
    // Deterministic: the replay re-derives the IDENTICAL workspace credential (derived per grant, so a
    // lost-ack retry re-returns the same value; the registry row's credential_sha256 is replaced in place).
    assert!(!r1.credential.is_empty());
    assert_eq!(r1.credential, r2.credential);
    // Both resolve a read scope (the same credential, the same confirmed member).
    assert!(
        a.resolve_read_scope("w_acme", "s_deploy", &r2.credential)
            .await
            .is_ok()
    );
}

#[sqlx::test]
async fn redeem_fails_closed_on_a_corrupt_stored_deployment_mode(pool: PgPool) {
    // The deployment mode decides the redeem GATE (cloud requires a rostered identity; self-host admits
    // the bearer). A corrupted/unknown stored mode must be an Integrity fault — never a fall-through to
    // the permissive self-host bearer semantics. The schema CHECK normally forbids such a row, so the
    // test drops it to simulate exactly the corruption (a bad restore, a slipped migration) the strict
    // parse is the defense against — matching start_device_auth/read_invite_bootstrap, which already
    // fail closed on it.
    let fx = Fixture::new(pool.clone(), "enr-mode-closed").await;
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
    let device_seed = [23u8; 32];
    let dpub = device_pub(&device_seed);
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;

    // Corrupt the stored mode AFTER the grant was issued, so the redeem is the first read of it.
    sqlx::query("ALTER TABLE workspace DROP CONSTRAINT workspace_deployment_mode_check")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE workspace SET deployment_mode = 'banana' WHERE workspace_id = $1")
        .bind(w.as_str())
        .execute(&pool)
        .await
        .unwrap();

    let err = a
        .redeem_enrollment(&grant.grant_token, dpub, NOW, "t0")
        .await
        .expect_err("a garbage stored mode must fail the redeem closed");
    assert!(
        matches!(err, AuthorityError::Integrity(_)),
        "an Integrity fault, never a bearer admission: {err:?}"
    );
    // Nothing was admitted: no device row, no read token, no self-host membership for alice.
    let devices = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM device_registry WHERE workspace_id = $1 AND device_key_id = $2",
    )
    .bind(w.as_str())
    .bind(&grant.device_key_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(devices, 0, "the failed redeem registered nothing");
}

#[sqlx::test]
async fn cloud_redeem_of_a_non_rostered_principal_is_denied(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-gate").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
    // The invite seeds ALICE onto the roster…
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
    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    // …but the device proves BOB (not on the roster) on the verification page.
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "bob@acme.com").await;
    let out = redeem(a, &grant, &device_seed, dpub).await;
    assert!(matches!(out, RedeemOutcome::Denied(_)), "got {out:?}");
}

#[sqlx::test]
async fn mixed_case_confirmed_principal_redeems_into_a_lowercase_invite_seat(pool: PgPool) {
    // THE BUG the canonical fold kills: the invite seats `alice@acme.com`, but the human proves the
    // MIXED-CASE `Alice@Acme.COM` on the verification page (an OIDC provider's casing, or just what
    // she typed). Pre-fix the byte-exact roster gate saw two identities — "invited but can't join".
    // The parse-boundary fold makes them ONE: the redeem succeeds and flips the SAME seat row.
    let fx = Fixture::new(pool.clone(), "enr-fold-redeem").await;
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

    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    // The confirm path receives the provider's casing verbatim; the op folds it internally.
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "Alice@Acme.COM").await;
    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("a mixed-case-confirmed principal must redeem into its lowercase seat");
    };
    assert_eq!(r.principal.as_str(), "alice@acme.com");
    // The one seat row flipped invited → confirmed — no second case-variant row was born.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT principal, status FROM workspace_member WHERE workspace_id = $1 AND role = 'member'",
    )
    .bind(w.as_str())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![("alice@acme.com".to_owned(), "confirmed".to_owned())]
    );
}

#[sqlx::test]
async fn a_mixed_case_invite_seats_the_canonical_row_and_a_lowercase_confirm_redeems_into_it(
    pool: PgPool,
) {
    // The reverse direction: the OWNER types the mixed case. `make_invite` parses the email at the
    // op edge (`prin` folds) so the request carries the FOLDED set — exactly what the fixed CLI sends —
    // the seat is born canonical, and a lowercase-proven identity lands in it.
    let fx = Fixture::new(pool.clone(), "enr-fold-invite").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
    let invite = make_invite(
        a,
        &w,
        &owner_seed,
        &owner_dk,
        &op_id(1),
        "Alice@Acme.COM",
        "s_deploy",
    )
    .await;

    // Born canonical: the stored seat carries the folded bytes, at `invited`.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT principal, status FROM workspace_member WHERE workspace_id = $1 AND role = 'member'",
    )
    .bind(w.as_str())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![("alice@acme.com".to_owned(), "invited".to_owned())]
    );

    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;
    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("a lowercase-confirmed principal must redeem into the folded seat");
    };
    assert_eq!(r.principal.as_str(), "alice@acme.com");
}

#[sqlx::test]
async fn self_host_redeem_grants_membership_without_smtp(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-selfhost").await;
    let a = &fx.authority;
    let w = ws("w_local");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "self_host").await;
    let invite = make_invite(
        a,
        &w,
        &owner_seed,
        &owner_dk,
        &op_id(1),
        "owner@acme.com",
        "s_deploy",
    )
    .await;

    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    // Self-host: the session is born confirmed (device-rooted principal); the first poll yields a grant.
    let start = a
        .start_device_auth(&invite, &dpub, "laptop", NOW, "t0")
        .await
        .unwrap();
    let grant = match a
        .poll_device_auth(&start.device_code, NOW, "t0")
        .await
        .unwrap()
    {
        DeviceAuthPoll::Granted(g) => g,
        other => panic!("expected Granted (no human step), got {other:?}"),
    };
    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("self-host redeem");
    };
    assert!(
        r.principal.as_str().starts_with("dev."),
        "device-rooted principal"
    );
    // Self-host granted the device-rooted principal membership, so its minted credential resolves a read
    // scope (the read gate — a confirmed member — is satisfied by the same redeem).
    assert!(!r.credential.is_empty());
    assert!(
        a.resolve_read_scope("w_local", "s_deploy", &r.credential)
            .await
            .is_ok()
    );
}

#[sqlx::test]
async fn revoke_device_stops_reads_and_refuses_later_device_ops(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-revoke").await;
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
    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let alice_dk = device_key_id_for(&dpub);
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;
    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("redeem");
    };
    let alice_cred = r.credential.clone();
    assert!(
        a.resolve_read_scope("w_acme", "s_deploy", &alice_cred)
            .await
            .is_ok()
    );

    // The owner revokes ALICE's device → its credential stops resolving (revoked=1; the read-lane resolver
    // requires a non-revoked row), so her reads instantly 404.
    let revoke = sign_governance(
        &owner_seed,
        w.as_str(),
        &op_id(2),
        &owner_dk,
        GovernanceOp::DeviceRevoke {
            target_device_key_id: alice_dk.clone(),
        },
    );
    assert_eq!(
        a.revoke_device(&w, &op_id(2), revoke, "t0", NOW).await.unwrap(),
        GovernanceOutcome::Ok
    );
    assert!(matches!(
        a.resolve_read_scope("w_acme", "s_deploy", &alice_cred)
            .await,
        Err(AuthorityError::NotFound)
    ));

    // The revoked device cannot RE-REDEEM its still-live grant to un-revoke itself (the kill switch is
    // durable, not undone within the grant TTL).
    assert!(
        matches!(
            redeem(a, &grant, &device_seed, dpub).await,
            RedeemOutcome::Denied(_)
        ),
        "a revoked device's re-redeem must be denied"
    );
    assert!(
        matches!(
            a.resolve_read_scope("w_acme", "s_deploy", &alice_cred)
                .await,
            Err(AuthorityError::NotFound)
        ),
        "the credential stays 404 — the denied re-redeem un-revoked nothing"
    );

    // The owner self-revokes its OWN device → a subsequent governance op from that device is refused.
    let self_revoke = sign_governance(
        &owner_seed,
        w.as_str(),
        &op_id(3),
        &owner_dk,
        GovernanceOp::DeviceRevoke {
            target_device_key_id: owner_dk.clone(),
        },
    );
    assert_eq!(
        a.revoke_device(&w, &op_id(3), self_revoke, "t0", NOW)
            .await
            .unwrap(),
        GovernanceOutcome::Ok
    );
    let after = sign_governance(
        &owner_seed,
        w.as_str(),
        &op_id(4),
        &owner_dk,
        GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![prin("carol@acme.com")],
            skills: vec![],
        },
    );
    let out = a.create_invite(&w, &op_id(4), after, "t0", NOW).await.unwrap();
    assert!(
        matches!(out, CreateInviteOutcome::Denied(_)),
        "revoked device refused: {out:?}"
    );
}

#[sqlx::test]
async fn roster_remove_revokes_the_members_reads(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-rosterremove").await;
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
    let device_seed = [13u8; 32];
    let dpub = device_pub(&device_seed);
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;
    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("redeem");
    };
    let alice_cred = r.credential.clone();
    assert!(
        a.resolve_read_scope("w_acme", "s_deploy", &alice_cred)
            .await
            .is_ok(),
        "alice can read before she is removed"
    );

    // The owner removes alice from the workspace roster (DELETE /v1/workspaces/{ws}/roster/{email}).
    let remove = sign_governance(
        &owner_seed,
        w.as_str(),
        &op_id(2),
        &owner_dk,
        GovernanceOp::RosterRemove {
            target: prin("alice@acme.com"),
        },
    );
    assert_eq!(
        a.roster_remove(&w, &op_id(2), remove, "t0", NOW).await.unwrap(),
        GovernanceOutcome::Ok
    );

    // Her read access is instantly revoked: the member row is gone, so although her credential still
    // resolves (the device is not revoked) the confirmed-member gate now denies — the read 404s, not a 403.
    assert!(
        matches!(
            a.resolve_read_scope("w_acme", "s_deploy", &alice_cred)
                .await,
            Err(AuthorityError::NotFound)
        ),
        "a removed member's reads must 404"
    );
}

/// The last-owner guard is a WRITE-SKEW, not a single-row race: two concurrent removals of DIFFERENT
/// owners each read the owner set `{owner1, owner2}` (count 2), each conclude they are not removing the
/// last owner, and each delete their target — zero owners, an orphaned workspace. A single-row `SELECT …
/// FOR UPDATE` on the target cannot catch it (the two targets are different rows); only `SERIALIZABLE`
/// (via the `run_serializable!` macro on every governance mutation) detects the mutual rw-antidependency,
/// aborts one with a serialization failure, and re-counts on the retry — where `would_orphan_owner` DENIES
/// it. So exactly one removal survives and one owner remains.
#[sqlx::test]
async fn concurrent_last_two_owner_removals_keep_one_owner(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-two-owner-race").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    // owner1 (seeded by seat_owner) + a second confirmed owner with its own device.
    let (owner1_seed, owner1, owner1_dk) = seat_owner(a, &w, "cloud").await;
    let owner2_seed = [9u8; 32];
    let owner2_pub = device_pub(&owner2_seed);
    let owner2_dk = device_key_id_for(&owner2_pub);
    let owner2 = prin("owner2@acme.com");
    a.db()
        .seed_workspace_member(&w, &owner2, "owner", "confirmed")
        .await
        .unwrap();
    a.db()
        .seed_device(
            &w,
            &owner2_dk,
            &owner2_pub,
            &owner2,
            false,
            &cred(&w, &owner2_dk),
        )
        .await
        .unwrap();

    // Each owner requests a removal of the OTHER — the write-skew.
    let remove_owner2 = sign_governance(
        &owner1_seed,
        w.as_str(),
        &op_id(2),
        &owner1_dk,
        GovernanceOp::RosterRemove {
            target: owner2.clone(),
        },
    );
    let remove_owner1 = sign_governance(
        &owner2_seed,
        w.as_str(),
        &op_id(3),
        &owner2_dk,
        GovernanceOp::RosterRemove {
            target: owner1.clone(),
        },
    );
    let (op2, op3) = (op_id(2), op_id(3));
    let (ra, rb) = tokio::join!(
        a.roster_remove(&w, &op2, remove_owner2, "t0", NOW),
        a.roster_remove(&w, &op3, remove_owner1, "t0", NOW),
    );
    let outcomes = [ra.unwrap(), rb.unwrap()];
    assert_eq!(
        outcomes
            .iter()
            .filter(|o| **o == GovernanceOutcome::Ok)
            .count(),
        1,
        "exactly one removal may succeed: {outcomes:?}"
    );
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, GovernanceOutcome::Denied(_))),
        "the other removal must be DENIED (it would orphan the workspace): {outcomes:?}"
    );
}

#[sqlx::test]
async fn a_members_governance_op_is_denied(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-rolematrix").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (_owner_seed, _o, _owner_dk) = seat_owner(a, &w, "cloud").await;
    // A confirmed MEMBER (not owner) with a registered device.
    let member_seed = [22u8; 32];
    let member_pub = device_pub(&member_seed);
    let member_dk = device_key_id_for(&member_pub);
    let member = prin("mary@acme.com");
    a.db()
        .seed_workspace_member(&w, &member, "member", "confirmed")
        .await
        .unwrap();
    a.db()
        .seed_device(
            &w,
            &member_dk,
            &member_pub,
            &member,
            false,
            &cred(&w, &member_dk),
        )
        .await
        .unwrap();

    let signed = sign_governance(
        &member_seed,
        w.as_str(),
        &op_id(9),
        &member_dk,
        GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![prin("x@acme.com")],
            skills: vec![],
        },
    );
    let out = a.create_invite(&w, &op_id(9), signed, "t0", NOW).await.unwrap();
    assert!(
        matches!(out, CreateInviteOutcome::Denied(_)),
        "member denied: {out:?}"
    );
}

#[sqlx::test]
async fn an_unauthenticated_governance_op_records_no_audit_row_and_cannot_squat_the_op_id(
    pool: PgPool,
) {
    let fx = Fixture::new(pool, "enr-noforge").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;

    // An attacker with NO registered device presents a governance op with a well-formed op_id.
    let attacker_seed = [99u8; 32];
    let forged = sign_governance(
        &attacker_seed,
        w.as_str(),
        &op_id(7),
        "dk_attacker_unregistered",
        GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![prin("x@acme.com")],
            skills: vec![],
        },
    );
    assert!(
        matches!(
            a.create_invite(&w, &op_id(7), forged, "t0", NOW).await.unwrap(),
            CreateInviteOutcome::Denied(_)
        ),
        "an unknown device is denied (pre-authentication)"
    );

    // The SAME op_id, now from the LEGIT owner, SUCCEEDS — the pre-authentication failure wrote no durable
    // workspace_events row, so it neither forged an audit entry nor squatted the op_id as an idempotency block.
    let legit = sign_governance(
        &owner_seed,
        w.as_str(),
        &op_id(7),
        &owner_dk,
        GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![prin("y@acme.com")],
            skills: vec![],
        },
    );
    assert!(
        matches!(
            a.create_invite(&w, &op_id(7), legit, "t0", NOW).await.unwrap(),
            CreateInviteOutcome::Created(_)
        ),
        "the legit owner's op with the same op_id is not blocked by a forged pre-auth row"
    );
}

#[sqlx::test]
async fn create_invite_is_op_id_idempotent_with_an_identical_link(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-idem").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
    let op = op_id(6);
    let mk = || {
        sign_governance(
            &owner_seed,
            w.as_str(),
            &op,
            &owner_dk,
            GovernanceOp::Invite {
                role: Role::Member,
                expires_at: None,
                emails: vec![prin("alice@acme.com")],
                skills: vec![(skill("s_deploy"), None)],
            },
        )
    };
    let CreateInviteOutcome::Created(c1) = a.create_invite(&w, &op, mk(), "t0", NOW).await.unwrap()
    else {
        panic!("first create");
    };
    let CreateInviteOutcome::Created(c2) = a.create_invite(&w, &op, mk(), "t0", NOW).await.unwrap()
    else {
        panic!("replay create");
    };
    assert_eq!(
        c1.link, c2.link,
        "the deterministic link replays identically"
    );
}

/// Governance op-id key-reuse: the audit/idempotency slot belongs to the FIRST request under an op id.
/// A same-op_id retry whose request payload DIVERGES (a different invitee) is a denied key-reuse, never a
/// second apply. The request identity is now a server-side canonical encoding of the request fields (the
/// signed frame is gone) — so this is behaviorally identical to the pre-de-crypto identity from the
/// test's view. (A same-op_id retry with the IDENTICAL request replays; that is pinned by
/// `create_invite_is_op_id_idempotent_with_an_identical_link`.)
#[sqlx::test]
async fn create_invite_op_id_reuse_with_a_divergent_request_is_denied(pool: PgPool) {
    let fx = Fixture::new(pool.clone(), "enr-opid-reuse").await;
    let a = &fx.authority;
    let w = ws("w_acme");
    let (owner_seed, _o, owner_dk) = seat_owner(a, &w, "cloud").await;
    let op = op_id(6);

    // First: op id X invites alice → Created.
    let first = sign_governance(
        &owner_seed,
        w.as_str(),
        &op,
        &owner_dk,
        GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![prin("alice@acme.com")],
            skills: vec![(skill("s_deploy"), None)],
        },
    );
    assert!(matches!(
        a.create_invite(&w, &op, first, "t0", NOW).await.unwrap(),
        CreateInviteOutcome::Created(_)
    ));

    // The SAME op id X with a DIVERGENT request (a different invitee) is a denied key-reuse — the slot
    // belongs to the first request, and nothing is re-applied.
    let divergent = sign_governance(
        &owner_seed,
        w.as_str(),
        &op,
        &owner_dk,
        GovernanceOp::Invite {
            role: Role::Member,
            expires_at: None,
            emails: vec![prin("carol@acme.com")],
            skills: vec![(skill("s_deploy"), None)],
        },
    );
    assert!(matches!(
        a.create_invite(&w, &op, divergent, "t0", NOW).await.unwrap(),
        CreateInviteOutcome::Denied(_)
    ));

    // carol was never seated — the denied key-reuse applied nothing.
    let carol = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM workspace_member WHERE workspace_id = $1 AND principal = 'carol@acme.com'",
    )
    .bind(w.as_str())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(carol, 0, "the denied key-reuse seated nobody");
}

#[sqlx::test]
async fn passcode_locks_after_the_attempt_cap(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-brute").await;
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
    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let start = a
        .start_device_auth(&invite, &dpub, "laptop", NOW, "t0")
        .await
        .unwrap();
    let pc = a
        .start_passcode(&start.user_code, "alice@acme.com", NOW, "t0")
        .await
        .unwrap();
    // A guaranteed-wrong guess (differs from the real code).
    let wrong = if pc.passcode == "000000" {
        "000001"
    } else {
        "000000"
    };
    for _ in 0..5 {
        let r = a
            .complete_passcode(&start.user_code, "alice@acme.com", wrong, NOW)
            .await
            .unwrap();
        assert!(matches!(r, PasscodeComplete::WrongCode { .. }), "got {r:?}");
    }
    // The cap is now hit — further attempts (even the RIGHT code) are locked out.
    assert_eq!(
        a.complete_passcode(&start.user_code, "alice@acme.com", &pc.passcode, NOW)
            .await
            .unwrap(),
        PasscodeComplete::TooManyAttempts
    );
}

#[sqlx::test]
async fn a_confirmed_same_principal_passcode_replay_stays_confirmed(pool: PgPool) {
    // First-writer-wins replay: once a session is confirmed for a principal, a SAME-principal retry or
    // refresh replays Confirmed BEFORE any passcode-row consultation — the row's later fate (expired,
    // deleted) must never turn a lost-ack retry into a WrongCode/Expired failure.
    let fx = Fixture::new(pool.clone(), "enr-replay").await;
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
    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let start = a
        .start_device_auth(&invite, &dpub, "laptop", NOW, "t0")
        .await
        .unwrap();
    let pc = a
        .start_passcode(&start.user_code, "alice@acme.com", NOW, "t0")
        .await
        .unwrap();
    assert_eq!(
        a.complete_passcode(&start.user_code, "alice@acme.com", &pc.passcode, NOW)
            .await
            .unwrap(),
        PasscodeComplete::Confirmed
    );

    // Replay AFTER the passcode row expired (10-min TTL; the 15-min session is still live) ⇒ Confirmed.
    assert_eq!(
        a.complete_passcode(
            &start.user_code,
            "alice@acme.com",
            &pc.passcode,
            NOW + 11 * 60 * 1000,
        )
        .await
        .unwrap(),
        PasscodeComplete::Confirmed
    );
    // Replay with the passcode row GONE entirely ⇒ still Confirmed (never a WrongCode).
    sqlx::query("DELETE FROM passcodes")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        a.complete_passcode(&start.user_code, "alice@acme.com", &pc.passcode, NOW)
            .await
            .unwrap(),
        PasscodeComplete::Confirmed
    );
    // The early replay sits BEHIND the different-principal miss: bob still gets the uniform NotFound.
    assert!(matches!(
        a.complete_passcode(&start.user_code, "bob@acme.com", &pc.passcode, NOW)
            .await,
        Err(AuthorityError::NotFound)
    ));
}

#[sqlx::test]
async fn device_key_id_is_server_derived_not_client_asserted(pool: PgPool) {
    let fx = Fixture::new(pool, "enr-dk").await;
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
    let device_seed = [11u8; 32];
    let dpub = device_pub(&device_seed);
    let grant = cloud_flow_to_grant(a, &invite, &device_seed, "alice@acme.com").await;
    // The id the server bound is purely a function of the public key.
    assert_eq!(grant.device_key_id, device_key_id_for(&dpub));
    let RedeemOutcome::Redeemed(r) = redeem(a, &grant, &device_seed, dpub).await else {
        panic!("redeem");
    };
    assert_eq!(r.device_key_id, device_key_id_for(&dpub));

    // Presenting a DIFFERENT key (whose server-derived id ≠ the grant's binding) is denied.
    let other_seed = [55u8; 32];
    let other_pub = device_pub(&other_seed);
    let out = a
        .redeem_enrollment(&grant.grant_token, other_pub, NOW, "t0")
        .await
        .unwrap();
    assert!(matches!(out, RedeemOutcome::Denied(_)), "got {out:?}");
}

#[sqlx::test]
async fn admin_claim_stands_up_a_workspace_and_replays_only_for_the_same_device(pool: PgPool) {
    // A SELF-HOST plane: the claim seats a device-rooted owner and the workspace takes the PLANE's mode.
    let fx = Fixture::with_mode(pool, "enr-admin", DeploymentMode::SelfHost).await;
    let a = &fx.authority;
    let w = ws("w_local");
    a.db().seed_admin_claim(&w, "claim-secret").await.unwrap();
    let device_seed = [33u8; 32];
    let dpub = device_pub(&device_seed);

    let RedeemOutcome::Redeemed(r) = a
        .admin_claim("claim-secret", dpub, NOW, "t0")
        .await
        .unwrap()
    else {
        panic!("admin claim");
    };
    assert_eq!(r.workspace_id.as_str(), "w_local");
    assert!(r.principal.as_str().starts_with("dev."));
    let created = a.db().read_workspace(&w).await.unwrap().expect("workspace");
    assert_eq!(created.deployment_mode, "self_host");

    // The SAME device's replay (a lost-200 retry) deterministically re-returns Redeemed.
    let RedeemOutcome::Redeemed(replay) = a
        .admin_claim("claim-secret", dpub, NOW, "t0")
        .await
        .unwrap()
    else {
        panic!("same-device replay must be Redeemed");
    };
    assert_eq!(replay.workspace_id.as_str(), "w_local");
    assert_eq!(replay.principal.as_str(), r.principal.as_str());

    // A DIFFERENT device presenting the consumed token is denied.
    let other = device_pub(&[44u8; 32]);
    let again = a
        .admin_claim("claim-secret", other, NOW, "t0")
        .await
        .unwrap();
    assert!(matches!(again, RedeemOutcome::Denied(_)));
}

#[sqlx::test]
async fn the_workspace_creation_cap_folds_case_variants_into_one_identity(pool: PgPool) {
    // The owned-workspace cap (3 confirmed-owner seats) counts the MAILBOX, not the casing: three
    // creates for `robert@x.io` exhaust the cap for `Robert@X.io` too — a case variant is the same
    // identity at the genesis door, never a cap bypass. (The cap mechanics themselves are the standup
    // suite's; this is the canonical-principal witness on that gate.)
    let fx = Fixture::new(pool, "enr-fold-cap").await;
    let a = &fx.authority;
    for n in 0..3 {
        let out = a
            .create_workspace(
                &format!("req-{n}"),
                None,
                "robert@x.io",
                DeploymentMode::Cloud,
                "t0",
            )
            .await
            .unwrap();
        assert!(
            matches!(out, CreateWorkspaceOutcome::Created(_)),
            "create {n}: {out:?}"
        );
    }
    assert!(matches!(
        a.create_workspace("req-3", None, "Robert@X.io", DeploymentMode::Cloud, "t0")
            .await
            .unwrap(),
        CreateWorkspaceOutcome::Denied("workspace creation limit reached")
    ));
}
