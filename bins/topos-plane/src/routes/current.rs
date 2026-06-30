//! `GET /v1/current/{read_token}` — a follower's currency check. Resolves the path token to an opaque scope
//! and serves the skill's signed `current` record, with a **commit-sensitive 304**: the ETag is the frozen
//! `"<epoch>.<seq>"` (for caching), but a 304 fires ONLY when the client's `If-None-Match` AND its
//! `Topos-Known-Version-Id` both match the served record — so a record that reuses a generation for a
//! DIFFERENT commit is always returned (and the client catches it as a reused-tuple ALARM), never hidden.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::Response;
use plane_store::AuthorityError;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;

/// Short, must-revalidate caching for the movable pointer (a stale cached pointer must never be trusted past
/// a few seconds — currency is the whole point).
const CACHE_CONTROL_CURRENT: &str = "max-age=10, must-revalidate";

#[utoipa::path(
    get,
    path = "/v1/current/{read_token}",
    tag = "reads",
    params(
        ("read_token" = String, Path, description = "The per-follower read token (resolves to an opaque scope)."),
        ("If-None-Match" = Option<String>, Header, description = "The cached ETag \"<epoch>.<seq>\"."),
        ("Topos-Known-Version-Id" = Option<String>, Header, description = "The client's known current commit id (hex64) — the commit-sensitive half of the 304."),
    ),
    responses(
        (status = 200, description = "The signed current record (application/json) + a commit-sensitive ETag.", body = String, content_type = "application/json"),
        (status = 304, description = "Pointer unchanged (the ETag AND the known version both match)."),
        (status = 404, description = "No such token, or no current pointer yet.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn get_current(
    State(state): State<PlaneState>,
    Path(read_token): Path<String>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let scope = state.authority().resolve_read_token(&read_token).await?;
    let pointer = state
        .authority()
        .read_current(&scope)
        .await?
        .ok_or(PlaneHttpError::Authority(AuthorityError::NotFound))?;

    let etag = format!(
        "\"{}.{}\"",
        pointer.generation.epoch, pointer.generation.seq
    );
    let known_version = hex::encode(pointer.version_id);

    let etag_matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str());
    let version_matches = headers
        .get("topos-known-version-id")
        .and_then(|v| v.to_str().ok())
        == Some(known_version.as_str());

    // Commit-sensitive: 304 ONLY when BOTH the generation (ETag) and the commit match.
    if etag_matches && version_matches {
        return Ok(Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag)
            .header(header::CACHE_CONTROL, CACHE_CONTROL_CURRENT)
            .body(Body::empty())
            .expect("a 304 response with static headers is always well-formed"));
    }

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ETAG, etag)
        .header(header::CACHE_CONTROL, CACHE_CONTROL_CURRENT)
        .body(Body::from(pointer.signed_record))
        .expect("a 200 current response with static headers is always well-formed"))
}
