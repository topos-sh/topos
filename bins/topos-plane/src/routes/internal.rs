//! The internal custody lane — the vault's ONLY HTTP surface besides `/healthz`.
//!
//! Every route sits under `/internal/v1`, gated by the ONE internal bearer token (unconfigured ⇒
//! the uniform 404, wrong ⇒ 401 — decided in the router middleware before any handler runs). The
//! ONE caller is the composing product app, which has already authorized the request; handlers are
//! thin: parse shape → call the authority → serialize.
//!
//! The request/response DTOs are **lane-local** (snake_case serde) — deliberately NOT in
//! `topos-types` and NOT in the committed OpenAPI: the lane is composition-internal. Ids ride as
//! opaque strings + 64-hex content ids; file bytes ride base64 in JSON (the same encoding the
//! public candidate wire uses).
//!
//! Error mapping (uniform, typed):
//! - 400 `BAD_REQUEST`  — a malformed id / body / attribution;
//! - 400 `REJECTED`     — a refused candidate (canonical rules, unknown parent, size cap, denylist,
//!   the lineage fence);
//! - 404 `NOT_FOUND`    — the uniform miss (single caller, so consistency is the whole discipline);
//! - 409 `CONFLICT`     — a lost pointer CAS, carrying the live `(generation, version_id)`;
//! - 409 `TARGET_PURGED` / 409 `POINTED_AT` — the typed purge/revert refusals;
//! - 500 `INTEGRITY` / `INTERNAL` — store corruption / infrastructure faults (chains logged
//!   server-side, never on the wire).

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use plane_store::{
    Authority, AuthorityError, BundleId, CandidateUpload, CommitId, FileMode, ObjectId,
    UploadedFile, WorkspaceId,
};

use crate::state::PlaneState;
use crate::wire::{error_chain, now_ms};

// ── the lane-local error ─────────────────────────────────────────────────────────────────────────

/// A typed lane error → one JSON body `{ code, message?, generation?, version_id? }` under the
/// matching status.
pub(crate) struct LaneError {
    status: StatusCode,
    code: &'static str,
    message: Option<String>,
    live: Option<(u64, String)>,
}

impl LaneError {
    pub(crate) fn not_found() -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code: "NOT_FOUND",
            message: None,
            live: None,
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: "BAD_REQUEST",
            message: Some(message.into()),
            live: None,
        }
    }
}

impl From<AuthorityError> for LaneError {
    fn from(e: AuthorityError) -> Self {
        match e {
            AuthorityError::NotFound => LaneError::not_found(),
            AuthorityError::InvalidId(err) => LaneError::bad_request(err.to_string()),
            AuthorityError::RejectedUpload(msg) => LaneError {
                status: StatusCode::BAD_REQUEST,
                code: "REJECTED",
                message: Some(msg),
                live: None,
            },
            AuthorityError::Conflict(live) => LaneError {
                status: StatusCode::CONFLICT,
                code: "CONFLICT",
                message: None,
                live: live.map(|l| (l.generation, l.version_id.to_hex())),
            },
            AuthorityError::TargetPurged => LaneError {
                status: StatusCode::CONFLICT,
                code: "TARGET_PURGED",
                message: None,
                live: None,
            },
            AuthorityError::PointedAt => LaneError {
                status: StatusCode::CONFLICT,
                code: "POINTED_AT",
                message: None,
                live: None,
            },
            // The boxed chains are server-side diagnostics; the wire stays flat. Log them HERE (the
            // event fires inside the router's request span, so it correlates).
            AuthorityError::Integrity(_) => {
                tracing::error!(error = %error_chain(&e), "custody integrity fault");
                LaneError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    code: "INTEGRITY",
                    message: None,
                    live: None,
                }
            }
            _ => {
                tracing::error!(error = %error_chain(&e), "custody internal fault");
                LaneError {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    code: "INTERNAL",
                    message: None,
                    live: None,
                }
            }
        }
    }
}

