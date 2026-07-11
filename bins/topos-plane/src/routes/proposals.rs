//! `POST /v1/proposals` — open a proposal (`publish --propose`): ingest a full candidate WITHOUT moving
//! `current` (`NEEDS_REVIEW`). Same input shape as publish; the op is `PublishPropose`.
//!
//! `GET /v1/workspaces/{ws}/skills/{skill}/proposals` — list a rostered skill's OPEN, non-stale proposals
//! (`version_id` + base generation + `created_at` ONLY; no bytes, no proposer). A bearer read token → an
//! opaque scope; the PATH's `(ws, skill)` drive the authority's scope-vs-path guard (mismatch ⇒ the
//! indistinguishable 404). The list is MUTABLE (a publish stales a proposal out of it), so — unlike the
//! immutable version-metadata read — it is not cacheable beyond a short must-revalidate window.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use plane_store::{DeviceOp, DeviceOpRequest, SkillId, WorkspaceId};
use topos_types::JsonEnvelope;
use topos_types::requests::{ProposeRequest, WireProposalList};

use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;
use crate::wire::{self, ApiJson, map};

/// The proposals listing is MUTABLE (a publish stales a proposal out of it; an approve/reject removes it), so
/// it carries only a short, must-revalidate cache window — never the version read's `immutable`. It is
/// **`private`**: unlike the token-in-path `current` read (URL-keyed per token) or the content-addressed
/// bundle/version reads (body invariant of principal), this response is `Authorization`-header-authed and
/// **varies by roster membership** on a principal-agnostic URL — so a shared cache must NEVER store one
/// member's list and serve it to another principal within the freshness window.
const CACHE_CONTROL_LIST: &str = "private, max-age=10, must-revalidate";

#[utoipa::path(
    post,
    path = "/v1/proposals",
    tag = "writes",
    request_body = ProposeRequest,
    responses(
        (status = 200, description = "The proposal receipt (NEEDS_REVIEW on success; CONFLICT / DENIED / … otherwise).", body = JsonEnvelope),
        (status = 400, description = "Malformed body or identifier.", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn propose(
    State(state): State<PlaneState>,
    ApiJson(req): ApiJson<ProposeRequest>,
) -> Result<Response, PlaneHttpError> {
    let ws =
        WorkspaceId::parse(&req.workspace_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let skill = SkillId::parse(&req.skill_id).map_err(|e| PlaneHttpError::BadId(e.to_string()))?;
    let op_id = wire::parse_op_id(&req.op_id)?;
    let candidate = map::candidate_to_domain(req.candidate)?;
    let device = DeviceOpRequest {
        device_key_id: req.device_key_id,
        op: DeviceOp::PublishPropose,
        expected: req.expected,
    };
    let (created_at, now) = wire::now_utc();
    let receipt = state
        .authority()
        .propose(
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

#[utoipa::path(
    get,
    path = "/v1/workspaces/{ws}/skills/{skill}/proposals",
    tag = "reads",
    params(
        ("ws" = String, Path, description = "Workspace id."),
        ("skill" = String, Path, description = "Skill id (must match the read token's scope)."),
        ("Authorization" = String, Header, description = "Bearer <read_token>."),
    ),
    responses(
        (status = 200, description = "The skill's OPEN, non-stale proposals (a possibly-empty list).", body = WireProposalList),
        (status = 404, description = "No/blank credential, or scope/path mismatch (indistinguishable).", body = JsonEnvelope),
        (status = 429, description = "Rate limited (Retry-After header).", body = JsonEnvelope),
        (status = 500, description = "Integrity / internal store fault.", body = JsonEnvelope),
    ),
)]
pub(crate) async fn list_proposals(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let token = wire::bearer_token(&headers)?;
    // The server clock (epoch-ms) enforces the read token's expiry inside the authority.
    let now = wire::now_utc().1;
    let scope = state.authority().resolve_read_token(&token, now).await?;
    let proposals = state
        .authority()
        .list_open_proposals(&scope, &ws, &skill)
        .await?;
    let response_headers = [(
        header::CACHE_CONTROL,
        HeaderValue::from_static(CACHE_CONTROL_LIST),
    )];
    // `Json` sets `application/json`; the array adds the short Cache-Control (no ETag — the list is mutable).
    Ok((
        StatusCode::OK,
        response_headers,
        Json(map::open_proposals_to_wire(proposals)),
    )
        .into_response())
}
