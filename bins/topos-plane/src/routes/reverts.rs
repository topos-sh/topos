//! `POST /v1/reverts` — a FORWARD revert: the server builds a new 1-parent commit carrying `good`'s bytes on
//! top of `current` (`seq` advances; the pointer never moves backward). No candidate; the op is `Revert`.

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use plane_store::{BundleId, CommitId, DeviceOp, DeviceOpAuth, WorkspaceId};
use topos_types::JsonEnvelope;
use topos_types::requests::RevertRequest;

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

#[utoipa::path(
    post,
    path = "/v1/reverts",
    tag = "writes",
    request_body = RevertRequest,
    params(
        ("Authorization" = String, Header, description = "`Bearer <workspace credential>` — the acting device's credential."),
    ),
    responses(
        (status = 200, description = "The revert receipt (OK on success; CONFLICT / DENIED / … otherwise).", body = JsonEnvelope),
        (status = 400, description = "Malformed body or identifier.", body = JsonEnvelope),
        (status = 404, description = "Missing/blank credential.", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn revert(
    State(state): State<PlaneState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<RevertRequest>,
) -> Result<Response, PlaneHttpError> {
    let credential = wire::bearer_token(&headers)?;
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let skill = BundleId::parse(&req.skill_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let op_id = wire::parse_op_id(&req.op_id)?;
    let good = wire::hex32(&req.good)
        .map(CommitId)
        .ok_or_else(|| PlaneHttpError::BadId(format!("invalid good version id {:?}", req.good)))?;
    let auth = DeviceOpAuth {
        credential,
        op: DeviceOp::Revert,
        expected: req.expected,
    };
    let (created_at, now) = wire::now_utc();
    let receipt = state
        .authority()
        .revert(
            &ws,
            &skill,
            good,
            auth,
            &req.author,
            &req.message,
            &op_id,
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
