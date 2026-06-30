//! `POST /v1/proposals` — open a proposal (`publish --propose`): ingest a full candidate WITHOUT moving
//! `current` or signing (`NEEDS_REVIEW`). Same input shape as publish; the op is `PublishPropose`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plane_store::{DeviceSignedOp, OpId, SkillId, WorkspaceId};
use topos_core::sign::DeviceOp;
use topos_types::JsonEnvelope;
use topos_types::requests::ProposeRequest;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/proposals",
    tag = "writes",
    request_body = ProposeRequest,
    params(("Topos-Device-Signature" = String, Header, description = "base64url(64-byte Ed25519 device-op signature), 86 chars")),
    responses(
        (status = 200, description = "The proposal receipt (NEEDS_REVIEW on success; CONFLICT / DENIED / … otherwise).", body = JsonEnvelope),
        (status = 400, description = "Malformed body, identifier, or device signature.", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn propose(
    State(state): State<PlaneState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<ProposeRequest>,
) -> Result<Response, PlaneHttpError> {
    let signature = wire::device_signature(&headers)?;
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let skill = SkillId::parse(&req.skill_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let op_id = OpId::parse(&req.op_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let candidate = map::candidate_to_domain(req.candidate)?;
    let device = DeviceSignedOp {
        device_key_id: req.device_key_id,
        op: DeviceOp::PublishPropose,
        signature,
        expected: req.expected,
    };
    let (created_at, now) = wire::now_utc();
    let receipt = state
        .authority()
        .propose(&ws, &skill, &op_id, candidate, device, &created_at, now)
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::write_envelope(&receipt, &req.workspace_id)),
    )
        .into_response())
}
