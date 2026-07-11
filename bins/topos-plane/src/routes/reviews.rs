//! `POST /v1/reviews` — a governance decision on an open proposal. `approve` runs the shared `(epoch,seq)`
//! CAS on the proposal's base (stale ⇒ CONFLICT) and, under `review_required`, four-eyes, then promotes
//! (an approve promotes the pointer); `reject` is a standalone status flip. The op is derived from the decision.

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use plane_store::{CommitId, DeviceOp, DeviceOpRequest, SkillId, WorkspaceId};
use topos_types::JsonEnvelope;
use topos_types::requests::ReviewRequest;
use topos_types::results::ReviewDecision;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/reviews",
    tag = "writes",
    request_body = ReviewRequest,
    responses(
        (status = 200, description = "The review receipt (OK on an approve that promoted; CONFLICT / DENIED / … otherwise).", body = JsonEnvelope),
        (status = 400, description = "Malformed body or identifier.", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn review(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<ReviewRequest>,
) -> Result<Response, PlaneHttpError> {
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let skill = SkillId::parse(&req.skill_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let op_id = wire::parse_op_id(&req.op_id)?;
    let proposal = wire::hex32(&req.proposal)
        .map(CommitId)
        .ok_or_else(|| PlaneHttpError::BadId(format!("invalid proposal id {:?}", req.proposal)))?;
    let (created_at, now) = wire::now_utc();

    let receipt = match req.decision {
        ReviewDecision::Approve => {
            let device = DeviceOpRequest {
                device_key_id: req.device_key_id,
                op: DeviceOp::ReviewApprove,
                expected: req.expected,
            };
            state
                .authority()
                .review_approve(&ws, &skill, proposal, device, &op_id, &created_at, now)
                .await?
        }
        ReviewDecision::Reject => {
            let device = DeviceOpRequest {
                device_key_id: req.device_key_id,
                op: DeviceOp::ReviewReject,
                expected: req.expected,
            };
            state
                .authority()
                .review_reject(&ws, &skill, proposal, device, &op_id, &created_at)
                .await?
        }
    };
    Ok((
        StatusCode::OK,
        Json(map::write_envelope(&receipt, &req.workspace_id)),
    )
        .into_response())
}
