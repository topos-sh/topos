//! The channel row ops — self-serve membership (join/leave) and curation (place/unplace a skill reference).
//! Bodyless PUT/DELETE, authenticated by the ONE Bearer workspace credential and front-doored by the ONE
//! membership predicate; every miss is the uniform 404. Naturally idempotent (no receipt): each answers a
//! 200 all-outcome envelope carrying a `status` string, or a 200 DENIED with a specific code
//! (`CHANNEL_BUILTIN` on `everyone`; `CURATED_ROLE_REQUIRED` / `BAD_NAME` / `SKILL_NOT_ACTIVE` on curation).
//! THIN: parse → one authority op → serialize.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plane_store::WorkspaceId;
use topos_types::JsonEnvelope;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, map};

/// Parse the path workspace id — a malformed id folds to the uniform miss.
fn parse_ws(ws: &str) -> Result<WorkspaceId, PlaneHttpError> {
    WorkspaceId::parse(ws).map_err(|_| plane_store::AuthorityError::NotFound.into())
}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/channels/{ch}/membership",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name to join."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The membership outcome (joined, or a 200 DENIED CHANNEL_BUILTIN for `everyone`).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn channel_join(
    State(state): State<PlaneState>,
    Path((ws, ch)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let created_at = wire::now_utc().0;
    let outcome = state
        .authority()
        .channel_join(&ws, &credential, &ch, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::membership_envelope("channel", outcome)),
    )
        .into_response())
}

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/channels/{ch}/membership",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name to leave."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The membership outcome (left / not_member, or a 200 DENIED CHANNEL_BUILTIN for `everyone`).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn channel_leave(
    State(state): State<PlaneState>,
    Path((ws, ch)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let (created_at, now) = wire::now_utc();
    let outcome = state
        .authority()
        .channel_leave(&ws, &credential, &ch, now, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::membership_envelope("channel", outcome)),
    )
        .into_response())
}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/channels/{ch}/skills/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name (created on first placement, member-level self-serve)."),
        ("skill" = String, Path, description = "The skill's immutable id to place into the channel."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The curation outcome (placed / created, or a 200 DENIED CURATED_ROLE_REQUIRED / BAD_NAME / SKILL_NOT_ACTIVE).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn channel_place(
    State(state): State<PlaneState>,
    Path((ws, ch, skill)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let created_at = wire::now_utc().0;
    let outcome = state
        .authority()
        .channel_place(&ws, &credential, &ch, &skill, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::curation_envelope("channel", outcome)),
    )
        .into_response())
}

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/channels/{ch}/skills/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name."),
        ("skill" = String, Path, description = "The skill's immutable id to remove from the channel."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The curation outcome (removed / not_placed, or a 200 DENIED CURATED_ROLE_REQUIRED).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn channel_unplace(
    State(state): State<PlaneState>,
    Path((ws, ch, skill)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let created_at = wire::now_utc().0;
    let outcome = state
        .authority()
        .channel_unplace(&ws, &credential, &ch, &skill, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::curation_envelope("channel", outcome)),
    )
        .into_response())
}
