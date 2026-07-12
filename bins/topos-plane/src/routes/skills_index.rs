//! `GET /v1/workspaces/{ws}/skills` — the workspace skill CATALOG (metadata only, no bytes) for a confirmed
//! workspace MEMBER, authorized by a DEVICE-credential read (catalog visibility == workspace membership, on
//! BOTH cloud and self-host). The reading device presents its Bearer workspace credential; the
//! authority resolves the non-revoked registry row by its sha256 and gates on confirmed membership. A
//! missing/blank credential, an unknown/revoked one, or a non-member is the single indistinguishable
//! **404** (never a 400/401/403). The catalog is MUTABLE (a publish moves a pointer; a proposal
//! opens/closes) and membership-varying, so it carries only a short, `private` must-revalidate window.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use topos_types::JsonEnvelope;
use topos_types::requests::WireSkillIndex;

use crate::state::PlaneState;
use crate::wire::{self, error::PlaneHttpError};

/// The catalog is MUTABLE (a publish moves a pointer; a proposal opens/closes) and **`private`** — it is
/// device-authed on a principal-agnostic URL and varies by membership, so a shared cache must never store one
/// member's catalog and serve it to another. A short must-revalidate window only (no ETag — the list moves).
const CACHE_CONTROL_LIST: &str = "private, max-age=10, must-revalidate";

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The workspace skill catalog (metadata only; a possibly-empty list ordered by skill id).", body = WireSkillIndex),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn list_skills(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let index = state.list_skills_device(&ws, &credential).await?;
    let response_headers = [(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_LIST),
    )];
    // `Json` sets `application/json`; the array adds the short Cache-Control (no ETag — the catalog moves).
    Ok((StatusCode::OK, response_headers, Json(index)).into_response())
}