impl IntoResponse for LaneError {
    fn into_response(self) -> Response {
        #[derive(Serialize)]
        struct Body {
            code: &'static str,
            #[serde(skip_serializing_if = "Option::is_none")]
            message: Option<String>,
            #[serde(skip_serializing_if = "Option::is_none")]
            generation: Option<u64>,
            #[serde(skip_serializing_if = "Option::is_none")]
            version_id: Option<String>,
        }
        let (generation, version_id) = match self.live {
            Some((g, v)) => (Some(g), Some(v)),
            None => (None, None),
        };
        (
            self.status,
            Json(Body {
                code: self.code,
                message: self.message,
                generation,
                version_id,
            }),
        )
            .into_response()
    }
}

type LaneResult<T> = Result<T, LaneError>;

// ── shape parsing (charset/length only — never meaning) ─────────────────────────────────────────

fn parse_ws(s: &str) -> LaneResult<WorkspaceId> {
    WorkspaceId::parse(s).map_err(|e| LaneError::bad_request(format!("workspace_id: {e}")))
}

fn parse_bundle(s: &str) -> LaneResult<BundleId> {
    BundleId::parse(s).map_err(|e| LaneError::bad_request(format!("bundle_id: {e}")))
}

/// A content id in a PATH: a malformed spelling is simply not a known id — the uniform 404.
fn parse_version_path(s: &str) -> LaneResult<CommitId> {
    CommitId::parse_hex(s).ok_or_else(LaneError::not_found)
}

/// A content id in a BODY: malformed is a 400 (the caller composed the body; tell it).
fn parse_version_body(s: &str) -> LaneResult<CommitId> {
    CommitId::parse_hex(s)
        .ok_or_else(|| LaneError::bad_request("version id must be 64 lowercase hex characters"))
}

// ── the lane-local DTOs ──────────────────────────────────────────────────────────────────────────

/// One candidate file: path + mode + base64 bytes (the server decodes and REHASHES every byte — no
/// client-claimed id exists on this wire at all).
#[derive(Debug, Deserialize)]
pub(crate) struct LaneFile {
    path: String,
    /// `"100644"` (regular) or `"100755"` (executable).
    mode: String,
    content_base64: String,
}

/// `POST …/versions` — ingest + commit WITHOUT moving the pointer (the propose path).
#[derive(Debug, Deserialize)]
pub(crate) struct CommitRequest {
    files: Vec<LaneFile>,
    /// The candidate's parent version (64-hex); absent = genesis.
    #[serde(default)]
    parent: Option<String>,
    /// The attribution display string recorded verbatim (the commit frame's author +
    /// `author_display`).
    attribution: String,
    /// The commit message.
    message: String,
}

/// `POST …/publish` — the composite: ingest + commit + CAS pointer move, one flow.
#[derive(Debug, Deserialize)]
pub(crate) struct PublishRequest {
    #[serde(flatten)]
    candidate: CommitRequest,
    /// `None` = genesis (creates the pointer at generation 1); `Some(g)` = the CAS.
    #[serde(default)]
    expected_generation: Option<u64>,
}

/// `POST …/pointer` — CAS move to an EXISTING version (the approve path).
#[derive(Debug, Deserialize)]
pub(crate) struct PointerMoveRequest {
    version_id: String,
    #[serde(default)]
    expected_generation: Option<u64>,
    attribution: String,
}

/// `POST …/revert` — forward commit `{tree: target.tree, parents: [current]}` + CAS move.
#[derive(Debug, Deserialize)]
pub(crate) struct RevertRequest {
    to_version_id: String,
    expected_generation: u64,
    attribution: String,
    /// The forward-revert commit message, recorded verbatim (the frame's inputs are the wire's —
    /// a device pre-derives the forward id and verifies the move landed on exactly that version).
    message: String,
}

/// `POST …/versions/{version_id}/purge` — the byte purge.
#[derive(Debug, Deserialize)]
pub(crate) struct PurgeRequest {
    attribution: String,
}

/// The committed-version answer (`versions` returns exactly this; `publish`/`revert` extend it).
/// `version_id` and `commit_id` carry the same value today — a version IS its commit; both fields
/// exist so the identities could ever diverge without a wire break.
#[derive(Debug, Serialize)]
pub(crate) struct CommitResponse {
    version_id: String,
    commit_id: String,
    bundle_digest: String,
    deduped: bool,
}

