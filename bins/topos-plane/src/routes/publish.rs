//! `POST /v1/publish` — a direct publish that moves `current` (or genesis). Under `review-required` the
//! authority refuses it closed with `APPROVAL_REQUIRED` (a 200 carrying that receipt), ingesting nothing.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plane_store::{DeviceSignedOp, SkillId, WorkspaceId};
use topos_core::sign::DeviceOp;
use topos_types::JsonEnvelope;
use topos_types::requests::PublishRequest;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/publish",
    tag = "writes",
    request_body = PublishRequest,
    params(("Topos-Device-Signature" = String, Header, description = "base64url(64-byte Ed25519 device-op signature), 86 chars")),
    responses(
        (status = 200, description = "The publish receipt — EVERY protocol outcome (OK / CONFLICT / APPROVAL_REQUIRED / DENIED / …).", body = JsonEnvelope),
        (status = 400, description = "Malformed body, identifier, or device signature.", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn publish(
    State(state): State<PlaneState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<PublishRequest>,
) -> Result<Response, PlaneHttpError> {
    let signature = wire::device_signature(&headers)?;
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let skill = SkillId::parse(&req.skill_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let op_id = wire::parse_op_id(&req.op_id)?;
    let candidate = map::candidate_to_domain(req.candidate)?;
    let device = DeviceSignedOp {
        device_key_id: req.device_key_id,
        op: DeviceOp::PublishDirect,
        signature,
        expected: req.expected,
    };
    let (created_at, now) = wire::now_utc();
    let receipt = state
        .authority()
        .publish(
            &ws,
            &skill,
            &op_id,
            candidate,
            device,
            req.display_name.as_deref(),
            &created_at,
            now,
        )
        .await?;
    Ok((
        StatusCode::OK,
        Json(map::write_envelope(&receipt, &req.workspace_id)),
    )
        .into_response())
}
