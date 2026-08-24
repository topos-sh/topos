//! [`router`] — the ONE composed surface: `/healthz` + the bearer-gated `/internal/v1` custody
//! lane. Anything else answers the uniform JSON 404 (the vault is internal-network-only with one
//! caller; there is no public face to decorate).

use axum::extract::{DefaultBodyLimit, MatchedPath, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use tracing::Instrument as _;

use crate::routes;
use crate::state::PlaneState;

/// ~160 MiB: the authority's ~100 MiB per-blob reject cap, ×4/3 for base64, plus headroom for a
/// multi-file candidate's JSON framing. The authority still enforces the real per-blob cap (typed,
/// at ingest); this is only the transport belt that rejects an absurd body before it is buffered.
const WRITE_BODY_LIMIT: usize = 160 * 1024 * 1024;

/// ~64 KiB: every non-candidate body (pointer moves, reverts, purges) is tiny.
const SMALL_BODY_LIMIT: usize = 64 * 1024;

/// Build the composed vault router.
pub fn router(state: PlaneState) -> Router {
    // The byte-bearing ingest routes take the large belt; everything else the small one.
    let ingest = Router::new()
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/versions",
            post(routes::internal::commit_version),
        )
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/publish",
            post(routes::internal::publish),
        )
        .layer(DefaultBodyLimit::max(WRITE_BODY_LIMIT));

    let ops = Router::new()
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/pointer",
            post(routes::internal::move_pointer),
        )
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/revert",
            post(routes::internal::revert),
        )
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/current",
            get(routes::internal::read_current),
        )
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/versions/{version_id}",
            get(routes::internal::read_version),
        )
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/versions/{version_id}/purge",
            post(routes::internal::purge_version),
        )
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/objects/{object_id}",
            get(routes::internal::read_object),
        )
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/log",
            get(routes::internal::read_log),
        )
        .route(
            "/internal/v1/workspaces/{ws}/bundles/{bundle}",
            delete(routes::internal::delete_bundle),
        )
        .route(
            "/internal/v1/workspaces/{ws}",
            delete(routes::internal::delete_workspace),
        )
        .route("/internal/v1/storage", get(routes::internal::read_storage))
        .layer(DefaultBodyLimit::max(SMALL_BODY_LIMIT));

    // The ONE bearer gate in front of the whole internal lane: unconfigured ⇒ the uniform 404
    // (the lane is invisible until armed), wrong/missing ⇒ an honest 401 (the caller is the app,
    // debugging its own shared secret — no oracle discipline applies on an internal lane).
    let internal = ingest
        .merge(ops)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            internal_bearer_gate,
        ));

    Router::new()
        .route("/healthz", get(healthz))
        .merge(internal)
        .fallback(uniform_not_found)
        // Outermost (the last layer added), so every response is traced.
        .layer(axum::middleware::from_fn(trace_requests))
        .with_state(state)
}

/// The liveness probe — unauthenticated, constant, no state touched.
async fn healthz() -> &'static str {
    "ok"
}

/// The uniform JSON 404 every unmatched path answers (no path echo, no existence signal).
async fn uniform_not_found() -> Response {
    routes::internal::LaneError::not_found().into_response()
}

/// The internal-lane bearer gate (see [`router`]).
async fn internal_bearer_gate(
    State(state): State<PlaneState>,
    req: Request,
    next: Next,
) -> Response {
    if !state.internal_token_configured() {
        return routes::internal::LaneError::not_found().into_response();
    }
    match bearer_token(req.headers()) {
        Some(token) if state.internal_token_matches(&token) => next.run(req).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "code": "UNAUTHORIZED" })),
        )
            .into_response(),
    }
}

/// Extract `Authorization: Bearer <token>` (case-insensitive scheme; `None` on anything else).
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_owned())
}

/// The route the container healthcheck polls. It is a constant `200 ok` that touches no state, and
/// a compose/Coolify probe hits it every few seconds — at INFO it drowns the log it shares with the
/// real custody traffic (a production sample: 171 of 243 lines).
const PROBE_ROUTE: &str = "/healthz";

/// Which level one served request's access-log line takes: the liveness probe is DEBUG (turn the
/// filter up to see it), every other route — the whole custody lane, and the `(unmatched)` miss —
/// stays INFO.
fn access_log_level(route: &str) -> tracing::Level {
    if route == PROBE_ROUTE {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    }
}

/// Request-level tracing: ONE `request` span per request carrying the method + the matched ROUTE
/// TEMPLATE, and one completion event recording the status + latency at the route's own
/// [`access_log_level`]. Handlers — and the error mapper's `tracing::error!` fault chains — run
/// inside the span, so a 500's server-side diagnostics correlate with exactly one request line in
/// the JSON logs.
///
/// The span records the route TEMPLATE, never the raw path (the bearer rides only the never-logged
/// Authorization header, but templates keep the logs free of any caller-composed string). A request
/// that matched no route logs the constant `(unmatched)`.
async fn trace_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map_or("(unmatched)", MatchedPath::as_str)
        .to_owned();
    let span = tracing::info_span!("request", %method, %route);
    let started = std::time::Instant::now();
    let response = next.run(req).instrument(span.clone()).await;
    let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let status = response.status().as_u16();
    span.in_scope(|| match access_log_level(&route) {
        tracing::Level::DEBUG => tracing::debug!(status, latency_ms, "request served"),
        _ => tracing::info!(status, latency_ms, "request served"),
    });
    response
}

#[cfg(test)]
mod tests {
    use super::{PROBE_ROUTE, access_log_level};

    #[test]
    fn the_liveness_probe_logs_at_debug_and_every_real_route_at_info() {
        assert_eq!(access_log_level(PROBE_ROUTE), tracing::Level::DEBUG);
        for route in [
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/publish",
            "/internal/v1/workspaces/{ws}/bundles/{bundle}/log",
            "/internal/v1/storage",
            "(unmatched)",
        ] {
            assert_eq!(
                access_log_level(route),
                tracing::Level::INFO,
                "{route} is real traffic"
            );
        }
    }
}
