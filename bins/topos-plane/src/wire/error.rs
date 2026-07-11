//! The non-2xx mapping — every transport/auth/integrity fault → an HTTP status + a uniform [`JsonEnvelope`]
//! error body (so curl/tooling sees the same shape every protocol outcome uses).
//!
//! Design rule: a returned protocol outcome (OK / CONFLICT / DENIED / …) is ALWAYS a 200 carrying a receipt
//! (see [`crate::wire::map::write_envelope`]); a non-2xx is ONLY a transport/auth/integrity fault:
//! - `400` — a malformed body / id;
//! - `404` — a missing/blank read credential OR `AuthorityError::NotFound` (indistinguishable — for READ
//!   credentials the plane never answers 401/403, so it reveals nothing about whether a token, workspace,
//!   skill, object, or version exists);
//! - `401` — ONLY the operator admin-token surface (a configured route with a missing/wrong token). The
//!   404-not-403 posture hides object existence from untrusted readers; it does not apply to an operator's
//!   own shared secret, where an honest, debuggable auth failure is worth more than uniformity (an
//!   UNCONFIGURED admin route still answers 404 — invisible, never an oracle);
//! - `500` — `AuthorityError::{Integrity, Internal}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use plane_store::AuthorityError;
use topos_types::{
    ActionCode, Affected, JsonEnvelope, NextAction, TerminalOutcome, WIRE_SCHEMA_VERSION, WireError,
};

/// A transport/auth/integrity fault on the way in or out of a handler. Maps to a non-2xx status with a
/// uniform envelope body.
#[derive(Debug)]
pub(crate) enum PlaneHttpError {
    /// A malformed request body / DTO / candidate field → 400.
    BadBody(String),
    /// A malformed identifier (workspace / skill / op id, or a hex commit/object id in a body) → 400.
    BadId(String),
    /// A missing/blank read credential → 404 (indistinguishable; never 401/403).
    MissingReadCredential,
    /// A configured admin-token surface with a missing/wrong bearer token → 401 (the one honest auth
    /// failure — an operator debugging their own secret; see the module doc's scoping note).
    Unauthorized,
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
            PlaneHttpError::BadId(m) => (StatusCode::BAD_REQUEST, "BAD_REQUEST", false, vec![], m),
            PlaneHttpError::MissingReadCredential => (
                StatusCode::NOT_FOUND,
                "NOT_FOUND",
                false,
                vec![],
                "not found".to_owned(),
            ),
            PlaneHttpError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                false,
                vec![],
                "missing or invalid admin token".to_owned(),
            ),
            PlaneHttpError::Authority(e) => {
                // The server-side diagnostics the flat body deliberately omits: `plane-store`'s error
                // contract retains the boxed source chain on Integrity/Internal "for server-side
                // diagnostics" — THIS is where that promise is honored. Log the full chain BEFORE
                // flattening (the event fires inside the router's request span, so it correlates with one
                // method/route/status line); the wire body below stays the schema-pinned flat "internal
                // store error" — an internal detail never crosses the wire.
                if matches!(
                    e,
                    AuthorityError::Integrity(_) | AuthorityError::Internal(_)
                ) {
                    tracing::error!(error = %error_chain(&e), "authority fault");
                }
                match e {
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
                }
            }
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
        schema_version: WIRE_SCHEMA_VERSION,
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

/// Flatten an error's full `source()` chain into one `": "`-joined line — the server-side diagnostic the
/// flat wire body deliberately omits. `AuthorityError::{Integrity, Internal}` `Display` a generic line and
/// carry the real fault (the database/store error and everything under it) as a boxed source, so without
/// walking the chain a 500 is undiagnosable. One line (not one event per level) keeps a JSON log entry
/// self-contained and grep-able. Shared with the maintenance scheduler's step logging.
pub(crate) fn error_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut line = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        line.push_str(": ");
        line.push_str(&cause.to_string());
        source = cause.source();
    }
    line
}

#[cfg(test)]
mod tests {
    use super::error_chain;

    #[derive(Debug, thiserror::Error)]
    #[error("outer fault")]
    struct Outer(#[source] Inner);

    #[derive(Debug, thiserror::Error)]
    #[error("inner cause")]
    struct Inner(#[source] std::io::Error);

    /// The chain walk renders EVERY `source()` level, joined on `": "` — the diagnostic line the 500
    /// mapper and the maintenance scheduler log (the wire body never carries it).
    #[test]
    fn error_chain_renders_every_source_level() {
        let e = Outer(Inner(std::io::Error::other("disk on fire")));
        assert_eq!(error_chain(&e), "outer fault: inner cause: disk on fire");
    }
}
