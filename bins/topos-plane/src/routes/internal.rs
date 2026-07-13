//! The INTERNAL SESSION LANE (`/internal/v1/*`) — HTTP access to the lib-only session wrappers for a
//! downstream, session-authenticated composing surface (a web app that has already proven who is acting).
//!
//! This lane exposes, over HTTP, the wrappers that otherwise have no route:
//! [`PlaneState::read_current_session`](crate::PlaneState) and its read siblings, the review/roster/standup
//! writes. It mirrors the admin-token policy
//! route's auth shape ([`routes::policy`](super::policy) + the `with_admin_token` family on
//! [`PlaneState`](crate::PlaneState)): a single configured bearer token gates the whole lane, and with NO
//! token configured every route answers the uniform **404** — an unconfigured plane never exposes the lane.
//!
//! **Auth (decided BEFORE any body/id parse — no oracle, the same ordering discipline as
//! [`routes::policy`](super::policy)):** (a) the lane token must be configured (else the uniform 404); (b)
//! `Authorization: Bearer <internal token>` must match (else an honest **401** — a composition's own shared
//! secret, the same scoped exception the admin-token route makes); (c) the acting principal rides the
//! `x-topos-acting-email` header (missing/empty ⇒ **400**). The acting email is the composing surface's
//! session-verified assertion; the wrappers' own in-transaction gates re-verify the roster rows, so this
//! lane adds **no** trust decision of its own. [`acting_principal`] is the one place all three checks live.
//!
//! **The request/response DTOs here are LANE-LOCAL** (`snake_case` serde structs) — deliberately NOT in
//! `topos-types` and NOT in the OpenAPI: this lane is composition-internal, excluded from the public wire
//! contract, so its handlers carry no `#[utoipa::path]` and never widen the committed schema.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use plane_store::AuthorityError;

use crate::lifecycle_cmd::{
    ArchiveSkillSummary, DeleteSkillSummary, PurgeVersionSummary, RenameSkillSummary,
    UnarchiveSkillSummary,
};
use crate::roster_cmd::RemoveMemberSummary;
use crate::session_read_cmd::{
    SessionCurrentSummary, SessionObjectSummary, SessionProposalsSummary, SessionVersionSummary,
};
use crate::session_review_cmd::{
    SessionProposalDetail, SessionProposalDetailSummary, SessionRevertSummary, SessionReviewSummary,
};
use crate::standup_cmd::{ApproveSessionSummary, ApproveStandupSummary, CreateWorkspaceSummary};
use crate::state::PlaneState;
use crate::wire::error::PlaneHttpError;

/// The session-verified acting principal header — the composing surface's assertion of who is acting.
const ACTING_EMAIL_HEADER: &str = "x-topos-acting-email";

// ── the lane guard ───────────────────────────────────────────────────────────────────────────────────

/// The lane's BEARER guard — steps (a)+(b) of the ordering discipline: the unconfigured-lane 404, then the
/// internal-token 401, both decided BEFORE any body or id parse. [`acting_principal`] adds step (c); the
/// PRE-IDENTITY passcode mint ([`mint_passcode`]) stops here, because no session-verified actor exists yet.
fn lane_guard(state: &PlaneState, headers: &HeaderMap) -> Result<(), PlaneHttpError> {
    // (a) Disabled ⇒ the same indistinguishable 404 a missing route answers — an unconfigured plane never
    //     exposes the lane. Checked FIRST, before any parse or authority touch.
    if !state.internal_token_configured() {
        return Err(PlaneHttpError::MissingReadCredential);
    }
    // (b) Configured ⇒ an honest 401 on a missing/malformed/wrong bearer token (a composition's own shared
    //     secret, not an object-existence oracle — the same scoped exception the admin-token route makes).
    let provided = crate::wire::bearer_token(headers).map_err(|_| PlaneHttpError::Unauthorized)?;
    if !state.internal_token_matches(&provided) {
        return Err(PlaneHttpError::Unauthorized);
    }
    Ok(())
}

