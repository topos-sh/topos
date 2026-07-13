//! The two BYTE-DECORATED describe reads the vault keeps serving after the door cutover: the
//! review inbox and a skill's log. Both decorate their rows with git commit messages/authors
//! (byte custody the composing app deliberately does not hold), so they stay here while every
//! other member-lane describe/row op moved to the app (its contract stubs live in
//! [`super::door`]).
//!
//! Each is authenticated by the ONE Bearer workspace credential and front-doored by the ONE
//! membership predicate; every miss — missing/blank/unknown/revoked credential, non-member, or
//! (for the log) an unknown skill — is the single indistinguishable **404**. The reads mint
//! nothing durable and are per-member/hot, so each answers `Cache-Control: no-store`. Each
//! handler is THIN: parse → one authority read → serialize.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use plane_store::WorkspaceId;
use topos_types::JsonEnvelope;
use topos_types::requests::{WireProposalIndex, WireSkillLog};

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, map};

/// The describe reads are per-member and hot (the "before" of a two-phase verb) — never cacheable.
const CACHE_CONTROL: &str = "no-store";

/// Parse the path workspace id — a malformed id folds to the uniform miss (never an existence oracle).
fn parse_ws(ws: &str) -> Result<WorkspaceId, PlaneHttpError> {
    WorkspaceId::parse(ws).map_err(|_| plane_store::AuthorityError::NotFound.into())
}

/// A member-scoped read response: 200 + `no-store` + the JSON body.
fn read_response<T: serde::Serialize>(body: T) -> Response {
    (
        StatusCode::OK,
        [(
            header::CACHE_CONTROL,
            HeaderValue::from_static(CACHE_CONTROL),
        )],
        Json(body),
    )
        .into_response()
}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/proposals",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "Every OPEN proposal in the workspace (the review inbox), author-message first.", body = WireProposalIndex),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn get_proposals(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let entries = state.authority().proposals_index(&ws, &credential).await?;
    Ok(read_response(map::proposals_index_to_wire(entries)))
}

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/log",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "The skill's catalog name (or an archived successor's freed base name, or a bare skill id)."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The skill's version history (purge tombstones included) + its proposal events.", body = WireSkillLog),
        (status = 404, description = "Missing/blank credential, non-member, or unknown skill (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn get_log(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = parse_ws(&ws)?;
    let log = state
        .authority()
        .skill_log(&ws, &credential, &skill)
        .await?;
    Ok(read_response(map::skill_log_to_wire(log)))
}
