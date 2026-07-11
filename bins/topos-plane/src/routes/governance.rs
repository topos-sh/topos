//! The governance routes — owner/admin device-credential mutations: create-invite, roster set/remove, and
//! device revoke. Each handler is THIN: rebuild the typed [`GovernanceRequest`] from the body (the presented
//! `device_key_id` + the op, plus the `{email}` path target), call ONE authority op, and map the
//! [`GovernanceOutcome`] to a 200 all-outcome envelope.
//!
//! The acting device (the **actor**) is NEVER `Principal::parse`d here — the authority resolves its principal
//! (and role) from the device registry row selected by the presented `device_key_id`. A `{email}` / target is
//! op DATA, so it is parsed into a [`Principal`] to build the typed op — never trusted as an identity. A
//! role-denial is a 200 + DENIED envelope (the actor is an authenticated member — nothing to hide), never a
//! 403; the indistinguishable 404 is reserved for skill-scoped object reads.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use plane_store::{GovernanceOp, GovernanceRequest, Principal, Role, SkillId, WorkspaceId};
use topos_types::requests::{
    DeviceRevokeRequest, InviteRequest, RosterRemoveRequest, RosterSetRequest, WorkspaceRole,
};

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/invites",
    tag = "governance",
    request_body = InviteRequest,
    responses(
        (status = 200, description = "The invite receipt — OK carries the InviteData (link + seeded roster/skills); DENIED the flat error.", body = topos_types::JsonEnvelope),
        (status = 400, description = "Malformed body or identifier.", body = topos_types::JsonEnvelope),
        (status = 429, description = "Rate limited.", body = topos_types::JsonEnvelope),
        (status = 500, description = "Internal store fault.", body = topos_types::JsonEnvelope),
    ),
)]
pub(crate) async fn create_invite(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<InviteRequest>,
) -> Result<Response, PlaneHttpError> {
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    // The invited emails + offered skills are op DATA.
    let emails = parse_emails(&req.emails)?;
    let skills = parse_invite_skills(req.skills)?;
    // An omitted role defaults to `member`.
    let role = role_or_default(req.role);
    let (created_at, _now) = wire::now_utc();
    let request = GovernanceRequest {
        device_key_id: req.device_key_id,
        op: GovernanceOp::Invite {
            role,
            // The wire body carries no invite expiry; an invite never expires unless governance adds it later.
            expires_at: None,
            emails,
            skills,
        },
    };
    let outcome = state
        .authority()
        .create_invite(&ws, &req.op_id, request, &created_at)
        .await?;
    Ok((StatusCode::OK, Json(map::invite_envelope(outcome))).into_response())
}

#[utoipa::path(
    put,
    path = "/v1/workspaces/{ws}/roster/{email}",
    tag = "governance",
    request_body = RosterSetRequest,
    params(
        ("ws" = String, Path, description = "Workspace id (REST sugar; the authoritative workspace_id rides the body)."),
        ("email" = String, Path, description = "The target principal whose role is set."),
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
    ApiJson(req): ApiJson<RosterSetRequest>,
) -> Result<Response, PlaneHttpError> {
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let target = parse_target(&email)?;
    let (created_at, _now) = wire::now_utc();
    let request = GovernanceRequest {
        device_key_id: req.device_key_id,
        op: GovernanceOp::RosterSet {
            role: wire::domain_role(req.role),
            target,
        },
    };
    let outcome = state
        .authority()
        .roster_set(&ws, &req.op_id, request, &created_at)
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
    ApiJson(req): ApiJson<RosterRemoveRequest>,
) -> Result<Response, PlaneHttpError> {
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let target = parse_target(&email)?;
    let (created_at, _now) = wire::now_utc();
    let request = GovernanceRequest {
        device_key_id: req.device_key_id,
        op: GovernanceOp::RosterRemove { target },
    };
    let outcome = state
        .authority()
        .roster_remove(&ws, &req.op_id, request, &created_at)
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
    ApiJson(req): ApiJson<DeviceRevokeRequest>,
) -> Result<Response, PlaneHttpError> {
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let (created_at, _now) = wire::now_utc();
    let request = GovernanceRequest {
        device_key_id: req.device_key_id,
        op: GovernanceOp::DeviceRevoke {
            target_device_key_id: req.target_device_key_id,
        },
    };
    let outcome = state
        .authority()
        .revoke_device(&ws, &req.op_id, request, &created_at)
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

/// An omitted invite role defaults to `member` (the least-privilege default).
fn role_or_default(role: Option<WorkspaceRole>) -> Role {
    wire::domain_role(role.unwrap_or(WorkspaceRole::Member))
}

/// Parse the invited emails into op-data principals (the authority binds them into the op's request identity).
fn parse_emails(emails: &[String]) -> Result<Vec<Principal>, PlaneHttpError> {
    emails.iter().map(|e| parse_target(e)).collect()
}

/// Parse one target email/principal (op data — never the actor's confirmed identity). A malformed value is a
/// 400 (a malformed identifier), exactly like a bad workspace/skill id.
fn parse_target(email: &str) -> Result<Principal, PlaneHttpError> {
    Principal::parse(email).map_err(|e| PlaneHttpError::BadId(e.to_string()))
}

/// Parse the offered invite skills into `(SkillId, Option<name>)` pairs (the name is op data).
fn parse_invite_skills(
    skills: Vec<topos_types::requests::InviteSkill>,
) -> Result<Vec<(SkillId, Option<String>)>, PlaneHttpError> {
    skills
        .into_iter()
        .map(|s| {
            let id =
                SkillId::parse(&s.skill_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
            Ok((id, s.name))
        })
        .collect()
}
