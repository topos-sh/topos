//! `GET /v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}` — one object's raw bytes through the
//! skill-scoped access rule. Bearer read token → opaque scope; the PATH's `(ws, skill)` go in as the
//! request scope so the authority does the scope-vs-path check (mismatch ⇒ the indistinguishable 404).
//! Content-addressed, so the response is `immutable` with the object id as the ETag.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::state::PlaneState;
use crate::wire::{self, error::PlaneHttpError};

/// A content-addressed object never changes — cache it forever.
const CACHE_CONTROL_IMMUTABLE: &str = "max-age=31536000, immutable";

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "Skill id (must match the read token's scope)."),
        ("object_id" = String, Path, description = "The content id (hex64) of the object to fetch."),
        ("Authorization" = String, Header, description = "Bearer <read_token>."),
    ),
    responses(
        (status = 200, description = "The raw object bytes (content-addressed, immutable).", body = String, content_type = "application/octet-stream"),
        (status = 404, description = "No/blank credential, scope/path mismatch, or unreachable object.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = topos_types::JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn get_bundle(
    State(state): State<PlaneState>,
    Path((ws, skill, object_id)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let token = wire::bearer_token(&headers)?;
    // The server clock (epoch-ms) enforces the read token's expiry inside the authority.
    let now = wire::now_utc().1;
    let scope = state.authority().resolve_read_token(&token, now).await?;
    let bytes = state
        .authority()
        .serve_object(&scope, &ws, &skill, &object_id)
        .await?;

    let etag = HeaderValue::from_str(&format!("\"{object_id}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("\"object\""));
    let response_headers = [
        (header::ETAG, etag),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL_IMMUTABLE),
        ),
    ];
    // `bytes` (a `Vec<u8>`) defaults to `application/octet-stream`; the array adds ETag + Cache-Control.
    Ok((StatusCode::OK, response_headers, bytes).into_response())
}