/// A pointer state (the move answers; `replayed` marks the idempotent-CAS carve-out).
#[derive(Debug, Serialize)]
pub(crate) struct PointerResponse {
    version_id: String,
    generation: u64,
    moved_at_ms: i64,
    moved_by_display: String,
    replayed: bool,
}

/// The publish/revert answer: the committed version + the moved pointer.
#[derive(Debug, Serialize)]
pub(crate) struct PublishResponse {
    #[serde(flatten)]
    version: CommitResponse,
    pointer: PointerResponse,
}

/// `GET …/current` — the pointer record + the pointed version's consent digest.
#[derive(Debug, Serialize)]
pub(crate) struct CurrentResponse {
    version_id: String,
    generation: u64,
    moved_at_ms: i64,
    moved_by_display: String,
    bundle_digest: String,
}

/// One file of a version listing (no bytes; the object read serves those).
#[derive(Debug, Serialize)]
pub(crate) struct VersionFileResponse {
    path: String,
    mode: &'static str,
    object_id: String,
}

/// `GET …/versions/{version_id}` — meta + file listing.
#[derive(Debug, Serialize)]
pub(crate) struct VersionResponse {
    version_id: String,
    parents: Vec<String>,
    author: String,
    message: String,
    bundle_digest: String,
    created_at_ms: i64,
    files: Vec<VersionFileResponse>,
}

/// One hop of the log.
#[derive(Debug, Serialize)]
pub(crate) struct LogEntryResponse {
    version_id: String,
    message: String,
    author_display: String,
    created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    purged_at_ms: Option<i64>,
}

/// `GET …/log` — the first-parent chain from current, newest first, capped.
#[derive(Debug, Serialize)]
pub(crate) struct LogResponse {
    versions: Vec<LogEntryResponse>,
}

/// The log's `?limit=` query (clamped to the authority's cap).
#[derive(Debug, Deserialize)]
pub(crate) struct LogQuery {
    #[serde(default)]
    limit: Option<usize>,
}

/// `POST …/purge` — what the purge did.
#[derive(Debug, Serialize)]
pub(crate) struct PurgeResponse {
    tombstoned: usize,
    reclaimed: usize,
}

/// `DELETE …/bundles/{bundle}` — what the bundle reclaim did.
#[derive(Debug, Serialize)]
pub(crate) struct BundleDeleteResponse {
    versions_dropped: u64,
    objects_reclaimed: usize,
}

// ── DTO → domain assembly ────────────────────────────────────────────────────────────────────────

fn assemble_candidate(req: CommitRequest) -> LaneResult<CandidateUpload> {
    let mut files = Vec::with_capacity(req.files.len());
    for f in req.files {
        let mode = match f.mode.as_str() {
            "100644" => FileMode::Regular,
            "100755" => FileMode::Executable,
            other => {
                return Err(LaneError::bad_request(format!(
                    "unknown file mode {other:?} (expected \"100644\" or \"100755\")"
                )));
            }
        };
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&f.content_base64)
            .map_err(|_| LaneError::bad_request("content_base64 is not valid base64"))?;
        files.push(UploadedFile {
            path: f.path,
            mode,
            bytes,
        });
    }
    let parent = req.parent.as_deref().map(parse_version_body).transpose()?;
    Ok(CandidateUpload {
        files,
        parent,
        attribution: req.attribution,
        message: req.message,
    })
}

fn commit_response(v: &plane_store::CommittedVersion) -> CommitResponse {
    let hex = v.version_id.to_hex();
    CommitResponse {
        commit_id: hex.clone(),
        version_id: hex,
        bundle_digest: plane_store::ObjectId(v.bundle_digest).to_hex(),
        deduped: v.deduped,
    }
}

fn pointer_response(p: &plane_store::PointerState) -> PointerResponse {
    PointerResponse {
        version_id: p.version_id.to_hex(),
        generation: p.generation,
        moved_at_ms: p.moved_at_ms,
        moved_by_display: p.moved_by.clone(),
        replayed: p.replayed,
    }
}

