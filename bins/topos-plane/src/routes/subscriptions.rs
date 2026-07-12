//! The person-scoped subscription writes — direct follow/unfollow of a skill (by its immutable id), and
//! this-device exclusion (the `remove` verb's row). Bodyless PUT/DELETE, authenticated by the ONE Bearer
//! workspace credential and front-doored by the ONE membership predicate; every miss is the uniform 404.
//! Naturally idempotent (no receipt): each answers a 200 all-outcome envelope carrying a `status` string,
//! or a 200 DENIED with a specific code (`SKILL_NOT_ACTIVE`). THIN: parse → one authority op → serialize.

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
    path = "/v1/workspaces/{ws}/follows/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id to direct-follow (the client resolves the address to it)."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The subscription outcome (followed, or a 200 DENIED SKILL_NOT_ACTIVE).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn follow_skill(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let created_at = wire::now_utc().0;
    let outcome = state
        .authority()
        .follow_skill(&ws, &credential, &skill, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::subscription_envelope("follow", outcome)),
    )
        .into_response())
}

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/follows/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id to unfollow (person-scoped negative mask)."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The subscription outcome (unfollowed, or a 200 DENIED SKILL_NOT_ACTIVE).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn unfollow_skill(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let (created_at, now) = wire::now_utc();
    let outcome = state
        .authority()
        .unfollow_skill(&ws, &credential, &skill, now, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::subscription_envelope("unfollow", outcome)),
    )
        .into_response())
}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/exclusions/{skill}",
    tag = "writes",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The followed skill's immutable id to exclude from THIS device."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The subscription outcome (excluded, or a 200 DENIED SKILL_NOT_ACTIVE).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn exclude_device(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let created_at = wire::now_utc().0;
    let outcome = state
        .authority()
        .exclude_device(&ws, &credential, &skill, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::subscription_envelope("remove", outcome)),
    )
        .into_response())
}
