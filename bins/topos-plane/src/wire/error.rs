//! The non-2xx mapping — every transport/auth/integrity fault → an HTTP status + a uniform [`JsonEnvelope`]
//! error body (so curl/tooling sees the same shape every protocol outcome uses).
//!
//! Design rule: a returned protocol outcome (OK / CONFLICT / DENIED / …) is ALWAYS a 200 carrying a receipt
//! (see [`crate::wire::map::write_envelope`]); a non-2xx is ONLY a transport/auth/integrity fault:
//! - `400` — a malformed body / id / device-signature header;
//! - `404` — a missing/blank read credential OR `AuthorityError::NotFound` (indistinguishable, never
//!   401/403: the plane never reveals whether a token, workspace, skill, object, or version exists);
//! - `500` — `AuthorityError::{Integrity, Internal}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use plane_store::AuthorityError;
use topos_types::{
    ActionCode, Affected, JsonEnvelope, NextAction, SCHEMA_VERSION, TerminalOutcome, WireError,
};

/// A transport/auth/integrity fault on the way in or out of a handler. Maps to a non-2xx status with a
/// uniform envelope body.
#[derive(Debug)]
pub(crate) enum PlaneHttpError {
    /// A malformed request body / DTO / candidate field → 400.
    BadBody(String),
    /// A missing or malformed `Topos-Device-Signature` header → 400.
    BadDeviceSignature,
    /// A malformed identifier (workspace / skill / op id, or a hex commit/object id in a body) → 400.
    BadId(String),
    /// A missing/blank read credential → 404 (indistinguishable; never 401/403).
    MissingReadCredential,
    /// An authority fault carried through and mapped by variant.
    Authority(AuthorityError),
}

impl From<AuthorityError> for PlaneHttpError {
    fn from(e: AuthorityError) -> Self {
        PlaneHttpError::Authority(e)
    }
}

impl IntoResponse for PlaneHttpError {
    fn into_response(self) -> Response {
        // (status, code, retryable, next-actions). The message never leaks an internal detail: a 404 is a
        // flat "not found", a 500 a flat "internal store error".
        let (status, code, retryable, next_actions, message) = match self {
            PlaneHttpError::BadBody(m) => {
                (StatusCode::BAD_REQUEST, "BAD_REQUEST", false, vec![], m)
            }
            PlaneHttpError::BadDeviceSignature => (
                StatusCode::BAD_REQUEST,
                "BAD_DEVICE_SIGNATURE",
                false,
                vec![],
                "missing or malformed Topos-Device-Signature header".to_owned(),
            ),
            PlaneHttpError::BadId(m) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", false, vec![], m),
            PlaneHttpError::MissingReadCredential => (
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                false,
                vec![],
                "not found".to_owned(),
            ),
            PlaneHttpError::Authority(e) => match e {
                AuthorityError::NotFound => (
                    StatusCode::NOT_FOUND,
                    "NOT_FOUND",
                    false,
                    vec![],
                    "not found".to_owned(),
                ),
                AuthorityError::Integrity(_) | AuthorityError::Internal(_) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "INTERNAL",
                    true,
                    vec![retry()],
                    "internal store error".to_owned(),
                ),
                // InvalidId / RejectedUpload / Denied (+ any future variant): a refused request → 400.
                other => (
                    StatusCode::BAD_REQUEST,
                    "BAD_REQUEST",
                    false,
                    vec![],
                    other.to_string(),
                ),
            },
        };
        let outcome = if status == StatusCode::INTERNAL_SERVER_ERROR {
            TerminalOutcome::RetryableFailure
        } else {
            TerminalOutcome::PermanentFailure
        };
        let envelope = error_envelope(code, outcome, retryable, &message, next_actions);
        (status, Json(envelope)).into_response()
    }
}

/// Build the uniform error [`JsonEnvelope`] (the helper a transport fault serializes through). `command` is
/// the honest `"error"` — a transport fault fires before (or around) any verb, so there is no command to name.
fn error_envelope(
    code: &str,
    outcome: TerminalOutcome,
    retryable: bool,
    message: &str,
    next_actions: Vec<NextAction>,
) -> JsonEnvelope {
    let error = WireError {
        code: code.to_owned(),
        outcome,
        retryable,
        affected: Affected::default(),
        expected_generation: None,
        current_generation: None,
        context: serde_json::json!({ "message": message }),
        next_actions: next_actions.clone(),
    };
    JsonEnvelope {
        schema_version: SCHEMA_VERSION,
        command: "error".to_owned(),
        ok: false,
        data: serde_json::json!({}),
        warnings: vec![],
        next_actions,
        receipt: None,
        error: Some(error),
    }
}

fn retry() -> NextAction {
    NextAction {
        code: ActionCode::Retry,
        argv: vec![],
    }
}