fn authority(state: &PlaneState) -> &Authority {
    state.authority()
}

// ── the handlers ─────────────────────────────────────────────────────────────────────────────────

/// `POST /internal/v1/workspaces/{ws}/bundles/{bundle}/versions` — ingest + commit (no pointer
/// move). Idempotent per content: an identical candidate answers the same ids with `deduped`.
pub(crate) async fn commit_version(
    State(state): State<PlaneState>,
    Path((ws, bundle)): Path<(String, String)>,
    Json(req): Json<CommitRequest>,
) -> LaneResult<Json<CommitResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let candidate = assemble_candidate(req)?;
    let version = authority(&state)
        .commit_version(&ws, &bundle, candidate, now_ms())
        .await?;
    Ok(Json(commit_response(&version)))
}

/// `POST /internal/v1/workspaces/{ws}/bundles/{bundle}/publish` — ingest + commit + CAS, one flow.
pub(crate) async fn publish(
    State(state): State<PlaneState>,
    Path((ws, bundle)): Path<(String, String)>,
    Json(req): Json<PublishRequest>,
) -> LaneResult<Json<PublishResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let expected = req.expected_generation;
    let candidate = assemble_candidate(req.candidate)?;
    let (version, pointer) = authority(&state)
        .publish(&ws, &bundle, candidate, expected, now_ms())
        .await?;
    Ok(Json(PublishResponse {
        version: commit_response(&version),
        pointer: pointer_response(&pointer),
    }))
}

/// `POST /internal/v1/workspaces/{ws}/bundles/{bundle}/pointer` — CAS to an existing version.
pub(crate) async fn move_pointer(
    State(state): State<PlaneState>,
    Path((ws, bundle)): Path<(String, String)>,
    Json(req): Json<PointerMoveRequest>,
) -> LaneResult<Json<PointerResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let version = parse_version_body(&req.version_id)?;
    let pointer = authority(&state)
        .move_pointer(
            &ws,
            &bundle,
            version,
            req.expected_generation,
            &req.attribution,
            now_ms(),
        )
        .await?;
    Ok(Json(pointer_response(&pointer)))
}

/// `POST /internal/v1/workspaces/{ws}/bundles/{bundle}/revert` — the forward revert.
pub(crate) async fn revert(
    State(state): State<PlaneState>,
    Path((ws, bundle)): Path<(String, String)>,
    Json(req): Json<RevertRequest>,
) -> LaneResult<Json<PublishResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let to = parse_version_body(&req.to_version_id)?;
    let (version, pointer) = authority(&state)
        .revert(
            &ws,
            &bundle,
            to,
            req.expected_generation,
            &req.attribution,
            &req.message,
            now_ms(),
        )
        .await?;
    Ok(Json(PublishResponse {
        version: commit_response(&version),
        pointer: pointer_response(&pointer),
    }))
}

/// `GET /internal/v1/workspaces/{ws}/bundles/{bundle}/current`.
pub(crate) async fn read_current(
    State(state): State<PlaneState>,
    Path((ws, bundle)): Path<(String, String)>,
) -> LaneResult<Json<CurrentResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let current = authority(&state)
        .read_current(&ws, &bundle)
        .await?
        .ok_or_else(LaneError::not_found)?;
    Ok(Json(CurrentResponse {
        version_id: current.version_id.to_hex(),
        generation: current.generation,
        moved_at_ms: current.moved_at_ms,
        moved_by_display: current.moved_by,
        bundle_digest: ObjectId(current.bundle_digest).to_hex(),
    }))
}

