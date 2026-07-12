//! The governance routes — owner/admin device-credential mutations: roster set/remove and device revoke.
//! Each handler is THIN: rebuild the typed [`GovernanceRequest`] (the Bearer workspace credential + the op,
//! plus the `{email}` path target), call ONE authority op, and map the [`GovernanceOutcome`] to a 200
//! all-outcome envelope. (Invitation moved to `POST /v1/workspaces/{ws}/invitations` — a member-lane roster
//! WRITE, not a governance op — when the tokened invite door was interred.)
//!
//! The acting device (the **actor**) is NEVER `Principal::parse`d here — the authority resolves its principal
//! (and role) from the device registry row the presented credential's sha256 selects. A `{email}` / target is
//! op DATA, so it is parsed into a [`Principal`] to build the typed op — never trusted as an identity. A
//! role-denial is a 200 + DENIED envelope (the actor is an authenticated member — nothing to hide), never a
//! 403; the indistinguishable 404 is reserved for skill-scoped object reads.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use plane_store::{GovernanceOp, GovernanceRequest, Principal, WorkspaceId};
use topos_types::requests::{DeviceRevokeRequest, RosterRemoveRequest, RosterSetRequest};

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/roster/{email}",
    tag = "governance",
    request_body = RosterSetRequest,
    params(
        ("ws" = String, Path, description = "Workspace id (REST sugar; the authoritative workspace_id rides the body)."),
        ("email" = String, Path, description = "The target principal whose role is set."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>` — the acting owner device's credential."),
    ),
    responses(
        (status = 200, description = "The roster-set receipt — OK on success, DENIED (a 200) on a role denial.", body = topos_types::JsonEnvelope),
        (status = 400, description = "Malformed body or identifier.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn roster_set(
    State(state): State<PlaneState>,
    // `{ws}` is REST sugar; the authoritative `workspace_id` rides the body. `{email}` is the (op-data) target.
    Path((_ws, email)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    ApiJson(req): ApiJson<RosterSetRequest>,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let target = parse_target(&email)?;
    let (created_at, now) = wire::now_utc();
    let request = GovernanceRequest {
        credential,
        op: GovernanceOp::RosterSet {
            role: wire::domain_role(req.role),
            target,
        },
    };
    let outcome = state
        .authority()
        .roster_set(&ws, &req.op_id, request, &created_at, now)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::governance_envelope(
            "roster",
            &outcome,
            serde_json::json!({}),
        )),
    )
        .into_response())
}

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/roster/{email}",
    tag = "governance",
    request_body = RosterRemoveRequest,
    params(
        ("ws" = String, Path, description = "Workspace id (REST sugar; the authoritative workspace_id rides the body)."),
        ("email" = String, Path, description = "The target principal to remove from the roster."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>` — the acting owner device's credential."),
    ),
    responses(
        (status = 200, description = "The roster-remove receipt — OK on success, DENIED (a 200) on a role denial.", body = topos_types::JsonEnvelope),
        (status = 400, description = "Malformed body or identifier.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn roster_remove(
    State(state): State<PlaneState>,
    Path((_ws, email)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    ApiJson(req): ApiJson<RosterRemoveRequest>,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let target = parse_target(&email)?;
    let (created_at, now) = wire::now_utc();
    let request = GovernanceRequest {
        credential,
        op: GovernanceOp::RosterRemove { target },
    };
    let outcome = state
        .authority()
        .roster_remove(&ws, &req.op_id, request, &created_at, now)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::governance_envelope(
            "roster",
            &outcome,
            serde_json::json!({}),
        )),
    )
        .into_response())
}

#[utoipa::path(
    delete,
    path = "/v1/workspaces/{ws}/devices",
    tag = "governance",
    request_body = DeviceRevokeRequest,
    params(
        ("ws" = String, Path, description = "Workspace id (REST sugar; the authoritative workspace_id rides the body)."),
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>` — the acting device's credential (owner, or the target's own principal)."),
    ),
    responses(
        (status = 200, description = "The revoke receipt — OK on success (instant revoke), DENIED (a 200) on a role denial.", body = topos_types::JsonEnvelope),
        (status = 400, description = "Malformed body or identifier.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn revoke_device(
    State(state): State<PlaneState>,
    Path(_ws): Path<String>,
    headers: axum::http::HeaderMap,
    ApiJson(req): ApiJson<DeviceRevokeRequest>,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let (created_at, now) = wire::now_utc();
    let request = GovernanceRequest {
        credential,
        op: GovernanceOp::DeviceRevoke {
            target_device_key_id: req.target_device_key_id,
        },
    };
    let outcome = state
        .authority()
        .revoke_device(&ws, &req.op_id, request, &created_at, now)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::governance_envelope(
            "revoke",
            &outcome,
            serde_json::json!({}),
        )),
    )
        .into_response())
}

/// Parse one target email/principal (op data — never the actor's confirmed identity). A malformed value is a
/// 400 (a malformed identifier), exactly like a bad workspace/skill id.
fn parse_target(email: &str) -> Result<Principal, PlaneHttpError> {
    Principal::parse(email).map_err(|e| PlaneHttpError::BadId(e.to_string()))
}