/// The ONE lane guard: the unconfigured-lane 404 → the internal-token 401 → the acting-principal 400, in
/// that order, so an unconfigured plane's response carries no oracle (auth is decided BEFORE any body or
/// id parse). Returns the session-verified acting email on success.
fn acting_principal(state: &PlaneState, headers: &HeaderMap) -> Result<String, PlaneHttpError> {
    lane_guard(state, headers)?;
    // (c) The session-verified acting principal — the composing surface proves it; the wrappers re-verify the
    //     roster rows in-transaction. A missing/empty header is a 400 (only ever reachable past a correct
    //     bearer, so it is never an oracle).
    let acting = headers
        .get(ACTING_EMAIL_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| PlaneHttpError::BadBody("missing acting principal".to_owned()))?;
    Ok(acting.to_owned())
}

/// Map a session wrapper's `anyhow` fault to the plane's uniform **500** path. The session wrappers return
/// `Err` ONLY for a genuine internal fault (an unparseable plane mode, a store fault) — every protocol
/// refusal is a typed summary. The error mapper logs the full source chain; the flat wire body stays
/// "internal store error", never leaking a detail.
fn internal_fault(error: anyhow::Error) -> PlaneHttpError {
    PlaneHttpError::Authority(AuthorityError::Internal(error.into()))
}

// ── response bodies (verbatim bytes / octet-stream, all `no-store`) ────────────────────────────────────

/// A `no-store` `application/json` response carrying verbatim body bytes (the stored record / a pre-serialized
/// wire body — the composing surface relays them unchanged, so the lane never re-encodes what a `/v1` route
/// already froze).
fn json_verbatim(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/json"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        bytes,
    )
        .into_response()
}

/// A `no-store` `application/octet-stream` response carrying raw object bytes.
fn octet_stream(bytes: Vec<u8>) -> Response {
    (
        StatusCode::OK,
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        ],
        bytes,
    )
        .into_response()
}

// ── reads (GET; `no-store`; every wrapper NotFound → the uniform 404) ──────────────────────────────────

/// `GET /internal/v1/workspaces/{ws}/skills/{skill}/current` — the stored `WireCurrentRecord` JSON verbatim.
pub(crate) async fn read_current(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    match state
        .read_current_session(&ws, &skill, &acting)
        .await
        .map_err(internal_fault)?
    {
        SessionCurrentSummary::Current { record } => Ok(json_verbatim(record)),
        SessionCurrentSummary::NotFound => Err(PlaneHttpError::MissingReadCredential),
    }
}