/// `GET /internal/v1/workspaces/{ws}/bundles/{bundle}/versions/{version_id}` — meta + file listing.
pub(crate) async fn read_version(
    State(state): State<PlaneState>,
    Path((ws, bundle, version)): Path<(String, String, String)>,
) -> LaneResult<Json<VersionResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let version = parse_version_path(&version)?;
    let meta = authority(&state)
        .read_version(&ws, &bundle, version)
        .await?;
    Ok(Json(VersionResponse {
        version_id: CommitId(meta.version_id).to_hex(),
        parents: meta.parents.iter().map(|p| CommitId(*p).to_hex()).collect(),
        author: meta.author,
        message: meta.message,
        bundle_digest: ObjectId(meta.bundle_digest).to_hex(),
        created_at_ms: meta.created_at_ms,
        files: meta
            .files
            .into_iter()
            .map(|f| VersionFileResponse {
                path: f.path,
                mode: match f.mode {
                    FileMode::Regular => "100644",
                    FileMode::Executable => "100755",
                },
                object_id: ObjectId(f.object_id).to_hex(),
            })
            .collect(),
    }))
}

/// `GET /internal/v1/workspaces/{ws}/bundles/{bundle}/objects/{object_id}` — one object's verified
/// bytes (`application/octet-stream`). Served only through a bundle whose live version reaches it.
pub(crate) async fn read_object(
    State(state): State<PlaneState>,
    Path((ws, bundle, object)): Path<(String, String, String)>,
) -> LaneResult<Response> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let object = ObjectId::parse_hex(&object).ok_or_else(LaneError::not_found)?;
    let bytes = authority(&state).read_object(&ws, &bundle, object).await?;
    Ok((
        [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
        bytes,
    )
        .into_response())
}

/// `GET /internal/v1/workspaces/{ws}/bundles/{bundle}/log?limit=N` — the first-parent chain.
pub(crate) async fn read_log(
    State(state): State<PlaneState>,
    Path((ws, bundle)): Path<(String, String)>,
    Query(query): Query<LogQuery>,
) -> LaneResult<Json<LogResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let limit = query
        .limit
        .unwrap_or(plane_store::DEFAULT_LOG_LIMIT)
        .clamp(1, plane_store::DEFAULT_LOG_LIMIT);
    let entries = authority(&state).log(&ws, &bundle, limit).await?;
    Ok(Json(LogResponse {
        versions: entries
            .into_iter()
            .map(|e| LogEntryResponse {
                version_id: e.version_id.to_hex(),
                message: e.message,
                author_display: e.author_display,
                created_at_ms: e.created_at_ms,
                purged_at_ms: e.purged_at_ms,
            })
            .collect(),
    }))
}

/// `POST /internal/v1/workspaces/{ws}/bundles/{bundle}/versions/{version_id}/purge` — the byte
/// purge (refused typed while pointed-at; idempotent once purged).
pub(crate) async fn purge_version(
    State(state): State<PlaneState>,
    Path((ws, bundle, version)): Path<(String, String, String)>,
    Json(req): Json<PurgeRequest>,
) -> LaneResult<Json<PurgeResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let version = parse_version_path(&version)?;
    let report = authority(&state)
        .purge_version(&ws, &bundle, version, &req.attribution, now_ms())
        .await?;
    Ok(Json(PurgeResponse {
        tombstoned: report.tombstoned,
        reclaimed: report.reclaimed,
    }))
}

/// `DELETE /internal/v1/workspaces/{ws}/bundles/{bundle}` — bundle GC on app instruction.
pub(crate) async fn delete_bundle(
    State(state): State<PlaneState>,
    Path((ws, bundle)): Path<(String, String)>,
) -> LaneResult<Json<BundleDeleteResponse>> {
    let (ws, bundle) = (parse_ws(&ws)?, parse_bundle(&bundle)?);
    let report = authority(&state)
        .delete_bundle(&ws, &bundle, now_ms())
        .await?;
    Ok(Json(BundleDeleteResponse {
        versions_dropped: report.versions_dropped,
        objects_reclaimed: report.objects_reclaimed,
    }))
}

/// `DELETE /internal/v1/workspaces/{ws}` — workspace reclaim (rows + physical stores).
pub(crate) async fn delete_workspace(
    State(state): State<PlaneState>,
    Path(ws): Path<String>,
) -> LaneResult<StatusCode> {
    let ws = parse_ws(&ws)?;
    authority(&state).delete_workspace(&ws).await?;
    Ok(StatusCode::NO_CONTENT)
}
