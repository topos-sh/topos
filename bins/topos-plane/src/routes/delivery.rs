//! The currency lane for one enrolled device: `GET /v1/workspaces/{ws}/delivery` (what this device should
//! have) + `PUT /v1/workspaces/{ws}/report` (the fleet's applied-state report). Both authenticate by the
//! ONE Bearer workspace credential; every miss — missing/blank/unknown/revoked credential, or a non-member
//! (a REMOVED member reads the whole workspace as not-found) — is the single indistinguishable **404**.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use topos_types::JsonEnvelope;
use topos_types::requests::{WireAppliedReport, WireDelivery};

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

/// Delivery is PER-DEVICE, hot, and membership/exclusion-varying: a shared cache must NEVER store one
/// device's answer and serve it to another, and even the same device must re-fetch every session (currency
/// is the whole point). So **`no-store`** — stricter than the catalog index's `private, max-age=10`, which
/// tolerates a few seconds of staleness on a coarser, per-workspace list; a delivery answer tolerates none.
const CACHE_CONTROL_DELIVERY: &str = "no-store";

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/delivery",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "This device's delivery answer (entitled skills, detached, notices, open-proposal count).", body = WireDelivery),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn get_delivery(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let delivery = state.delivery(&ws, &credential).await?;
    let response_headers = [(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_DELIVERY),
    )];
    Ok((StatusCode::OK, response_headers, Json(delivery)).into_response())
}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/report",
    tag = "reads",
    request_body = WireAppliedReport,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 204, description = "The applied-state report was recorded (no body)."),
        (status = 400, description = "Malformed body or a bad skill / version id.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn put_report(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<WireAppliedReport>,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let applied = map::applied_report_to_domain(req)?;
    state.report_applied(&ws, &credential, &applied).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}
