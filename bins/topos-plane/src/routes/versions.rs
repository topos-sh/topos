//! `GET /v1/workspaces/{ws}/skills/{skill}/versions/{version_id}` — a version's authenticated metadata
//! (`version_id`, parents, author, message, `bundle_digest`, per-file `(path, mode, object_id)` leaves),
//! assembled WITHOUT reading any blob bytes. Bearer read token → opaque scope; the PATH's `(ws, skill)`
//! drive the authority's scope-vs-path + R1 check (mismatch/unauthorized ⇒ the indistinguishable 404).
//! Content-addressed (the id pins the bytes), so the response is `immutable` with the version id as ETag.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use topos_types::requests::WireVersionMeta;

use crate::state::PlaneState;
use crate::wire::{self, error::PlaneHttpError, map};

/// A version's metadata is fixed by its content id — cache it forever.
const CACHE_CONTROL_IMMUTABLE: &str = "max-age=31536000, immutable";

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "Skill id (must match the read token's scope)."),
        ("version_id" = String, Path, description = "The version (commit) id (hex64) whose metadata to read."),
        ("Authorization" = String, Header, description = "Bearer <read_token>."),
    ),
    responses(
        (status = 200, description = "The version's authenticated metadata (immutable).", body = WireVersionMeta),
        (status = 404, description = "No/blank credential, scope/path mismatch, or unauthorized/unreachable version.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = topos_types::JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn get_version(
    State(state): State<PlaneState>,
    Path((ws, skill, version_id)): Path<(String, String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let token = wire::bearer_token(&headers)?;
    // The server clock (epoch-ms) enforces the read token's expiry inside the authority.
    let now = wire::now_utc().1;
    let scope = state.authority().resolve_read_token(&token, now).await?;
    let meta = state
        .authority()
        .read_version_metadata(&scope, &ws, &skill, &version_id)
        .await?;
    let wire_meta: WireVersionMeta = map::version_meta_to_wire(meta);

    let etag = HeaderValue::from_str(&format!("\"{version_id}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("\"version\""));
    let response_headers = [
        (header::ETAG, etag),
        (
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL_IMMUTABLE),
        ),
    ];
    // `Json` sets `application/json`; the array adds ETag + Cache-Control.
    Ok((StatusCode::OK, response_headers, Json(wire_meta)).into_response())
}
