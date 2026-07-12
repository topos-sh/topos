//! `POST /v1/login` — redeem a LOGIN grant. The grant (from a `login`-intent device flow) is the bearer
//! credential; the body presents the device public key it is bound to (a binding check, no signature). One
//! transaction re-mints this device's workspace credential in EVERY workspace where the proven identity
//! holds a confirmed seat — a `blocked` marker where a mint was refused (revoked / key-squatted). ZERO seats
//! is a valid empty success. THIN: parse → one authority op → the all-outcome envelope.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use topos_types::requests::LoginRedeemRequest;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/login",
    tag = "enrollment",
    request_body = LoginRedeemRequest,
    responses(
        (status = 200, description = "The login receipt — OK carries the LoginData (the proven identity + one re-minted credential, or a blocked marker, per confirmed seat); DENIED the flat error.", body = topos_types::JsonEnvelope),
        (status = 400, description = "Malformed body or device public key.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn login(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<LoginRedeemRequest>,
) -> Result<Response, PlaneHttpError> {
    // The grant is the bearer credential; the server checks the presented device public key matches the key
    // the grant is bound to (binding consistency, not a possession proof).
    let device_public_key = wire::base64url_key(&req.device_public_key)?;
    let now = wire::now_utc().1;
    let outcome = state
        .authority()
        .redeem_login(&req.grant, device_public_key, now)
        .await?;
    Ok((StatusCode::OK, Json(map::login_envelope(outcome))).into_response())
}
