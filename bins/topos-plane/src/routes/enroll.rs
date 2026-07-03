//! The enrollment routes — the device-authorization flow, the passcode second factor, the central grant
//! redeem, and the self-host admin-claim. None of these is device-op-signed EXCEPT redeem, which carries the
//! enrollment **possession** signature in the `Topos-Device-Signature` header. Every handler is THIN: parse
//! the wire DTO/headers → call ONE authority op → serialize. A confirmed identity is NEVER `Principal::parse`d
//! here — it is resolved from a server-trusted row inside the authority.
//!
//! Read-shaped steps (authorize / token / verify / passcode / confirm) return a plain typed DTO and reserve
//! 404 for the single indistinguishable not-found (a dead invite, an unknown code, a non-live session). The
//! op_id-less WRITES (redeem / admin-claim) return a 200 all-outcome envelope: `OK` carries the data,
//! `DENIED` the uniform flat error (never a 403).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use topos_types::requests::{
    AdminClaimRequest, DeviceAuthorizeRequest, DeviceTokenRequest, PasscodeAck, PasscodeAckStatus,
    PasscodeConfirmRequest, PasscodeRequest, RedeemRequest, SessionIntent,
};

use crate::enroll::mailer::{MailContext, Passcode};
use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/device/authorize",
    tag = "enrollment",
    request_body = DeviceAuthorizeRequest,
    responses(
        (status = 200, description = "The device-authorization grant (device_code / user_code / verification_uri(_complete); a standup start also carries the plane block to TOFU-pin).", body = topos_types::requests::DeviceAuthorizeResponse),
        (status = 400, description = "Malformed body, device public key, or intent/invite combination.", body = topos_types::JsonEnvelope),
        (status = 404, description = "No such invite (or it is revoked/expired), or a standup start on a plane that does not offer it.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn start_device_auth(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<DeviceAuthorizeRequest>,
) -> Result<Response, PlaneHttpError> {
    let device_public_key = wire::base64url_key(&req.device_public_key)?;
    let (created_at, now) = wire::now_utc();
    // Dispatch on (intent, invite_token): an explicit intent must be consistent with the invite's presence
    // (fail closed on a contradictory body); an absent intent defaults from the invite's presence.
    match (req.intent, req.invite_token.as_deref()) {
        // ENROLL: the invite-anchored flow (the pre-existing path, byte-identical response fields plus the
        // additive complete URI).
        (Some(SessionIntent::Enroll) | None, Some(invite_token)) => {
            let start = state
                .authority()
                .start_device_auth(
                    invite_token,
                    &device_public_key,
                    &req.machine_name,
                    now,
                    &created_at,
                )
                .await?;
            Ok(Json(map::device_auth_to_wire(start, now, None)).into_response())
        }
        // STANDUP: no invite — the session is born workspace-less; the response carries the plane block
        // (base URL + posture + the signing key to TOFU-pin), which an enroll start leaves to `/i/`.
        (Some(SessionIntent::Standup) | None, None) => {
            let start = state
                .authority()
                .start_standup_device_auth(&device_public_key, &req.machine_name, now, &created_at)
                .await?;
            let plane = map::standup_plane_block(&state)?;
            Ok(Json(map::device_auth_to_wire(start, now, Some(plane))).into_response())
        }
        (Some(SessionIntent::Standup), Some(_)) => Err(PlaneHttpError::BadBody(
            "a standup start takes no invite_token".to_owned(),
        )),
        (Some(SessionIntent::Enroll), None) => Err(PlaneHttpError::BadBody(
            "an enroll start requires an invite_token".to_owned(),
        )),
    }
}

#[utoipa::path(
    post,
    path = "/v1/device/token",
    tag = "enrollment",
    request_body = DeviceTokenRequest,
    responses(
        (status = 200, description = "The poll status (pending/slow_down/denied/expired/granted + the grant on granted).", body = topos_types::requests::DeviceTokenResponse),
        (status = 400, description = "Malformed body.", body = topos_types::JsonEnvelope),
        (status = 404, description = "No such device code.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn poll_device_auth(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<DeviceTokenRequest>,
) -> Result<Response, PlaneHttpError> {
    let (created_at, now) = wire::now_utc();
    let poll = state
        .authority()
        .poll_device_auth(&req.device_code, now, &created_at)
        .await?;
    Ok(Json(map::device_poll_to_wire(poll)).into_response())
}

#[utoipa::path(
    get,
    path = "/v1/enroll/verify/{user_code}",
    tag = "enrollment",
    params(("user_code" = String, Path, description = "The user code shown by `device/authorize`.")),
    responses(
        (status = 200, description = "The verification-page disclosure (machine name, fingerprint, workspace, offered skills).", body = topos_types::requests::VerificationContextResponse),
        (status = 404, description = "No live session for that user code.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn read_verification_context(
    State(state): State<PlaneState>,
    Path(user_code): Path<String>,
) -> Result<Response, PlaneHttpError> {
    let now = wire::now_utc().1;
    let context = state
        .authority()
        .read_verification_context(&user_code, now)
        .await?;
    Ok(Json(map::verification_to_wire(context)).into_response())
}

#[utoipa::path(
    post,
    path = "/v1/enroll/passcode",
    tag = "enrollment",
    request_body = PasscodeRequest,
    responses(
        (status = 200, description = "A constant-shaped ack (the send is fire-and-forget; no enumeration oracle).", body = topos_types::requests::PasscodeAck),
        (status = 400, description = "Malformed body.", body = topos_types::JsonEnvelope),
        (status = 404, description = "No live session for that user code.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn start_passcode(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<PasscodeRequest>,
) -> Result<Response, PlaneHttpError> {
    let (created_at, now) = wire::now_utc();
    // The verification context supplies the workspace name for the email body (and confirms the session is
    // live — the same indistinguishable 404 the passcode start would give for a non-live user code).
    let context = state
        .authority()
        .read_verification_context(&req.user_code, now)
        .await?;
    // The email is parsed INSIDE the authority op (never a handler `Principal::parse`); a constant-shaped ack
    // means a non-rostered address is no enumeration oracle (the cloud gate is enforced at redeem).
    let started = state
        .authority()
        .start_passcode(&req.user_code, &req.email, now, &created_at)
        .await?;

    // Fire-and-forget the blocking SMTP send on `spawn_blocking`: spawn it, drop the handle, return the ack
    // immediately — so neither the response body nor its latency leaks whether the address was rostered.
    let mailer = state.mailer().clone();
    let to = req.email.clone();
    let ctx = MailContext {
        workspace_display_name: context.workspace_display_name,
        verify_base_url: state.enroll().verify_base_url.clone(),
    };
    let code = Passcode::new(started.passcode);
    tokio::task::spawn_blocking(move || {
        // The send result is intentionally dropped — a failure is never surfaced (no oracle, no latency leak).
        let _ = mailer.send_passcode(&to, &code, &ctx);
    });

    Ok(Json(PasscodeAck {
        status: PasscodeAckStatus::Sent,
    })
    .into_response())
}

#[utoipa::path(
    post,
    path = "/v1/enroll/passcode/confirm",
    tag = "enrollment",
    request_body = PasscodeConfirmRequest,
    responses(
        (status = 200, description = "The confirmation status (confirmed/wrong_code/expired/too_many_attempts).", body = topos_types::requests::PasscodeConfirmResponse),
        (status = 400, description = "Malformed body.", body = topos_types::JsonEnvelope),
        (status = 404, description = "No live session for that user code.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn complete_passcode(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<PasscodeConfirmRequest>,
) -> Result<Response, PlaneHttpError> {
    let now = wire::now_utc().1;
    let outcome = state
        .authority()
        .complete_passcode(&req.user_code, &req.email, &req.code, now)
        .await?;
    Ok(Json(map::passcode_complete_to_wire(outcome)).into_response())
}

#[utoipa::path(
    post,
    path = "/v1/workspaces/{ws}/devices",
    tag = "enrollment",
    request_body = RedeemRequest,
    params(
        ("ws" = String, Path, description = "Workspace id (the grant is authoritative for the workspace)."),
        ("Topos-Device-Signature" = String, Header, description = "base64url(64-byte Ed25519 enrollment-possession signature), 86 chars"),
    ),
    responses(
        (status = 200, description = "The redeem receipt — OK carries the registered device + minted read creds; DENIED the flat error.", body = topos_types::JsonEnvelope),
        (status = 400, description = "Malformed body, key, or signature header.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn redeem(
    State(state): State<PlaneState>,
    // The path `{ws}` is REST sugar; the grant is the authoritative source of the workspace.
    Path(_ws): Path<String>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<RedeemRequest>,
) -> Result<Response, PlaneHttpError> {
    // The enrollment possession proof rides the header (NOT the body), reusing the device-signature parser.
    let enroll_sig = wire::device_signature(&headers)?;
    let device_public_key = wire::base64url_key(&req.device_public_key)?;
    let (created_at, now) = wire::now_utc();
    let outcome = state
        .authority()
        .redeem_enrollment(&req.grant, &enroll_sig, device_public_key, now, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::redeem_envelope("redeem", outcome)),
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/v1/admin-claim",
    tag = "enrollment",
    request_body = AdminClaimRequest,
    responses(
        (status = 200, description = "The admin-claim receipt — OK stands up the workspace + seats the first owner; DENIED the flat error.", body = topos_types::JsonEnvelope),
        (status = 400, description = "Malformed body or device public key.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn admin_claim(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<AdminClaimRequest>,
) -> Result<Response, PlaneHttpError> {
    let device_public_key = wire::base64url_key(&req.device_public_key)?;
    let (created_at, now) = wire::now_utc();
    // `req.display_name` is DISCLOSURE-ONLY (what the agent showed its human): the seated workspace's name
    // comes from the claim row minted server-side, so an adversarial body cannot rename it.
    let _ = &req.display_name;
    let outcome = state
        .authority()
        .admin_claim(&req.claim_token, device_public_key, now, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::redeem_envelope("admin-claim", outcome)),
    )
        .into_response())
}
