//! `POST /v1/workspaces/{ws}/invitations` — invitation as a member-lane ROSTER WRITE. The seats are the
//! invitation; the response carries the workspace ADDRESS and the honest `mailed` flag (true only when a
//! real relay is configured). An unknown channel is a 200 DENIED with nothing written.

use std::sync::Arc;

use topos_types::requests::InvitationData;

use super::*;

/// With a mailer configured (the FakeMailer stands in for a real relay), an invitation seats the email,
/// returns the workspace ADDRESS, reports `mailed: true`, and the mail is actually sent (recorded).
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_invitation_seats_the_email_returns_the_address_and_mails(pool: PgPool) {
    let ctx = enroll_setup(pool, "invite-ok").await;
    let body = serde_json::json!({ "emails": [ALICE_EMAIL] });
    let (status, _, bytes) = send(
        ctx.app(),
        req_json_auth(
            "POST",
            &format!("/v1/workspaces/{WS}/invitations"),
            body,
            OWNER_CRED,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(env.ok, "an invitation is ok: {env:?}");
    assert_eq!(env.command, "invite");
    let data: InvitationData = serde_json::from_value(env.data).expect("OK data is InvitationData");
    assert!(
        data.address.ends_with(&format!("/{WS_NAME}")),
        "the address is the workspace slug under the link base: {}",
        data.address
    );
    assert!(data.invited.iter().any(|e| e == ALICE_EMAIL));
    assert!(data.mailed, "a configured mailer reports mailed: true");

    // The mail was actually sent — the FakeMailer recorded the recipient + the disclosed address.
    let mailed = wait_for_invitation(&ctx.fake);
    assert_eq!(mailed.to, ALICE_EMAIL);
    assert_eq!(mailed.address, data.address);
}

/// With NO relay (the self-host default `NoopMailer`), the same invitation still seats the email but
/// reports `mailed: false` honestly (the inviter pastes the address by hand).
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_invitation_without_a_relay_reports_mailed_false(pool: PgPool) {
    let ctx = enroll_setup(pool, "invite-noop").await;
    // A state over the SAME authority but with the silent NoopMailer (no relay configured).
    let noop_state = ctx
        .state
        .clone()
        .with_mailer(Arc::new(crate::enroll::mailer::NoopMailer));
    let body = serde_json::json!({ "emails": [ALICE_EMAIL] });
    let (status, _, bytes) = send(
        router(noop_state),
        req_json_auth(
            "POST",
            &format!("/v1/workspaces/{WS}/invitations"),
            body,
            OWNER_CRED,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data: InvitationData =
        serde_json::from_value(envelope(&bytes).data).expect("OK data is InvitationData");
    assert!(data.invited.iter().any(|e| e == ALICE_EMAIL));
    assert!(!data.mailed, "no relay reports mailed: false");
}

/// An unknown pre-placement channel is a 200 DENIED (`UNKNOWN_CHANNEL`) with nothing written — resolve-all-
/// or-apply-none, and no mail.
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_invitation_into_an_unknown_channel_is_denied(pool: PgPool) {
    let ctx = enroll_setup(pool, "invite-unknown-ch").await;
    let body = serde_json::json!({ "emails": [ALICE_EMAIL], "channels": ["no-such-channel"] });
    let (status, _, bytes) = send(
        ctx.app(),
        req_json_auth(
            "POST",
            &format!("/v1/workspaces/{WS}/invitations"),
            body,
            OWNER_CRED,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let env = envelope(&bytes);
    assert!(!env.ok, "an unknown channel is denied: {env:?}");
    assert_eq!(
        env.error.expect("DENIED carries a WireError").code,
        "UNKNOWN_CHANNEL"
    );
    // Nothing was mailed (the seat was never written).
    assert!(ctx.fake.invitations().is_empty());
}

/// A missing/blank credential is the uniform 404 (never an existence oracle).
#[sqlx::test(migrator = "plane_store::MIGRATOR")]
async fn an_invitation_with_no_credential_is_the_uniform_404(pool: PgPool) {
    let ctx = enroll_setup(pool, "invite-404").await;
    let body = serde_json::json!({ "emails": [ALICE_EMAIL] });
    let (status, _, _) = send(
        ctx.app(),
        post_nosig(&format!("/v1/workspaces/{WS}/invitations"), body),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// Block (briefly) until the fire-and-forget invitation send lands in the `FakeMailer`.
fn wait_for_invitation(
    fake: &crate::enroll::mailer::FakeMailer,
) -> crate::enroll::mailer::SentInvitation {
    for _ in 0..200 {
        if let Some(m) = fake.invitations().into_iter().next() {
            return m;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    panic!("no invitation mailed within the timeout");
}