/// `GET /internal/v1/workspaces/{ws}/skills/{skill}/versions/{version_id}` — the wire version-metadata JSON.
pub(crate) async fn read_version(
    State(state): State<PlaneState>,
    Path((ws, skill, version_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    match state
        .read_version_session(&ws, &skill, &version_id, &acting)
        .await
        .map_err(internal_fault)?
    {
        SessionVersionSummary::Body(bytes) => Ok(json_verbatim(bytes)),
        SessionVersionSummary::NotFound => Err(PlaneHttpError::MissingReadCredential),
    }
}

/// `GET /internal/v1/workspaces/{ws}/skills/{skill}/bundles/{object_id}` — one object's raw bytes.
pub(crate) async fn read_object(
    State(state): State<PlaneState>,
    Path((ws, skill, object_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    match state
        .read_object_session(&ws, &skill, &object_id, &acting)
        .await
        .map_err(internal_fault)?
    {
        SessionObjectSummary::Bytes(bytes) => Ok(octet_stream(bytes)),
        SessionObjectSummary::NotFound => Err(PlaneHttpError::MissingReadCredential),
    }
}

/// `GET /internal/v1/workspaces/{ws}/skills/{skill}/proposals` — the open proposals, wire JSON verbatim.
pub(crate) async fn list_proposals(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    match state
        .list_proposals_session(&ws, &skill, &acting)
        .await
        .map_err(internal_fault)?
    {
        SessionProposalsSummary::Body(bytes) => Ok(json_verbatim(bytes)),
        SessionProposalsSummary::NotFound => Err(PlaneHttpError::MissingReadCredential),
    }
}

/// One proposal's detail as this lane discloses it (the [`SessionProposalDetail`] fields, `snake_case`).
#[derive(serde::Serialize)]
struct ProposalDetailResponse {
    version_id: String,
    status: String,
    base_epoch: u64,
    base_seq: u64,
    created_at: String,
    proposer: String,
    review_required: bool,
    resolved_by: Option<String>,
    resolved_reason: Option<String>,
    resolved_at: Option<String>,
}

impl From<SessionProposalDetail> for ProposalDetailResponse {
    fn from(d: SessionProposalDetail) -> Self {
        Self {
            version_id: d.version_id,
            status: d.status,
            base_epoch: d.base_epoch,
            base_seq: d.base_seq,
            created_at: d.created_at,
            proposer: d.proposer,
            review_required: d.review_required,
            resolved_by: d.resolved_by,
            resolved_reason: d.resolved_reason,
            resolved_at: d.resolved_at,
        }
    }
}

/// `GET /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}` — one proposal's detail.
pub(crate) async fn read_proposal(
    State(state): State<PlaneState>,
    Path((ws, skill, version_id)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    match state
        .read_proposal_session(&ws, &skill, &version_id, &acting)
        .await
        .map_err(internal_fault)?
    {
        SessionProposalDetailSummary::Detail(detail) => Ok((
            StatusCode::OK,
            [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
            Json(ProposalDetailResponse::from(*detail)),
        )
            .into_response()),
        SessionProposalDetailSummary::NotFound => Err(PlaneHttpError::MissingReadCredential),
    }
}

// ── writes (200 all-outcome bodies, except the idempotent 204 policy set) ──────────────────────────────

/// Parse a lane-local request body AFTER the guard (a raw-`Bytes` parse, never a body extractor — an
/// extractor rejection would answer 400 before the guard runs, making a disabled route observable).
fn parse_body<T: serde::de::DeserializeOwned>(body: &Bytes) -> Result<T, PlaneHttpError> {
    serde_json::from_slice(body)
        .map_err(|e| PlaneHttpError::BadBody(format!("malformed request body: {e}")))
}

/// `POST /internal/v1/workspaces` — stand a workspace up for an already-verified owner email.
#[derive(serde::Deserialize)]
struct CreateWorkspaceRequest {
    request_id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum CreateWorkspaceResponse {
    Created {
        workspace_id: String,
        display_name: String,
        address: String,
    },
    Replayed {
        workspace_id: String,
        display_name: String,
        address: String,
    },
    Denied {
        reason: String,
    },
}

pub(crate) async fn create_workspace(
    State(state): State<PlaneState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let req: CreateWorkspaceRequest = parse_body(&body)?;
    let summary = state
        .create_workspace(
            &req.request_id,
            req.display_name.as_deref(),
            req.name.as_deref(),
            &acting,
        )
        .await
        .map_err(internal_fault)?;
    let resp = match summary {
        CreateWorkspaceSummary::Created {
            workspace_id,
            display_name,
            address,
        } => CreateWorkspaceResponse::Created {
            workspace_id,
            display_name,
            address,
        },
        CreateWorkspaceSummary::Replayed {
            workspace_id,
            display_name,
            address,
        } => CreateWorkspaceResponse::Replayed {
            workspace_id,
            display_name,
            address,
        },
        CreateWorkspaceSummary::Denied { reason } => CreateWorkspaceResponse::Denied { reason },
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `POST /internal/v1/device-sessions/{user_code}/approve` — the member/owner web-approve of an enroll
/// session (first-writer-wins; `not_found` stays a 200 body outcome so the composing page renders the
/// uniform miss itself, never an HTTP 404).
#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum ApproveSessionResponse {
    Confirmed,
    NotFound,
}

pub(crate) async fn approve_session(
    State(state): State<PlaneState>,
    Path(user_code): Path<String>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let resp = match state
        .approve_session(&user_code, &acting)
        .await
        .map_err(internal_fault)?
    {
        ApproveSessionSummary::Confirmed => ApproveSessionResponse::Confirmed,
        ApproveSessionSummary::NotFound => ApproveSessionResponse::NotFound,
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `POST /internal/v1/device-sessions/{user_code}/approve-standup` — the human leg of the workspace-standup
/// door for an already-verified email.
#[derive(serde::Deserialize)]
struct ApproveStandupRequest {
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum ApproveStandupResponse {
    Approved {
        workspace_id: String,
        display_name: String,
    },
    AlreadyApproved {
        workspace_id: String,
    },
    Denied {
        reason: String,
    },
    NotFound,
}

pub(crate) async fn approve_standup(
    State(state): State<PlaneState>,
    Path(user_code): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let req: ApproveStandupRequest = parse_body(&body)?;
    let resp = match state
        .approve_standup(
            &user_code,
            &acting,
            req.display_name.as_deref(),
            req.name.as_deref(),
        )
        .await
        .map_err(internal_fault)?
    {
        ApproveStandupSummary::Approved {
            workspace_id,
            display_name,
        } => ApproveStandupResponse::Approved {
            workspace_id,
            display_name,
        },
        ApproveStandupSummary::AlreadyApproved { workspace_id } => {
            ApproveStandupResponse::AlreadyApproved { workspace_id }
        }
        ApproveStandupSummary::Denied { reason } => ApproveStandupResponse::Denied { reason },
        ApproveStandupSummary::NotFound => ApproveStandupResponse::NotFound,
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `POST /internal/v1/workspaces/{ws}/roster/remove` — remove a member (idempotent; last-owner denied).
#[derive(serde::Deserialize)]
struct RemoveMemberRequest {
    request_id: String,
    email: String,
}

#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum RemoveMemberResponse {
    Removed,
    Denied { reason: String },
}

pub(crate) async fn remove_member(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let req: RemoveMemberRequest = parse_body(&body)?;
    let resp = match state
        .remove_member(&ws, &req.request_id, &acting, &req.email)
        .await
        .map_err(internal_fault)?
    {
        RemoveMemberSummary::Removed => RemoveMemberResponse::Removed,
        RemoveMemberSummary::Denied { reason } => RemoveMemberResponse::Denied { reason },
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// The pointer-move family's all-outcome response (approve / reject / revert). Each op's summary maps only to
/// the outcomes it can produce; the shared enum keeps the wire shape identical across the three.
#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum PointerMoveResponse {
    Approved,
    Rejected,
    Reverted,
    Conflict,
    NotFound,
    Denied { reason: String },
}

fn review_response(summary: SessionReviewSummary) -> PointerMoveResponse {
    match summary {
        SessionReviewSummary::Approved => PointerMoveResponse::Approved,
        SessionReviewSummary::Rejected => PointerMoveResponse::Rejected,
        SessionReviewSummary::Conflict => PointerMoveResponse::Conflict,
        SessionReviewSummary::Denied { reason } => PointerMoveResponse::Denied { reason },
        SessionReviewSummary::NotFound => PointerMoveResponse::NotFound,
    }
}

/// `POST /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}/approve`.
#[derive(serde::Deserialize)]
struct ApproveProposalRequest {
    request_id: String,
    expected_epoch: u64,
    expected_seq: u64,
}

pub(crate) async fn approve_proposal(
    State(state): State<PlaneState>,
    Path((ws, skill, version_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let req: ApproveProposalRequest = parse_body(&body)?;
    let summary = state
        .review_approve_session(
            &ws,
            &skill,
            &version_id,
            req.expected_epoch,
            req.expected_seq,
            &req.request_id,
            &acting,
        )
        .await
        .map_err(internal_fault)?;
    Ok((StatusCode::OK, Json(review_response(summary))).into_response())
}

/// `POST /internal/v1/workspaces/{ws}/skills/{skill}/proposals/{version_id}/reject`. An empty `reason` passes
/// through — the wrapper synthesizes the typed denial.
#[derive(serde::Deserialize)]
struct RejectProposalRequest {
    request_id: String,
    expected_epoch: u64,
    expected_seq: u64,
    reason: String,
}

pub(crate) async fn reject_proposal(
    State(state): State<PlaneState>,
    Path((ws, skill, version_id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let req: RejectProposalRequest = parse_body(&body)?;
    let summary = state
        .review_reject_session(
            &ws,
            &skill,
            &version_id,
            req.expected_epoch,
            req.expected_seq,
            &req.reason,
            &req.request_id,
            &acting,
        )
        .await
        .map_err(internal_fault)?;
    Ok((StatusCode::OK, Json(review_response(summary))).into_response())
}

// ── the skill-lifecycle ceremonies (owner acts; every route keys on the IMMUTABLE skill id, never the
// mutable catalog name — the composing surface resolves name→id in its own loader, so a concurrent
// rename is a harmless miss, never a wrong-target act) ─────────────────────────────────────────────────

/// The lifecycle family's DENIED/NOT_FOUND arms, shared by the five ceremony routes; each success arm
/// is route-specific. `reason` is the guarded function's outcome code VERBATIM (`owner_role_required`,
/// `not_active`, `not_archived`, `name_taken`, `bad_name`, `is_current`, `already_purged`).
#[derive(serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum LifecycleResponse {
    Archived { archived_name: String },
    Unarchived { name: String },
    Deleted,
    Purged,
    Renamed { name: String },
    Denied { reason: String },
    NotFound,
}

/// `POST /internal/v1/workspaces/{ws}/skills/{skill}/archive` — retire the skill for the whole team
/// (rename + free the base name, unplace everywhere, close open proposals with author notices).
pub(crate) async fn archive_skill(
    State(state): State<PlaneState>,
    Path((ws, skill_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let resp = match state
        .archive_skill_session(&ws, &acting, &skill_id)
        .await
        .map_err(internal_fault)?
    {
        ArchiveSkillSummary::Archived { archived_name } => {
            LifecycleResponse::Archived { archived_name }
        }
        ArchiveSkillSummary::Denied { reason } => LifecycleResponse::Denied { reason },
        ArchiveSkillSummary::NotFound => LifecycleResponse::NotFound,
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `POST /internal/v1/workspaces/{ws}/skills/{skill}/unarchive` — rename back to the base name
/// (refused `name_taken` when a new identity claimed it).
pub(crate) async fn unarchive_skill(
    State(state): State<PlaneState>,
    Path((ws, skill_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let resp = match state
        .unarchive_skill_session(&ws, &acting, &skill_id)
        .await
        .map_err(internal_fault)?
    {
        UnarchiveSkillSummary::Unarchived { name } => LifecycleResponse::Unarchived { name },
        UnarchiveSkillSummary::Denied { reason } => LifecycleResponse::Denied { reason },
        UnarchiveSkillSummary::NotFound => LifecycleResponse::NotFound,
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `POST /internal/v1/workspaces/{ws}/skills/{skill}/delete` — tombstone an ARCHIVED skill
/// (archive-first; deletion cannot recall device copies).
pub(crate) async fn delete_skill(
    State(state): State<PlaneState>,
    Path((ws, skill_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let resp = match state
        .delete_skill_session(&ws, &acting, &skill_id)
        .await
        .map_err(internal_fault)?
    {
        DeleteSkillSummary::Deleted => LifecycleResponse::Deleted,
        DeleteSkillSummary::Denied { reason } => LifecycleResponse::Denied { reason },
        DeleteSkillSummary::NotFound => LifecycleResponse::NotFound,
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `POST /internal/v1/workspaces/{ws}/skills/{skill}/purge` — un-root ONE version's bytes (refused
/// `is_current` on the live pointer). The body names the version; it is parsed AFTER the guard, and a
/// version id that is not 64 lowercase-hex chars is a malformed BODY (400), not an unknown object.
#[derive(serde::Deserialize)]
struct PurgeVersionRequest {
    version_id: String,
}

pub(crate) async fn purge_version(
    State(state): State<PlaneState>,
    Path((ws, skill_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let req: PurgeVersionRequest = parse_body(&body)?;
    if req.version_id.len() != 64
        || !req
            .version_id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(PlaneHttpError::BadBody(
            "version_id must be 64 lowercase hex characters".to_owned(),
        ));
    }
    let resp = match state
        .purge_version_session(&ws, &acting, &skill_id, &req.version_id)
        .await
        .map_err(internal_fault)?
    {
        PurgeVersionSummary::Purged => LifecycleResponse::Purged,
        PurgeVersionSummary::Denied { reason } => LifecycleResponse::Denied { reason },
        PurgeVersionSummary::NotFound => LifecycleResponse::NotFound,
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `POST /internal/v1/workspaces/{ws}/skills/{skill}/rename` — move the user-facing catalog name;
/// the identity and every id-keyed reference survive, and the old name keeps resolving as a redirect.
/// A rule-breaking name is the guarded function's `bad_name` DENIED (a member-entitled answer), never
/// a transport 400.
#[derive(serde::Deserialize)]
struct RenameSkillRequest {
    new_name: String,
}

pub(crate) async fn rename_skill(
    State(state): State<PlaneState>,
    Path((ws, skill_id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let req: RenameSkillRequest = parse_body(&body)?;
    let resp = match state
        .rename_skill_session(&ws, &acting, &skill_id, &req.new_name)
        .await
        .map_err(internal_fault)?
    {
        RenameSkillSummary::Renamed { name } => LifecycleResponse::Renamed { name },
        RenameSkillSummary::Denied { reason } => LifecycleResponse::Denied { reason },
        RenameSkillSummary::NotFound => LifecycleResponse::NotFound,
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

/// `POST /internal/v1/workspaces/{ws}/skills/{skill}/reverts` — the web one-click roll-back to a good version.
#[derive(serde::Deserialize)]
struct RevertRequest {
    request_id: String,
    good_version_id: String,
    expected_epoch: u64,
    expected_seq: u64,
}

pub(crate) async fn revert(
    State(state): State<PlaneState>,
    Path((ws, skill)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    let acting = acting_principal(&state, &headers)?;
    let req: RevertRequest = parse_body(&body)?;
    let resp = match state
        .revert_session(
            &ws,
            &skill,
            &req.good_version_id,
            req.expected_epoch,
            req.expected_seq,
            &req.request_id,
            &acting,
        )
        .await
        .map_err(internal_fault)?
    {
        SessionRevertSummary::Reverted => PointerMoveResponse::Reverted,
        SessionRevertSummary::Conflict => PointerMoveResponse::Conflict,
        SessionRevertSummary::Denied { reason } => PointerMoveResponse::Denied { reason },
        SessionRevertSummary::NotFound => PointerMoveResponse::NotFound,
    };
    Ok((StatusCode::OK, Json(resp)).into_response())
}

// ── the pre-identity passcode mint (the composing surface delivers what this returns) ──────────────────

/// `POST /internal/v1/enroll/passcode` body — the live session's user code + the address to verify.
#[derive(serde::Deserialize)]
struct PasscodeMintRequest {
    user_code: String,
    email: String,
}

/// The minted passcode + the workspace display name the mail renders with. The plaintext code crosses HERE
/// ONCE — bearer-gated, `no-store`, never logged (the trace layer records route templates only); the
/// composing surface mails it and the vault holds no mail transport.
#[derive(serde::Serialize)]
struct PasscodeMintResponse {
    passcode: String,
    workspace_display_name: String,
}

/// `POST /internal/v1/enroll/passcode` — mint the passcode second factor for a live device-auth session.
///
/// Deliberately [`lane_guard`]-only, no acting principal: the op is PRE-IDENTITY — the body's email is the
/// UNVERIFIED subject the passcode will prove (parsed inside the authority op, never a handler
/// `Principal::parse`), not a session-verified actor. The PUBLIC wire keeps its shape at the composing
/// surface: the app serves `POST /v1/enroll/passcode`'s constant-shaped ack itself (the [`routes::door`]
/// stub pins that contract) and fire-and-forgets the send through its mail seam, so neither the ack body
/// nor its latency says whether the address was rostered.
///
/// [`routes::door`]: super::door
pub(crate) async fn mint_passcode(
    State(state): State<PlaneState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, PlaneHttpError> {
    lane_guard(&state, &headers)?;
    let req: PasscodeMintRequest = parse_body(&body)?;
    let (created_at, now) = crate::wire::now_utc();
    // The verification context supplies the workspace name for the mail body (and confirms the session is
    // live — the same indistinguishable 404 a dead user code answers on the public verify read).
    let context = state
        .authority()
        .read_verification_context(&req.user_code, now)
        .await?;
    let started = state
        .authority()
        .start_passcode(&req.user_code, &req.email, now, &created_at)
        .await?;
    Ok((
        StatusCode::OK,
        [(header::CACHE_CONTROL, HeaderValue::from_static("no-store"))],
        Json(PasscodeMintResponse {
            passcode: started.passcode,
            workspace_display_name: context.workspace_display_name,
        }),
    )
        .into_response())
}
