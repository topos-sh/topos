//! The `protect` setter — per-bundle protection (a skill: `reviewed`/`open`) or per-channel mode (a channel:
//! `curated`/`open`), kind-polymorphic over ONE authority op. A small `{level}` JSON body; authenticated by
//! the ONE Bearer workspace credential and front-doored by the ONE membership predicate; every miss is the
//! uniform 404. The level is parsed per kind at the edge (a wrong level for the kind is a 400). Naturally
//! idempotent (no receipt): a 200 all-outcome envelope carrying `set`, or a 200 DENIED with a specific code
//! (`REVIEWER_ROLE_REQUIRED` tightening / `OWNER_ROLE_REQUIRED` loosening). THIN: parse → one op → serialize.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plane_store::{ProtectKind, ProtectLevel, WorkspaceId};
use topos_types::JsonEnvelope;
use topos_types::requests::ProtectionSetRequest;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

/// Parse the path workspace id — a malformed id folds to the uniform miss.
fn parse_ws(ws: &str) -> Result<WorkspaceId, PlaneHttpError> {
    WorkspaceId::parse(ws).map_err(|_| plane_store::AuthorityError::NotFound.into())
}

/// Parse the wire `level` for a given kind — a level not valid for the kind is a 400 (`reviewed` is a
/// skill's protected level, `curated` a channel's; `open` loosens either).
fn parse_level(kind: ProtectKind, level: &str) -> Result<ProtectLevel, PlaneHttpError> {
    match (kind, level) {
        (_, "open") => Ok(ProtectLevel::Open),
        (ProtectKind::Skill, "reviewed") => Ok(ProtectLevel::Protected),
        (ProtectKind::Channel, "curated") => Ok(ProtectLevel::Protected),
        (ProtectKind::Skill, _) => Err(PlaneHttpError::BadBody(
            "a skill protection level must be `reviewed` or `open`".to_owned(),
        )),
        (ProtectKind::Channel, _) => Err(PlaneHttpError::BadBody(
            "a channel protection level must be `curated` or `open`".to_owned(),
        )),
    }
}

async fn set_protection(
    state: &PlaneState,
    ws: &str,
    kind: ProtectKind,
    target_name: &str,
    req: ProtectionSetRequest,
    headers: &HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(headers)?;
    let ws = parse_ws(ws)?;
    let level = parse_level(kind, &req.level)?;
    let created_at = wire::now_utc().0;
    let outcome = state
        .authority()
        .protect(&ws, &credential, kind, target_name, level, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::protect_envelope("protect", outcome)),
    )
        .into_response())
}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/skills/{skill}/protection",
    tag = "writes",
    request_body = ProtectionSetRequest,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's immutable id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The protect outcome (set, or a 200 DENIED REVIEWER_ROLE_REQUIRED / OWNER_ROLE_REQUIRED).", body = JsonEnvelope),
        (status = 400, description = "A level not valid for a skill (must be `reviewed` or `open`).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn set_skill_protection(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<ProtectionSetRequest>,
) -> Result<Response, PlaneHttpError> {
    set_protection(&state, &ws, ProtectKind::Skill, &skill, req, &headers).await
}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/channels/{ch}/protection",
    tag = "writes",
    request_body = ProtectionSetRequest,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("ch" = String, Path, description = "The channel name."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The protect outcome (set, or a 200 DENIED REVIEWER_ROLE_REQUIRED / OWNER_ROLE_REQUIRED).", body = JsonEnvelope),
        (status = 400, description = "A level not valid for a channel (must be `curated` or `open`).", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn set_channel_protection(
    State(state): State<PlaneState>,
    Path((ws, ch)): Path<(String, String)>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<ProtectionSetRequest>,
) -> Result<Response, PlaneHttpError> {
    set_protection(&state, &ws, ProtectKind::Channel, &ch, req, &headers).await
}
