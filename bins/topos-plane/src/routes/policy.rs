//! `PUT /v1/workspaces/{ws}/policy/review-required` — the SELF-HOST operator's `review-required` toggle.
//!
//! Authenticated by the plane's admin bearer token (`--admin-token` / `TOPOS_PLANE_ADMIN_TOKEN`), not a
//! device-op signature — the operator owns the plane; a device-signed governance variant needs a new
//! kernel frame and lands later. With NO admin token configured the route answers **404** (invisible), so
//! a downstream composition that merges `router(state)` without setting a token can never expose an
//! unauthenticated policy write on its open `/v1/` lane; a configured route answers an honest **401** on a
//! missing/wrong token (the operator's own secret — see the `wire::error` scoping note). Enforcement of
//! the policy itself lives in the write path (a direct publish under the gate fails typed,
//! `APPROVAL_REQUIRED`); this route only sets the workspace row, idempotently. **204** on success.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plane_store::WorkspaceId;
use topos_types::requests::PolicyReviewRequiredRequest;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson};

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/policy/review-required",
    tag = "policy",
    request_body = PolicyReviewRequiredRequest,
    params(
        ("ws" = String, Path, description = "The workspace id."),
        ("Authorization" = String, Header, description = "Bearer <admin token> (the plane operator's secret)."),
    ),
    responses(
        (status = 204, description = "The policy was set (an idempotent write)."),
        (status = 400, description = "Malformed body or workspace id.", body = topos_types::JsonEnvelope),
        (status = 401, description = "Admin token configured but missing/invalid on the request.", body = topos_types::JsonEnvelope),
        (status = 404, description = "No admin token is configured on this plane (the route is disabled).", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn set_review_required(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<PolicyReviewRequiredRequest>,
) -> Result<Response, PlaneHttpError> {
    // Disabled ⇒ the same indistinguishable 404 a missing route answers — checked BEFORE any parse or
    // authority touch, so an unconfigured plane's response carries no oracle.
    if !state.admin_token_configured() {
        return Err(PlaneHttpError::MissingReadCredential);
    }
    // Configured ⇒ an honest 401 on a missing/malformed/wrong bearer token.
    let provided = wire::bearer_token(&headers).map_err(|_| PlaneHttpError::Unauthorized)?;
    if !state.admin_token_matches(&provided) {
        return Err(PlaneHttpError::Unauthorized);
    }
    let ws = WorkspaceId::parse(&ws).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    state
        .authority()
        .set_review_required(&ws, req.review_required)
        .await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
