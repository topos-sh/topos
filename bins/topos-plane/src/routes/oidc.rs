//! The OIDC enrollment routes — behind `enroll-oidc` (DEFAULT-OFF), so they are absent (and the committed
//! OpenAPI contract excludes them) in a default build. Two thin pass-throughs over the connector
//! ([`crate::enroll::oidc`]): `start` builds the IdP authorize redirect (returning the PKCE/CSRF/nonce flow
//! secrets for the composing layer to persist), and `callback` runs the SERVER-SIDE exchange + id_token
//! validation + session confirm. **No user token ever returns to the agent** — the connector drops it; only
//! the proven email crosses into the authority's session confirm.
//!
//! The OIDC request/response DTOs live here (feature-local), not in `topos-types`: they are not part of the
//! committed cross-language contract (the feature is default-off), so they need only `serde`.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use plane_store::AuthorityError;
use serde::{Deserialize, Serialize};

use crate::enroll::oidc::{self, EnrollError, OidcCallback};
use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson};

/// `POST /v1/enroll/oidc/start` body — begin an OIDC login for a live device-auth `user_code`.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OidcStartRequest {
    /// The user code naming the live device-auth session the login confirms.
    pub(crate) user_code: String,
}

/// `POST /v1/enroll/oidc/start` response — the authorize URL plus the flow secrets the caller MUST persist
/// (keyed to the browser flow) and present back to `callback`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OidcStartResponse {
    pub(crate) authorize_url: String,
    pub(crate) state: String,
    pub(crate) pkce_verifier: String,
    pub(crate) nonce: String,
}

/// `POST /v1/enroll/oidc/callback` body — the IdP's `(code, returned_state)` plus the persisted flow secrets.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct OidcCallbackRequest {
    pub(crate) code: String,
    pub(crate) returned_state: String,
    pub(crate) expected_state: String,
    pub(crate) pkce_verifier: String,
    pub(crate) nonce: String,
}

/// `POST /v1/enroll/oidc/callback` response — a constant-shaped confirm (no token, no claim).
#[derive(Debug, Clone, Serialize)]
pub(crate) struct OidcCallbackResponse {
    pub(crate) status: &'static str,
}

pub(crate) async fn start(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<OidcStartRequest>,
) -> Result<Response, PlaneHttpError> {
    // No connector configured ⇒ the indistinguishable not-found (never reveal the OIDC config state).
    let cfg = state
        .oidc()
        .ok_or(PlaneHttpError::Authority(AuthorityError::NotFound))?;
    let now = wire::now_utc().1;
    let redirect = oidc::start(cfg, &req.user_code, now)
        .await
        .map_err(enroll_error)?;
    Ok(Json(OidcStartResponse {
        authorize_url: redirect.url,
        state: redirect.state,
        pkce_verifier: redirect.pkce_verifier,
        nonce: redirect.nonce,
    })
    .into_response())
}

pub(crate) async fn callback(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<OidcCallbackRequest>,
) -> Result<Response, PlaneHttpError> {
    let cfg = state
        .oidc()
        .ok_or(PlaneHttpError::Authority(AuthorityError::NotFound))?;
    let now = wire::now_utc().1;
    let params = OidcCallback {
        code: req.code,
        returned_state: req.returned_state,
        expected_state: req.expected_state,
        pkce_verifier: req.pkce_verifier,
        nonce: req.nonce,
    };
    // The connector validates state → exchanges the code → validates the id_token → confirms the session. The
    // id/access token is consumed + dropped inside; only `()` returns (a regression test pins that).
    oidc::callback(state.authority(), cfg, params, now)
        .await
        .map_err(enroll_error)?;
    Ok((
        StatusCode::OK,
        Json(OidcCallbackResponse {
            status: "confirmed",
        }),
    )
        .into_response())
}

/// Map an [`EnrollError`] to the uniform non-2xx — the authority confirm reuses its mapping; every other
/// (coarse, secret-free) OIDC-handshake failure is a 400.
fn enroll_error(e: EnrollError) -> PlaneHttpError {
    match e {
        EnrollError::Confirm(inner) => PlaneHttpError::Authority(inner),
        other => PlaneHttpError::BadBody(other.to_string()),
    }
}
