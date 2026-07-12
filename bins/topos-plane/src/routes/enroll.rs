//! The enrollment routes — the device-authorization flow, the passcode second factor, the central grant
//! redeem, and the self-host admin-claim. The GRANT is the bearer credential; the redeem presents it in the
//! body (with the device public key it must match) — no signature. Every handler is THIN: parse the wire DTO
//! → call ONE authority op → serialize. A confirmed identity is NEVER `Principal::parse`d here — it is
//! resolved from a server-trusted row inside the authority.
//!
//! Read-shaped steps (authorize / token / verify / passcode / confirm) return a plain typed DTO and reserve
//! 404 for the single indistinguishable not-found (a dead invite, an unknown code, a non-live session). The
//! op_id-less WRITES (redeem / admin-claim) return a 200 all-outcome envelope: `OK` carries the data,
//! `DENIED` the uniform flat error (never a 403).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
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
        (status = 200, description = "The device-authorization grant (device_code / user_code / verification_uri(_complete)) plus the plane block (API base + posture + method, no trust root) every start now carries.", body = topos_types::requests::DeviceAuthorizeResponse),
        (status = 400, description = "Malformed body, device public key, workspace name, or intent/workspace combination.", body = topos_types::JsonEnvelope),
        (status = 404, description = "A standup start on a plane that does not offer it (self-host).", body = topos_types::JsonEnvelope),
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
    // Dispatch on (intent, workspace): an explicit intent must be consistent with the workspace's presence
    // (fail closed on a contradictory body); an absent intent defaults from the workspace's presence
    // (a named workspace ⇒ enroll; none ⇒ standup). Every start carries the plane block for parity.
    let start = match (req.intent, req.workspace.as_deref()) {
        // ENROLL: toward a workspace ADDRESS name (resolution is never disclosed here — an unknown name
        // runs the same flow to the redeem's one uniform denial; a malformed name is the syntax 400).
        (Some(SessionIntent::Enroll) | None, Some(workspace)) => {
            state
                .authority()
                .start_device_auth(
                    workspace,
                    &device_public_key,
                    &req.machine_name,
                    now,
                    &created_at,
                )
                .await?
        }
        // STANDUP: no workspace — the session is born workspace-less; a signed-in human's approval creates
        // one (cloud only; self-host ⇒ the uniform 404).
        (Some(SessionIntent::Standup) | None, None) => {
            state
                .authority()
                .start_standup_device_auth(&device_public_key, &req.machine_name, now, &created_at)
                .await?
        }
        // LOGIN: no workspace — the session proves the person's identity; its grant redeems at
        // `POST /v1/login` into one credential per confirmed seat (allowed on BOTH postures).
        (Some(SessionIntent::Login), None) => {
            state
                .authority()
                .start_login_device_auth(&device_public_key, &req.machine_name, now, &created_at)
                .await?
        }
        (Some(SessionIntent::Enroll), None) => {
            return Err(PlaneHttpError::BadBody(
                "an enroll start requires a workspace".to_owned(),
            ));
        }
        (Some(SessionIntent::Standup), Some(_)) => {
            return Err(PlaneHttpError::BadBody(
                "a standup start takes no workspace".to_owned(),
            ));
        }
        (Some(SessionIntent::Login), Some(_)) => {
            return Err(PlaneHttpError::BadBody(
                "a login start takes no workspace".to_owned(),
            ));
        }
    };
    let plane = map::plane_block(&state)?;
    Ok(Json(map::device_auth_to_wire(start, now, Some(plane))).into_response())
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
    ),
    responses(
        (status = 200, description = "The redeem receipt — OK carries the registered device + minted read creds; DENIED the flat error.", body = topos_types::JsonEnvelope),
        (status = 400, description = "Malformed body or key.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn redeem(
    State(state): State<PlaneState>,
    // The path `{ws}` scopes the redeem; the redeem's membership gate checks it against the grant's own
    // workspace (a wrong-workspace redeem is the ONE uniform denial, never an existence oracle).
    Path(ws): Path<String>,
    ApiJson(req): ApiJson<RedeemRequest>,
) -> Result<Response, PlaneHttpError> {
    let ws =
        plane_store::WorkspaceId::parse(&ws).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    // The grant is the bearer credential; the server checks the body's device public key matches the key the
    // grant is bound to (binding consistency, not a possession proof).
    let device_public_key = wire::base64url_key(&req.device_public_key)?;
    let now = wire::now_utc().1;
    let outcome = state
        .authority()
        .redeem_enrollment(&ws, &req.grant, device_public_key, now)
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
