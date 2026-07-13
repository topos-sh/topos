//! `POST /v1/reviews` — a governance decision on an open proposal. `approve` runs the shared `(epoch,seq)`
//! CAS on the proposal's base (stale ⇒ CONFLICT) and, under `review_required`, four-eyes, then promotes
//! (an approve promotes the pointer); `reject` is a standalone status flip. The op is derived from the decision.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plane_store::{BundleId, CommitId, DeviceOp, DeviceOpAuth, WorkspaceId};
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
    params(
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>` — the acting device's credential."),
    ),
    responses(
        (status = 200, description = "The review receipt (OK on an approve that promoted; CONFLICT / DENIED / … otherwise).", body = JsonEnvelope),
        (status = 400, description = "Malformed body or identifier.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential.", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn review(
    State(state): State<PlaneState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<ReviewRequest>,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let skill = BundleId::parse(&req.skill_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let op_id = wire::parse_op_id(&req.op_id)?;
    let proposal = wire::hex32(&req.proposal)
        .map(CommitId)
        .ok_or_else(|| PlaneHttpError::BadId(format!("invalid proposal id {:?}", req.proposal)))?;
    let (created_at, now) = wire::now_utc();

    let receipt = match req.decision {
        ReviewDecision::Approve => {
            let auth = DeviceOpAuth {
                credential,
                op: DeviceOp::ReviewApprove,
                expected: req.expected,
            };
            state
                .authority()
                .review_approve(&ws, &skill, proposal, auth, &op_id, &created_at, now)
                .await?
        }
        ReviewDecision::Reject => {
            let auth = DeviceOpAuth {
                credential,
                op: DeviceOp::ReviewReject,
                expected: req.expected,
            };
            // The reject reason is MANDATORY on the device lane now (an empty/absent reason is the
            // authority's SYNTHESIZED REASON_REQUIRED denial — never persisted, so a corrected retry
            // proceeds fresh under the same op_id).
            let reason = req.reason.as_deref().unwrap_or("");
            state
                .authority()
                .review_reject(&ws, &skill, proposal, auth, reason, &op_id, &created_at)
                .await?
        }
        ReviewDecision::Withdraw => {
            // The AUTHOR retracting their own open proposal — a status flip, no pointer move, no reason.
            let auth = DeviceOpAuth {
                credential,
                op: DeviceOp::ReviewWithdraw,
                expected: req.expected,
            };
            state
                .authority()
                .review_withdraw(&ws, &skill, proposal, auth, &op_id, &created_at)
                .await?
        }
    };
    Ok((
        StatusCode::OK,
        Json(map::write_envelope(&receipt, &req.workspace_id)),
    )
        .into_response())
}
