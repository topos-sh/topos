//! `POST /v1/workspaces/{ws}/notices/ack` — mark the caller's own notices read by id (the delivery feed
//! carries them unacked; an interactive session acks what it narrated, the silent hook never acks).
//! Authenticated by the ONE Bearer workspace credential and front-doored by the ONE membership predicate;
//! every miss is the uniform 404. Idempotent (only the person's own unacked rows move); a 200 envelope
//! carrying `{ "acked": true }`. THIN: parse → one authority op → serialize.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use plane_store::WorkspaceId;
use topos_types::JsonEnvelope;
use topos_types::requests::NoticeAckRequest;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/workspaces/{ws}/notices/ack",
    tag = "writes",
    request_body = NoticeAckRequest,
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>`."),
    ),
    responses(
        (status = 200, description = "The notices were acked (idempotent — only the caller's own unacked rows move).", body = JsonEnvelope),
        (status = 400, description = "Malformed body.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential, unknown/revoked one, or non-member (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn ack_notices(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
    headers: axum::http::HeaderMap,
    ApiJson(req): ApiJson<NoticeAckRequest>,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws = WorkspaceId::parse(&ws)
        .map_err(|_| PlaneHttpError::from(plane_store::AuthorityError::NotFound))?;
    let now = wire::now_utc().1;
    state
        .authority()
        .ack_notices(&ws, &credential, &req.ids, now)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::ok_status_envelope("notices", "acked")),
    )
        .into_response())
}
